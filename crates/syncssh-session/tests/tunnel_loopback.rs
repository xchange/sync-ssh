//! Isolated, real SSH and TCP sockets exercise forwarding without contacting
//! external hosts or reading the user's SSH configuration/credentials.
use russh::{
    keys::{Algorithm, PrivateKey},
    server::{self, Server as _, Session},
    Channel, ChannelId, Disconnect, Pty,
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{Shutdown, TcpStream},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use syncssh_session::{
    connect_ssh, HostKey, KnownHosts, SessionStatus, SshAuth, SshConfig, TerminalIo, TunnelError,
    TunnelHandle, TunnelKind, TunnelSpec, TunnelState,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    task::JoinHandle,
};

struct ForwardServer {
    auth_count: Arc<AtomicUsize>,
    deny_forwarding: bool,
    listeners: BTreeMap<(String, u32), JoinHandle<()>>,
    connections: Vec<JoinHandle<()>>,
    shell: Option<ChannelId>,
}
impl server::Server for ForwardServer {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        Self {
            auth_count: self.auth_count.clone(),
            deny_forwarding: self.deny_forwarding,
            listeners: BTreeMap::new(),
            connections: Vec::new(),
            shell: None,
        }
    }
}
impl Drop for ForwardServer {
    fn drop(&mut self) {
        for task in self.listeners.values().chain(self.connections.iter()) {
            task.abort();
        }
    }
}
impl server::Handler for ForwardServer {
    type Error = russh::Error;
    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        self.auth_count.fetch_add(1, Ordering::SeqCst);
        Ok(if user == "tunnel-test" && password == "test-only" {
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
        _: &str,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }
    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.shell = Some(channel);
        session.channel_success(channel)
    }
    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.shell != Some(channel) {
            return Ok(());
        }
        match data {
            b"exit-with-status" => {
                session.exit_status_request(channel, 7)?;
                session.eof(channel)?;
                session.close(channel)
            }
            b"exit-without-status" => {
                session.eof(channel)?;
                session.close(channel)
            }
            b"network-disconnect" => {
                session.disconnect(Disconnect::ByApplication, "test network interruption", "en")
            }
            _ => session.data(channel, data.to_vec()),
        }
    }
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<server::Msg>,
        host: &str,
        port: u32,
        _: &str,
        _: u32,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.deny_forwarding {
            return Ok(());
        }
        let Ok(mut target) = tokio::net::TcpStream::connect((host, port as u16)).await else {
            return Ok(());
        };
        reply.accept().await;
        self.connections.push(tokio::spawn(async move {
            let mut stream = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut target).await;
        }));
        Ok(())
    }
    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        if self.deny_forwarding {
            return Ok(false);
        }
        let Ok(listener) = tokio::net::TcpListener::bind((address, *port as u16)).await else {
            return Ok(false);
        };
        *port = listener.local_addr().unwrap().port() as u32;
        let address = address.to_owned();
        let bound_port = *port;
        let connection = session.handle();
        self.listeners.insert((address.clone(), bound_port), tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((mut socket, peer)) = accepted else { break; };
                        let connection = connection.clone();
                        let address = address.clone();
                        clients.spawn(async move {
                            if let Ok(channel) = connection.channel_open_forwarded_tcpip(address, bound_port, peer.ip().to_string(), peer.port() as u32).await {
                                let mut stream = channel.into_stream();
                                let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                            }
                        });
                    }
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        }));
        Ok(true)
    }
    async fn cancel_tcpip_forward(
        &mut self,
        address: &str,
        port: u32,
        _: &mut Session,
    ) -> Result<bool, Self::Error> {
        if let Some(task) = self.listeners.remove(&(address.to_owned(), port)) {
            task.abort();
            let _ = task.await;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

struct Fixture {
    config: SshConfig,
    trusted: KnownHosts,
    auth_count: Arc<AtomicUsize>,
    target_port: u16,
    stop: flume::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn new(deny_forwarding: bool) -> Self {
        let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let public = host_key.public_key().to_openssh().unwrap();
        let auth_count = Arc::new(AtomicUsize::new(0));
        let mut server = ForwardServer {
            auth_count: auth_count.clone(),
            deny_forwarding,
            listeners: BTreeMap::new(),
            connections: Vec::new(),
            shell: None,
        };
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stop, stopped) = flume::bounded(1);
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let ssh = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
                let target = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
                ready_tx.send((ssh.local_addr().unwrap().port(), target.local_addr().unwrap().port())).unwrap();
                let target_task = tokio::spawn(async move {
                    loop {
                        let (mut socket, _) = target.accept().await.unwrap();
                        tokio::spawn(async move {
                            // Deliberately reply only after EOF: losing SSH/TCP
                            // half-close would hang this test or lose the reply.
                            let mut bytes = Vec::new();
                            if socket.read_to_end(&mut bytes).await.is_ok() {
                                let _ = socket.write_all(b"after EOF:").await;
                                let _ = socket.write_all(&bytes).await;
                                let _ = socket.shutdown().await;
                            }
                        });
                    }
                });
                let config = Arc::new(server::Config { keys: vec![host_key], auth_rejection_time: Duration::ZERO, auth_rejection_time_initial: Some(Duration::ZERO), ..Default::default() });
                tokio::select! { _ = server.run_on_socket(config, &ssh) => {}, _ = stopped.recv_async() => {} }
                target_task.abort();
            });
        });
        let (port, target_port) = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut trusted = KnownHosts::memory();
        trusted
            .trust(HostKey::from_openssh("127.0.0.1", port, &public).unwrap())
            .unwrap();
        Self {
            config: SshConfig {
                host: "127.0.0.1".into(),
                port,
                username: "tunnel-test".into(),
                auth: SshAuth::Password("test-only".into()),
            },
            trusted,
            auth_count,
            target_port,
            stop,
            thread: Some(thread),
        }
    }
    fn connect(&self) -> TerminalIo {
        connect_ssh(self.config.clone(), self.trusted.clone(), 80, 24).unwrap()
    }
    fn spec(&self, kind: TunnelKind) -> TunnelSpec {
        TunnelSpec {
            kind,
            target_port: self.target_port,
            ..Default::default()
        }
    }
    fn shutdown(&mut self) {
        let _ = self.stop.try_send(());
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn wait_until(description: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(6);
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out: {description}");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn socket(port: u16) -> TcpStream {
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
}
fn exchange(mut stream: TcpStream) {
    // Exceed the default SSH channel window to exercise backpressure and
    // window replenishment, not just the initial buffered burst.
    let payload: Vec<_> = (0..3 * 1024 * 1024 + 17).map(|i| (i % 251) as u8).collect();
    stream.write_all(&payload).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut received = Vec::new();
    stream.read_to_end(&mut received).unwrap();
    let mut expected = b"after EOF:".to_vec();
    expected.extend_from_slice(&payload);
    assert_eq!(received.len(), expected.len());
    assert!(received == expected, "forwarded bytes differ");
}
fn socks(port: u16, target: u16, domain: bool) -> TcpStream {
    let mut stream = socket(port);
    stream.write_all(&[5, 2, 2, 0]).unwrap();
    let mut method = [0; 2];
    stream.read_exact(&mut method).unwrap();
    assert_eq!(method, [5, 0]);
    let mut request = if domain {
        b"\x05\x01\x00\x03\x09localhost".to_vec()
    } else {
        vec![5, 1, 0, 1, 127, 0, 0, 1]
    };
    request.extend_from_slice(&target.to_be_bytes());
    // Splitting the request ensures parsing is stream-based.
    for byte in request {
        stream.write_all(&[byte]).unwrap();
    }
    let mut reply = [0; 10];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(reply[0..2], [5, 0]);
    stream
}
fn stopped(handle: &TunnelHandle, id: u64) -> bool {
    handle
        .snapshot()
        .iter()
        .any(|status| status.id == id && status.state == TunnelState::Stopped)
}

fn assert_stream_closed(stream: &mut TcpStream) {
    let mut byte = [0];
    match stream.read(&mut byte) {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            ) => {}
        other => panic!("stream did not close promptly: {other:?}"),
    }
}

#[test]
fn all_three_tunnel_kinds_share_authentication_and_preserve_half_close() {
    let fixture = Fixture::new(false);
    let mut terminal = fixture.connect();
    let tunnels = terminal.tunnels.as_ref().unwrap();
    let local = tunnels.start(fixture.spec(TunnelKind::Local)).unwrap();
    let remote = tunnels.start(fixture.spec(TunnelKind::Remote)).unwrap();
    let dynamic = tunnels.start(fixture.spec(TunnelKind::Dynamic)).unwrap();
    exchange(socket(local.bound_port));
    exchange(socket(remote.bound_port));
    exchange(socks(dynamic.bound_port, fixture.target_port, false));
    exchange(socks(dynamic.bound_port, fixture.target_port, true));
    assert_eq!(fixture.auth_count.load(Ordering::SeqCst), 1);
    terminal.writer.write_all(b"shell still alive").unwrap();
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = [0; 17];
        terminal.reader.read_exact(&mut bytes).unwrap();
        sent.send(bytes).unwrap();
    });
    assert_eq!(
        &received.recv_timeout(Duration::from_secs(5)).unwrap(),
        b"shell still alive"
    );
    for status in [local, remote, dynamic] {
        tunnels.stop(status.id).unwrap();
        assert!(stopped(tunnels, status.id));
        assert!(TcpStream::connect(("127.0.0.1", status.bound_port)).is_err());
    }
}

#[test]
fn occupied_ports_and_server_refusal_are_reported_without_closing_shell() {
    let fixture = Fixture::new(true);
    let terminal = fixture.connect();
    let tunnels = terminal.tunnels.as_ref().unwrap();
    assert!(matches!(
        tunnels.start(fixture.spec(TunnelKind::Remote)),
        Err(TunnelError::Transport(_))
    ));
    let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let mut spec = fixture.spec(TunnelKind::Local);
    spec.listen_port = occupied.local_addr().unwrap().port();
    assert!(matches!(
        tunnels.start(spec),
        Err(TunnelError::Transport(_))
    ));
    let dynamic = tunnels.start(fixture.spec(TunnelKind::Dynamic)).unwrap();
    let mut stream = socket(dynamic.bound_port);
    stream.write_all(&[5, 1, 0]).unwrap();
    let mut method = [0; 2];
    stream.read_exact(&mut method).unwrap();
    stream
        .write_all(&[5, 1, 0, 1, 127, 0, 0, 1, 0, 80])
        .unwrap();
    let mut rejected = [0; 10];
    stream.read_exact(&mut rejected).unwrap();
    assert_eq!(rejected[1], 5);
    wait_until("forwarding rejection surfaced", || {
        tunnels
            .snapshot()
            .iter()
            .any(|s| s.id == dynamic.id && s.last_error.is_some())
    });
    assert_eq!(
        terminal.status.as_ref().unwrap().state(),
        SessionStatus::Connected
    );
    tunnels.stop(dynamic.id).unwrap();
}

#[test]
fn stopping_rule_and_closing_tab_release_listeners_and_active_streams() {
    let fixture = Fixture::new(false);
    let terminal = fixture.connect();
    let tunnels = terminal.tunnels.as_ref().unwrap().clone();
    let monitor = terminal.status.as_ref().unwrap().clone();
    let local = tunnels.start(fixture.spec(TunnelKind::Local)).unwrap();
    let remote = tunnels.start(fixture.spec(TunnelKind::Remote)).unwrap();
    let mut first = socket(local.bound_port);
    let mut second = socket(remote.bound_port);
    wait_until("both active connections", || {
        tunnels
            .snapshot()
            .iter()
            .filter(|s| s.active_connections == 1)
            .count()
            == 2
    });
    tunnels.stop(local.id).unwrap();
    assert_stream_closed(&mut first);
    assert!(tunnels
        .snapshot()
        .iter()
        .find(|s| s.id == local.id)
        .unwrap()
        .last_error
        .is_none());
    assert!(TcpStream::connect(("127.0.0.1", local.bound_port)).is_err());
    tunnels.stop(remote.id).unwrap();
    assert_stream_closed(&mut second);
    assert!(TcpStream::connect(("127.0.0.1", remote.bound_port)).is_err());
    let remote = tunnels.start(fixture.spec(TunnelKind::Remote)).unwrap();
    let mut second = socket(remote.bound_port);
    wait_until("remote restarted", || {
        tunnels
            .snapshot()
            .iter()
            .any(|s| s.id == remote.id && s.active_connections == 1)
    });
    drop(terminal.guard);
    assert_eq!(monitor.state(), SessionStatus::Closed);
    wait_until("remote listener released", || {
        TcpStream::connect(("127.0.0.1", remote.bound_port)).is_err()
    });
    assert_stream_closed(&mut second);
    wait_until("all rules stopped", || stopped(&tunnels, remote.id));
    assert!(matches!(
        tunnels.start(fixture.spec(TunnelKind::Local)),
        Err(TunnelError::Closed)
    ));
}

#[test]
fn session_status_distinguishes_shell_exit_and_network_disconnect() {
    let mut fixture = Fixture::new(false);
    for (command, code) in [
        (b"exit-with-status".as_slice(), Some(7)),
        (b"exit-without-status".as_slice(), None),
    ] {
        let mut terminal = fixture.connect();
        let monitor = terminal.status.as_ref().unwrap().clone();
        terminal.writer.write_all(command).unwrap();
        wait_until("orderly shell exit", || {
            monitor.state() != SessionStatus::Connected
        });
        assert_eq!(monitor.state(), SessionStatus::Exited { exit_code: code });
        drop(terminal);
        assert_eq!(monitor.state(), SessionStatus::Exited { exit_code: code });
    }
    let mut terminal = fixture.connect();
    let monitor = terminal.status.as_ref().unwrap().clone();
    terminal.writer.write_all(b"network-disconnect").unwrap();
    wait_until("server disconnect reason", || {
        monitor.state() != SessionStatus::Connected
    });
    assert!(
        matches!(monitor.state(), SessionStatus::Disconnected { reason } if reason.contains("test network interruption")),
        "{:?}",
        monitor.state()
    );
    let terminal = fixture.connect();
    let tunnels = terminal.tunnels.as_ref().unwrap().clone();
    let monitor = terminal.status.as_ref().unwrap().clone();
    let local = tunnels.start(fixture.spec(TunnelKind::Local)).unwrap();
    fixture.shutdown();
    wait_until("abrupt network disconnect", || {
        monitor.state() != SessionStatus::Connected
    });
    assert!(matches!(
        monitor.state(),
        SessionStatus::Disconnected { .. }
    ));
    wait_until("local listener released on network loss", || {
        TcpStream::connect(("127.0.0.1", local.bound_port)).is_err()
    });
    assert!(tunnels.is_closed());
}

#[test]
fn socks_rejects_auth_udp_and_unknown_address_types_and_rules_are_bounded() {
    let fixture = Fixture::new(false);
    let terminal = fixture.connect();
    let tunnels = terminal.tunnels.as_ref().unwrap();
    let dynamic = tunnels.start(fixture.spec(TunnelKind::Dynamic)).unwrap();
    let mut authentication = socket(dynamic.bound_port);
    authentication.write_all(&[5, 1, 2]).unwrap();
    let mut method = [0; 2];
    authentication.read_exact(&mut method).unwrap();
    assert_eq!(method, [5, 255]);
    assert_stream_closed(&mut authentication);
    for (request, code) in [([5, 3, 0, 1], 7), ([5, 1, 0, 9], 8)] {
        let mut rejected = socket(dynamic.bound_port);
        rejected.write_all(&[5, 1, 0]).unwrap();
        rejected.read_exact(&mut method).unwrap();
        rejected.write_all(&request).unwrap();
        let mut reply = [0; 10];
        rejected.read_exact(&mut reply).unwrap();
        assert_eq!(reply[1], code);
        assert_stream_closed(&mut rejected);
    }
    // Maximum 32 simultaneously listening rules, across all kinds.
    for _ in 1..32 {
        tunnels.start(fixture.spec(TunnelKind::Local)).unwrap();
    }
    assert!(matches!(
        tunnels.start(fixture.spec(TunnelKind::Local)),
        Err(TunnelError::Limit)
    ));
    assert_eq!(tunnels.snapshot().len(), 32);
    for rule in tunnels.snapshot() {
        tunnels.stop(rule.id).unwrap();
    }
    let restarted = tunnels.start(fixture.spec(TunnelKind::Local)).unwrap();
    exchange(socket(restarted.bound_port));
    assert_eq!(
        terminal.status.as_ref().unwrap().state(),
        SessionStatus::Connected
    );
}
