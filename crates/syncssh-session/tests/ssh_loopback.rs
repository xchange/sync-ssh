//! A real, isolated SSH server verifies protocol behavior without using the
//! user's hosts, keys, accounts, or external network.
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{self, Server as _, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId, Pty};
use std::io::{Read, Write};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use syncssh_session::{connect_ssh, HostKey, KnownHosts, SessionError, SshAuth, SshConfig};

#[derive(Clone)]
struct EchoServer {
    attempts: Arc<AtomicUsize>,
    user_key: PublicKey,
    resize: std::sync::mpsc::Sender<(u32, u32)>,
}
impl server::Server for EchoServer {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        self.clone()
    }
}
impl server::Handler for EchoServer {
    type Error = russh::Error;
    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Ok(if user == "tester" && password == "test-only-password" {
            server::Auth::Accept
        } else {
            server::Auth::reject()
        })
    }
    async fn auth_publickey(
        &mut self,
        user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<server::Auth, Self::Error> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Ok(if user == "tester" && key.public_key == &self.user_key {
            server::Auth::Accept
        } else {
            server::Auth::reject()
        })
    }
    async fn channel_open_session(
        &mut self,
        _: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        assert_eq!(term, "xterm-256color");
        session.channel_success(channel)?;
        Ok(())
    }
    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        session.data(channel, b"SYNCSSH READY\n".to_vec())?;
        Ok(())
    }
    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.data(channel, data.to_vec())?;
        Ok(())
    }
    async fn window_change_request(
        &mut self,
        _: ChannelId,
        cols: u32,
        rows: u32,
        _: u32,
        _: u32,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        let _ = self.resize.send((cols, rows));
        Ok(())
    }
}
struct Shutdown(flume::Sender<()>);
impl Drop for Shutdown {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

#[test]
fn real_ssh_verifies_key_before_auth_then_supports_password_key_and_resize() {
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let user_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let (resize_tx, resize_rx) = std::sync::mpsc::channel();
    let mut server = EchoServer {
        attempts: attempts.clone(),
        user_key: user_key.public_key().clone(),
        resize: resize_tx,
    };
    let (stop_tx, stop_rx) = flume::bounded(1);
    let _shutdown = Shutdown(stop_tx);
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    let public = host_key.public_key().to_openssh().unwrap();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let socket=tokio::net::TcpListener::bind(("127.0.0.1",0)).await.unwrap();
            port_tx.send(socket.local_addr().unwrap().port()).unwrap();
            let config=Arc::new(server::Config {
                keys:vec![host_key],auth_rejection_time:Duration::ZERO,auth_rejection_time_initial:Some(Duration::ZERO),
                ..Default::default()
            });
            tokio::select! { _=server.run_on_socket(config,&socket)=>{}, _=stop_rx.recv_async()=>{} }
        });
    });
    let port = port_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let config = SshConfig {
        host: "127.0.0.1".into(),
        port,
        username: "tester".into(),
        auth: SshAuth::Password("test-only-password".into()),
    };
    let key = HostKey::from_openssh("127.0.0.1", port, &public).unwrap();
    let unknown = connect_ssh(config.clone(), KnownHosts::memory(), 80, 24)
        .err()
        .unwrap();
    match unknown {
        SessionError::UnknownHostKey(challenge) => assert_eq!(challenge, key),
        other => panic!("{other}"),
    }
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        0,
        "must not send a password before server key approval"
    );

    let different = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let mut mismatched = KnownHosts::memory();
    mismatched
        .trust(
            HostKey::from_openssh(
                "127.0.0.1",
                port,
                &different.public_key().to_openssh().unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        connect_ssh(config.clone(), mismatched, 80, 24),
        Err(SessionError::HostKeyChanged { .. })
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 0);

    let mut trusted = KnownHosts::memory();
    trusted.trust(key).unwrap();
    let mut bad_password = config.clone();
    bad_password.auth = SshAuth::Password("incorrect".into());
    assert!(matches!(
        connect_ssh(bad_password, trusted.clone(), 80, 24),
        Err(SessionError::Authentication)
    ));

    let mut terminal = connect_ssh(config.clone(), trusted.clone(), 80, 24).unwrap();
    let data = b"\x1b[A UTF-8 \xe4\xb8\xad\xe6\x96\x87\r\n";
    terminal.writer.write_all(data).unwrap();
    (terminal.resize)(111, 37);
    let (read_tx, read_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut expected = b"SYNCSSH READY\n".to_vec();
        expected.extend_from_slice(data);
        let mut bytes = vec![0; expected.len()];
        terminal.reader.read_exact(&mut bytes).unwrap();
        read_tx.send((bytes, expected)).unwrap();
    });
    let (received, expected) = read_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(received, expected);
    assert_eq!(
        resize_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        (111, 37)
    );
    drop(terminal.guard);

    let mut key_config = config;
    key_config.auth = SshAuth::PrivateKey {
        pem: user_key
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .unwrap()
            .to_string(),
        passphrase: None,
        certificate: None,
    };
    let terminal = connect_ssh(key_config, trusted, 80, 24).unwrap();
    drop(terminal);
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}
