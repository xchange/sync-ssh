use crate::{
    dimensions, HostKey, KnownHosts, SessionError, SessionGuard, SessionMonitor, SessionStatus,
    TerminalIo,
};
use flume::{Receiver, Sender};
use russh::client;
use russh::keys::{decode_secret_key, Certificate, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{Channel, ChannelMsg, Disconnect};
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const CHUNK_SIZE: usize = 16 * 1024;
const QUEUE_LENGTH: usize = 128;

/// Authentication material is erased when the configuration is dropped.
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub enum SshAuth {
    Password(String),
    PrivateKey {
        pem: String,
        passphrase: Option<String>,
        certificate: Option<String>,
    },
    Agent,
}

impl std::fmt::Debug for SshAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Password(_) => "Password([REDACTED])",
            Self::PrivateKey { .. } => "PrivateKey([REDACTED])",
            Self::Agent => "Agent",
        })
    }
}

/// A direct SSH connection with independently verified host identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
}

impl SshConfig {
    fn validate(&self) -> Result<(), SessionError> {
        validate_endpoint(&self.host, self.port, &self.username, &self.auth)
    }
}

fn validate_endpoint(
    host: &str,
    port: u16,
    username: &str,
    auth: &SshAuth,
) -> Result<(), SessionError> {
    if host.is_empty()
        || host.len() > 253
        || host.chars().any(|c| c.is_control() || c.is_whitespace())
        || port == 0
    {
        return Err(SessionError::InvalidConfig(
            "host or port is invalid".into(),
        ));
    }
    if username.is_empty() || username.len() > 256 || username.chars().any(char::is_control) {
        return Err(SessionError::InvalidConfig("username is invalid".into()));
    }
    match auth {
        SshAuth::Password(value) if value.len() > 64 * 1024 => {
            Err(SessionError::InvalidConfig("password is too large".into()))
        }
        SshAuth::PrivateKey {
            pem,
            passphrase,
            certificate,
        } if pem.len() > 1024 * 1024
            || passphrase.as_ref().is_some_and(|p| p.len() > 64 * 1024)
            || certificate.as_ref().is_some_and(|c| c.len() > 1024 * 1024) =>
        {
            Err(SessionError::InvalidConfig(
                "key material is too large".into(),
            ))
        }
        _ => Ok(()),
    }
}

/// Establishes SSH on a dedicated I/O thread. The caller waits up to twenty
/// seconds per host for DNS, handshake, verified host key, authentication and PTY setup.
/// Call this function on a background worker, never on GPUI's foreground thread.
pub fn connect_ssh(
    config: SshConfig,
    known_hosts: KnownHosts,
    cols: u16,
    rows: u16,
) -> Result<TerminalIo, SessionError> {
    config.validate()?;
    let connection_timeout = CONNECT_TIMEOUT;
    let (cols, rows) = dimensions(cols, rows);
    let (input_tx, input_rx) = flume::bounded(QUEUE_LENGTH);
    let (output_tx, output_rx) = flume::bounded(QUEUE_LENGTH);
    let (stop_tx, stop_rx) = flume::bounded(1);
    let (resize_tx, resize_rx) = tokio::sync::watch::channel((cols, rows));
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (sftp, sftp_requests, sftp_closed) = crate::sftp::channel();
    let worker_closed = sftp_closed.clone();
    let (tunnels, forwarded, tunnel_service) = crate::tunnel::channel();
    let worker_tunnels = tunnels.clone();
    let status = SessionMonitor::connected();
    let worker_status = status.clone();
    let disconnect_reason = Arc::new(Mutex::new(None::<String>));
    std::thread::Builder::new()
        .name("syncssh-ssh".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready_tx.send(Err(SessionError::Io(error)));
                    return;
                }
            };
            runtime.block_on(async move {
                let connected = tokio::time::timeout(
                    connection_timeout,
                    establish(
                        config,
                        known_hosts,
                        cols,
                        rows,
                        forwarded,
                        disconnect_reason.clone(),
                    ),
                )
                .await;
                let (session, channel) = match connected {
                    Ok(Ok(value)) => value,
                    Ok(Err(error)) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                    Err(_) => {
                        let _ = ready_tx.send(Err(SessionError::Timeout));
                        return;
                    }
                };
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                let session = Arc::new(session);
                let (fatal_tx, fatal_rx) = flume::bounded(1);
                let mut tunnel_worker = tokio::spawn(tunnel_service.run(session.clone(), fatal_tx));
                let mut sftp_worker = tokio::spawn(crate::sftp::serve(
                    session.clone(),
                    sftp_requests,
                    worker_closed.clone(),
                ));
                let terminal = drive(channel, input_rx, output_tx, stop_rx, resize_rx);
                tokio::pin!(terminal);
                let ended = tokio::select! {
                    biased;
                    ended = &mut terminal => ended,
                    reason = fatal_rx.recv_async() => match reason {
                        Ok(reason) => SessionStatus::Disconnected { reason },
                        // The service also ends when the SSH transport closes.
                        // Let the channel distinguish an orderly shell EOF from
                        // network failure instead of racing its queued messages.
                        Err(_) => terminal.await,
                    },
                };
                let ended = match ended {
                    SessionStatus::Disconnected { reason } => SessionStatus::Disconnected {
                        reason: disconnect_reason
                            .lock()
                            .ok()
                            .and_then(|mut value| value.take())
                            .unwrap_or(reason),
                    },
                    other => other,
                };
                worker_status.finish(ended);
                worker_tunnels.close();
                worker_closed.store(true, std::sync::atomic::Ordering::Release);
                if tokio::time::timeout(Duration::from_secs(2), &mut tunnel_worker)
                    .await
                    .is_err()
                {
                    tunnel_worker.abort();
                }
                // Give transfers a brief chance to clean partial files before disconnecting.
                if tokio::time::timeout(Duration::from_secs(3), &mut sftp_worker)
                    .await
                    .is_err()
                {
                    sftp_worker.abort();
                }
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    session.disconnect(Disconnect::ByApplication, "Terminal closed", "en"),
                )
                .await;
            });
        })?;
    // Bound the caller too: a malfunctioning runtime must not stall the app.
    match ready_rx.recv_timeout(connection_timeout + Duration::from_secs(1)) {
        Ok(result) => result?,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            let _ = stop_tx.try_send(());
            return Err(SessionError::Timeout);
        }
        Err(_) => return Err(SessionError::Transport("connection worker stopped".into())),
    }
    Ok(TerminalIo {
        sftp: Some(sftp),
        tunnels: Some(tunnels.clone()),
        status: Some(status.clone()),
        reader: Box::new(ChannelReader {
            receiver: output_rx,
            current: Vec::new(),
            offset: 0,
        }),
        writer: Box::new(ChannelWriter(input_tx)),
        resize: Arc::new(move |cols, rows| {
            let _ = resize_tx.send(dimensions(cols, rows));
        }),
        guard: SessionGuard::new(move || {
            status.finish(SessionStatus::Closed);
            tunnels.close();
            sftp_closed.store(true, std::sync::atomic::Ordering::Release);
            let _ = stop_tx.try_send(());
        }),
    })
}

struct VerifiedClient {
    host: String,
    port: u16,
    known_hosts: KnownHosts,
    verification_error: Arc<Mutex<Option<SessionError>>>,
    forwarded: Option<Sender<crate::tunnel::Forwarded>>,
    disconnect_reason: Arc<Mutex<Option<String>>>,
}

impl client::Handler for VerifiedClient {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // We pin the actual host public key, including when a server presents a
        // certificate. We do not silently treat any CA as trusted.
        let result = HostKey::from_public_key(&self.host, self.port, &key.public_key())
            .and_then(|key| self.known_hosts.verify(&key));
        match result {
            Ok(()) => Ok(true),
            Err(error) => {
                if let Ok(mut slot) = self.verification_error.lock() {
                    *slot = Some(error);
                }
                Ok(false)
            }
        }
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<client::Msg>,
        _: &str,
        connected_port: u32,
        _: &str,
        _: u32,
        reply: client::ChannelOpenHandle,
        _: &mut client::Session,
    ) -> Result<(), Self::Error> {
        // Keep the SSH event loop free to carry terminal data while the tunnel
        // worker resolves and connects the approved local destination.
        if let Some(forwarded) = &self.forwarded {
            let _ = forwarded.try_send(crate::tunnel::Forwarded {
                channel,
                connected_port,
                reply,
            });
        }
        Ok(())
    }

    async fn disconnected(
        &mut self,
        reason: client::DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        let message = match &reason {
            client::DisconnectReason::ReceivedDisconnect(info) => format!(
                "SSH 服务器断开连接：{:?} {}",
                info.reason_code, info.message
            ),
            client::DisconnectReason::Error(error) => error.to_string(),
        };
        if let Ok(mut value) = self.disconnect_reason.lock() {
            *value = Some(message);
        }
        match reason {
            client::DisconnectReason::Error(error) => Err(error),
            _ => Ok(()),
        }
    }
}

fn transport(error: impl std::fmt::Display) -> SessionError {
    SessionError::Transport(error.to_string())
}

type Established = (client::Handle<VerifiedClient>, Channel<client::Msg>);

async fn establish(
    config: SshConfig,
    known_hosts: KnownHosts,
    cols: u16,
    rows: u16,
    forwarded: Sender<crate::tunnel::Forwarded>,
    disconnect_reason: Arc<Mutex<Option<String>>>,
) -> Result<Established, SessionError> {
    let session =
        connect_verified(&config, known_hosts, Some(forwarded), disconnect_reason).await?;
    let mut channel = session.channel_open_session().await.map_err(transport)?;
    channel
        .request_pty(true, "xterm-256color", cols.into(), rows.into(), 0, 0, &[])
        .await
        .map_err(transport)?;
    expect_success(&mut channel, "server denied PTY allocation").await?;
    channel.request_shell(true).await.map_err(transport)?;
    expect_success(&mut channel, "server denied interactive shell").await?;
    Ok((session, channel))
}

async fn connect_verified(
    endpoint: &SshConfig,
    known_hosts: KnownHosts,
    forwarded: Option<Sender<crate::tunnel::Forwarded>>,
    disconnect_reason: Arc<Mutex<Option<String>>>,
) -> Result<client::Handle<VerifiedClient>, SessionError> {
    let verification_error = Arc::new(Mutex::new(None));
    let handler = VerifiedClient {
        host: endpoint.host.clone(),
        port: endpoint.port,
        known_hosts,
        verification_error: verification_error.clone(),
        forwarded,
        disconnect_reason,
    };
    let client_config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        window_size: 1024 * 1024,
        maximum_packet_size: 32768,
        channel_buffer_size: 64,
        nodelay: true,
        ..Default::default()
    });
    let connection = client::connect(
        client_config,
        (endpoint.host.as_str(), endpoint.port),
        handler,
    )
    .await;
    let mut session = match connection {
        Ok(session) => session,
        Err(error) => {
            if let Ok(mut slot) = verification_error.lock() {
                if let Some(error) = slot.take() {
                    return Err(error);
                }
            }
            return Err(transport(error));
        }
    };
    let authenticated = match &endpoint.auth {
        SshAuth::Password(password) => session
            .authenticate_password(&endpoint.username, password.as_str())
            .await
            .map_err(transport)?
            .success(),
        SshAuth::PrivateKey {
            pem,
            passphrase,
            certificate,
        } => {
            let key = decode_secret_key(pem, passphrase.as_deref()).map_err(|_| {
                SessionError::InvalidConfig("cannot decrypt or parse private key".into())
            })?;
            if let Some(certificate) = certificate
                .as_deref()
                .filter(|text| !text.trim().is_empty())
            {
                let certificate = Certificate::from_openssh(certificate).map_err(|_| {
                    SessionError::InvalidConfig("cannot parse OpenSSH certificate".into())
                })?;
                if certificate.public_key() != key.public_key().key_data() {
                    return Err(SessionError::InvalidConfig(
                        "certificate does not match private key".into(),
                    ));
                }
                session
                    .authenticate_openssh_cert(&endpoint.username, Arc::new(key), certificate)
                    .await
                    .map_err(transport)?
                    .success()
            } else {
                let hash = session
                    .best_supported_rsa_hash()
                    .await
                    .map_err(transport)?
                    .flatten();
                session
                    .authenticate_publickey(
                        &endpoint.username,
                        PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                    )
                    .await
                    .map_err(transport)?
                    .success()
            }
        }
        SshAuth::Agent => authenticate_agent(&mut session, &endpoint.username).await?,
    };
    if !authenticated {
        return Err(SessionError::Authentication);
    }
    Ok(session)
}

async fn expect_success(
    channel: &mut Channel<client::Msg>,
    error: &str,
) -> Result<(), SessionError> {
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Success) => return Ok(()),
            Some(ChannelMsg::Failure) | Some(ChannelMsg::Close) | None => {
                return Err(SessionError::Transport(error.into()))
            }
            // Well-behaved SSH servers send the request response before shell
            // data. Do not consume any user output silently if they don't.
            Some(ChannelMsg::Data { .. } | ChannelMsg::ExtendedData { .. }) => {
                return Err(SessionError::Transport(
                    "server sent data before shell acceptance".into(),
                ))
            }
            _ => {}
        }
    }
}

#[cfg(unix)]
async fn authenticate_agent(
    session: &mut client::Handle<VerifiedClient>,
    username: &str,
) -> Result<bool, SessionError> {
    use russh::keys::agent::{client::AgentClient, AgentIdentity};
    let mut agent = AgentClient::connect_env()
        .await
        .map_err(|_| SessionError::InvalidConfig("SSH_AUTH_SOCK agent is unavailable".into()))?;
    if let Some(binding) = session.agent_session_binding() {
        // Unsupported agent extensions are expected on older agents; requests
        // still require agent-held keys and cannot disclose their private data.
        let _ = agent.bind_session(binding, false).await;
    }
    let identities = agent.request_identities().await.map_err(transport)?;
    let hash = session
        .best_supported_rsa_hash()
        .await
        .map_err(transport)?
        .flatten();
    for identity in identities.into_iter().take(16) {
        let result = match identity {
            AgentIdentity::PublicKey { key, .. } => {
                session
                    .authenticate_publickey_with(username, key, hash, &mut agent)
                    .await
            }
            AgentIdentity::Certificate { certificate, .. } => {
                session
                    .authenticate_certificate_with(username, certificate, hash, &mut agent)
                    .await
            }
        };
        if result.map_err(transport)?.success() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(not(unix))]
async fn authenticate_agent(
    _: &mut client::Handle<VerifiedClient>,
    _: &str,
) -> Result<bool, SessionError> {
    Err(SessionError::InvalidConfig("SSH agent authentication is currently available on macOS/Linux; choose a password or private key on Windows".into()))
}

async fn drive(
    channel: Channel<client::Msg>,
    input: Receiver<Zeroizing<Vec<u8>>>,
    output: Sender<Vec<u8>>,
    stop: Receiver<()>,
    mut resize: tokio::sync::watch::Receiver<(u16, u16)>,
) -> SessionStatus {
    let (mut read, write) = channel.split();
    // EOF/Close packets are normal shell termination; disappearance of the
    // transport without either packet is a disconnect, eligible for reconnect.
    let output_loop = async {
        let mut exit_code = None;
        loop {
            match read.wait().await {
                Some(ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. }) => {
                    for chunk in data.chunks(CHUNK_SIZE) {
                        if output.send_async(chunk.to_vec()).await.is_err() {
                            return Ok::<_, SessionError>(SessionStatus::Closed);
                        }
                    }
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => exit_code = Some(exit_status),
                Some(ChannelMsg::Eof | ChannelMsg::Close) => {
                    return Ok(SessionStatus::Exited { exit_code })
                }
                None => {
                    return Ok(SessionStatus::Disconnected {
                        reason: "SSH 连接意外中断".into(),
                    })
                }
                _ => {}
            }
        }
    };
    let input_loop = async {
        loop {
            tokio::select! {
                input = input.recv_async() => match input {
                    Ok(bytes) => write.data(&bytes[..]).await.map_err(transport)?,
                    Err(_) => return Ok::<_, SessionError>(SessionStatus::Closed),
                },
                changed = resize.changed() => {
                    if changed.is_err() { return Ok(SessionStatus::Closed); }
                    let (cols, rows) = *resize.borrow_and_update();
                    write.window_change(cols.into(), rows.into(), 0, 0).await.map_err(transport)?;
                }
            }
        }
    };
    let result = tokio::select! {
        biased;
        _ = stop.recv_async() => Ok(SessionStatus::Closed),
        result = output_loop => result,
        result = input_loop => result,
    };
    result.unwrap_or_else(|error| SessionStatus::Disconnected {
        reason: error.to_string(),
    })
}

struct ChannelReader {
    receiver: Receiver<Vec<u8>>,
    current: Vec<u8>,
    offset: usize,
}
impl Read for ChannelReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        while self.offset == self.current.len() {
            match self.receiver.recv() {
                Ok(data) => {
                    self.current = data;
                    self.offset = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let count = buffer.len().min(self.current.len() - self.offset);
        buffer[..count].copy_from_slice(&self.current[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}
struct ChannelWriter(Sender<Zeroizing<Vec<u8>>>);
impl Write for ChannelWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let count = bytes.len().min(CHUNK_SIZE);
        // Never block the GUI while a remote process is stalled. Bounded queue
        // pressure is explicit to the caller, allowing a paste error to surface.
        self.0
            .try_send(Zeroizing::new(bytes[..count].to_vec()))
            .map_err(|error| match error {
                flume::TrySendError::Full(_) => {
                    io::Error::new(io::ErrorKind::WouldBlock, "SSH input queue is full")
                }
                flume::TrySendError::Disconnected(_) => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "SSH session disconnected")
                }
            })?;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_channel_preserves_bytes_and_reports_backpressure() {
        let (tx, rx) = flume::bounded(1);
        let mut writer = ChannelWriter(tx);
        assert_eq!(writer.write(b"\x1b[A\xe4\xb8\xad").unwrap(), 6);
        assert_eq!(
            writer.write(b"full").unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(&**rx.recv().unwrap(), b"\x1b[A\xe4\xb8\xad");
        drop(rx);
        assert_eq!(
            writer.write(b"closed").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn output_reader_handles_chunk_boundaries_and_eof() {
        let (tx, rx) = flume::bounded(4);
        tx.send(Vec::new()).unwrap();
        tx.send(b"abc".to_vec()).unwrap();
        tx.send(b"def".to_vec()).unwrap();
        drop(tx);
        let mut reader = ChannelReader {
            receiver: rx,
            current: Vec::new(),
            offset: 0,
        };
        let mut small = [0; 2];
        assert_eq!(reader.read(&mut small).unwrap(), 2);
        assert_eq!(&small, b"ab");
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"cdef");
    }

    #[test]
    fn credentials_never_appear_in_debug_output() {
        assert!(!format!("{:?}", SshAuth::Password("secret-token".into())).contains("secret-token"));
        assert!(!format!(
            "{:?}",
            SshAuth::PrivateKey {
                pem: "private-key".into(),
                passphrase: None,
                certificate: None
            }
        )
        .contains("private-key"));
    }
}
