//! Runtime TCP forwarding on the already verified, authenticated SSH session.
//! UI-facing start/stop methods are blocking and belong on a background worker.
use flume::{Receiver, Sender};
use russh::{client, Channel};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
    task::{JoinHandle, JoinSet},
};

const MAX_RULES: usize = 32;
const MAX_CONNECTIONS: usize = 64;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TunnelKind {
    Local,
    Remote,
    Dynamic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunnelSpec {
    pub kind: TunnelKind,
    pub listen_host: String,
    pub listen_port: u16,
    pub target_host: String,
    pub target_port: u16,
}

impl Default for TunnelSpec {
    fn default() -> Self {
        Self {
            kind: TunnelKind::Local,
            listen_host: "127.0.0.1".into(),
            listen_port: 0,
            target_host: "127.0.0.1".into(),
            target_port: 22,
        }
    }
}

impl TunnelSpec {
    pub fn validate(&self) -> Result<(), TunnelError> {
        if !valid_host(&self.listen_host) {
            return Err(TunnelError::Invalid("监听地址无效".into()));
        }
        if self.kind != TunnelKind::Dynamic
            && (!valid_host(&self.target_host) || self.target_port == 0)
        {
            return Err(TunnelError::Invalid("目标主机或端口无效".into()));
        }
        Ok(())
    }
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.chars().any(|c| c.is_control() || c.is_whitespace())
        && !host.contains(['/', '\\', '@'])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TunnelState {
    Running,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunnelStatus {
    pub id: u64,
    pub spec: TunnelSpec,
    pub bound_port: u16,
    pub state: TunnelState,
    pub active_connections: usize,
    pub last_error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("SSH 连接已关闭")]
    Closed,
    #[error("隧道配置无效：{0}")]
    Invalid(String),
    #[error("隧道已达到规则或连接数量上限")]
    Limit,
    #[error("隧道操作超时")]
    Timeout,
    #[error("隧道不存在")]
    NotFound,
    #[error("隧道操作失败：{0}")]
    Transport(String),
}

#[derive(Clone)]
pub struct TunnelHandle {
    commands: Sender<Command>,
    states: watch::Sender<Vec<TunnelStatus>>,
    closed: Arc<AtomicBool>,
}

impl TunnelHandle {
    pub fn snapshot(&self) -> Vec<TunnelStatus> {
        self.states.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<Vec<TunnelStatus>> {
        self.states.subscribe()
    }
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire) || self.commands.is_disconnected()
    }
    pub fn start(&self, spec: TunnelSpec) -> Result<TunnelStatus, TunnelError> {
        spec.validate()?;
        let (reply, response) = flume::bounded(1);
        self.submit(Command::Start(spec, reply))?;
        self.wait(response)
    }
    pub fn stop(&self, id: u64) -> Result<(), TunnelError> {
        let (reply, response) = flume::bounded(1);
        self.submit(Command::Stop(id, reply))?;
        self.wait(response)
    }
    fn submit(&self, command: Command) -> Result<(), TunnelError> {
        if self.is_closed() {
            return Err(TunnelError::Closed);
        }
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                flume::TrySendError::Full(_) => TunnelError::Limit,
                flume::TrySendError::Disconnected(_) => TunnelError::Closed,
            })
    }
    fn wait<T>(&self, receiver: Receiver<Result<T, TunnelError>>) -> Result<T, TunnelError> {
        loop {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(result) => return result,
                Err(flume::RecvTimeoutError::Disconnected) => return Err(TunnelError::Closed),
                Err(flume::RecvTimeoutError::Timeout) if self.is_closed() => {
                    return Err(TunnelError::Closed)
                }
                Err(flume::RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

enum Command {
    Start(TunnelSpec, Sender<Result<TunnelStatus, TunnelError>>),
    Stop(u64, Sender<Result<(), TunnelError>>),
}

pub(crate) struct Forwarded {
    pub channel: Channel<client::Msg>,
    pub connected_port: u32,
    pub reply: client::ChannelOpenHandle,
}

pub(crate) struct Service {
    commands: Receiver<Command>,
    forwarded: Receiver<Forwarded>,
    handle: TunnelHandle,
}

pub(crate) fn channel() -> (TunnelHandle, Sender<Forwarded>, Service) {
    let (commands, requests) = flume::bounded(32);
    let (forwarded, incoming) = flume::bounded(MAX_CONNECTIONS);
    let (states, _) = watch::channel(Vec::new());
    let handle = TunnelHandle {
        commands,
        states,
        closed: Arc::new(AtomicBool::new(false)),
    };
    (
        handle.clone(),
        forwarded,
        Service {
            commands: requests,
            forwarded: incoming,
            handle,
        },
    )
}

impl TunnelHandle {
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

struct Rule {
    status: TunnelStatus,
    cancel: watch::Sender<bool>,
    listener: Option<JoinHandle<()>>,
}

impl Drop for Rule {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
        if let Some(listener) = &self.listener {
            listener.abort();
        }
    }
}

enum Accepted {
    Socket(u64, TcpStream, SocketAddr),
    Failed(u64, String),
}

fn publish(handle: &TunnelHandle, rules: &BTreeMap<u64, Rule>) {
    handle
        .states
        .send_replace(rules.values().map(|rule| rule.status.clone()).collect());
}

impl Service {
    pub(crate) async fn run<H: client::Handler + Send + 'static>(
        self,
        session: Arc<client::Handle<H>>,
        fatal: Sender<String>,
    ) {
        let mut rules = BTreeMap::<u64, Rule>::new();
        let (accepted_tx, accepted_rx) = flume::bounded(MAX_CONNECTIONS);
        let mut connections = JoinSet::<(u64, Result<(), TunnelError>)>::new();
        let mut next_id = 0u64;
        while !self.handle.is_closed() && !session.is_closed() {
            tokio::select! {
                biased;
                command = self.commands.recv_async() => match command {
                    Ok(Command::Start(spec, reply)) => {
                        next_id += 1;
                        let result = self.start_rule(next_id, spec, &session, &mut rules, &accepted_tx, &fatal).await;
                        publish(&self.handle, &rules);
                        if let Err(error) = reply.send(result) {
                            // The caller normally waits until completion; if it disappeared,
                            // don't leave an unowned listener running.
                            if let Ok(status) = error.0 { let _ = stop_rule(status.id, &session, &mut rules, &fatal).await; }
                        }
                    }
                    Ok(Command::Stop(id, reply)) => {
                        let result = stop_rule(id, &session, &mut rules, &fatal).await;
                        publish(&self.handle, &rules);
                        let _ = reply.send(result);
                    }
                    Err(_) => break,
                },
                accepted = accepted_rx.recv_async() => if let Ok(accepted) = accepted {
                    match accepted {
                        Accepted::Socket(id, socket, peer) => {
                            let Some(rule) = rules.get_mut(&id).filter(|r| r.status.state == TunnelState::Running) else { continue };
                            if connections.len() >= MAX_CONNECTIONS {
                                rule.status.last_error = Some(TunnelError::Limit.to_string());
                            } else {
                                rule.status.active_connections += 1;
                                let spec = rule.status.spec.clone();
                                let cancel = rule.cancel.subscribe();
                                let connection = session.clone();
                                connections.spawn(async move { (id, cancelled(cancel, serve_local(socket, peer, spec, connection)).await) });
                            }
                        }
                        Accepted::Failed(id, error) => if let Some(rule) = rules.get_mut(&id) {
                            rule.status.state = TunnelState::Failed;
                            rule.status.last_error = Some(error);
                            rule.cancel.send_replace(true);
                        },
                    }
                    publish(&self.handle, &rules);
                },
                incoming = self.forwarded.recv_async() => if let Ok(incoming) = incoming {
                    let rule = rules.values_mut().find(|rule| rule.status.state == TunnelState::Running
                        && rule.status.spec.kind == TunnelKind::Remote && rule.status.bound_port as u32 == incoming.connected_port);
                    if let Some(rule) = rule {
                        if connections.len() >= MAX_CONNECTIONS {
                            rule.status.last_error = Some(TunnelError::Limit.to_string());
                        } else {
                            let id = rule.status.id;
                            rule.status.active_connections += 1;
                            let spec = rule.status.spec.clone();
                            let cancel = rule.cancel.subscribe();
                            connections.spawn(async move { (id, cancelled(cancel, serve_remote(incoming, spec)).await) });
                        }
                        publish(&self.handle, &rules);
                    }
                    // Dropping an unknown/excess request automatically rejects it.
                },
                result = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Ok((id, result))) = result {
                        if let Some(rule) = rules.get_mut(&id) {
                            rule.status.active_connections = rule.status.active_connections.saturating_sub(1);
                            if let Err(error) = result {
                                if rule.status.state == TunnelState::Running {
                                    rule.status.last_error = Some(error.to_string());
                                }
                            }
                        }
                        publish(&self.handle, &rules);
                    }
                },
                _ = tokio::time::sleep(Duration::from_millis(50)) => {},
            }
        }
        self.handle.close();
        for rule in rules.values_mut() {
            rule.cancel.send_replace(true);
            if let Some(listener) = rule.listener.take() {
                listener.abort();
                let _ = listener.await;
            }
            rule.status.state = TunnelState::Stopped;
            rule.status.active_connections = 0;
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        publish(&self.handle, &rules);
        // The owning SSH worker disconnects the entire session immediately
        // after this service exits, releasing every remote listener atomically.
    }

    async fn start_rule<H: client::Handler + Send + 'static>(
        &self,
        id: u64,
        spec: TunnelSpec,
        session: &Arc<client::Handle<H>>,
        rules: &mut BTreeMap<u64, Rule>,
        accepted: &Sender<Accepted>,
        fatal: &Sender<String>,
    ) -> Result<TunnelStatus, TunnelError> {
        spec.validate()?;
        let running = rules
            .values()
            .filter(|r| r.status.state == TunnelState::Running)
            .count();
        if running >= MAX_RULES {
            return Err(TunnelError::Limit);
        }
        // Retain only a bounded amount of stopped-rule history.
        while rules.len() >= MAX_RULES * 2 {
            let Some(old) = rules
                .iter()
                .find(|(_, r)| r.status.state != TunnelState::Running)
                .map(|(id, _)| *id)
            else {
                break;
            };
            rules.remove(&old);
        }
        let (cancel, stop) = watch::channel(false);
        let (bound_port, listener) = match spec.kind {
            TunnelKind::Remote => {
                // Forwarded-tcpip reports the listening port. Keep that key
                // unambiguous even when a server rewrites the requested address.
                if spec.listen_port != 0
                    && rules.values().any(|r| {
                        r.status.state == TunnelState::Running
                            && r.status.spec.kind == TunnelKind::Remote
                            && r.status.bound_port == spec.listen_port
                    })
                {
                    return Err(TunnelError::Invalid("此远程端口已有转发规则".into()));
                }
                let allocated = match tokio::time::timeout(
                    REQUEST_TIMEOUT,
                    session.tcpip_forward(&spec.listen_host, spec.listen_port as u32),
                )
                .await
                {
                    Ok(result) => result.map_err(transport)?,
                    Err(_) => {
                        let _ = fatal.try_send(
                            "远程转发请求超时；为避免遗留未知远程监听器，已关闭 SSH 连接".into(),
                        );
                        return Err(TunnelError::Timeout);
                    }
                };
                let port = if spec.listen_port == 0 {
                    let Some(port) = u16::try_from(allocated).ok().filter(|p| *p != 0) else {
                        let _ = fatal.try_send(
                            "服务器返回了无效远程监听端口，已关闭 SSH 连接以释放监听器".into(),
                        );
                        return Err(TunnelError::Transport("服务器返回了无效监听端口".into()));
                    };
                    if rules.values().any(|r| {
                        r.status.state == TunnelState::Running
                            && r.status.spec.kind == TunnelKind::Remote
                            && r.status.bound_port == port
                    }) {
                        let _ = fatal.try_send(
                            "服务器返回了重复远程监听端口，已关闭 SSH 连接以释放监听器".into(),
                        );
                        return Err(TunnelError::Transport("服务器返回了重复监听端口".into()));
                    }
                    port
                } else {
                    spec.listen_port
                };
                (port, None)
            }
            TunnelKind::Local | TunnelKind::Dynamic => {
                let listener = tokio::time::timeout(
                    REQUEST_TIMEOUT,
                    TcpListener::bind((spec.listen_host.as_str(), spec.listen_port)),
                )
                .await
                .map_err(|_| TunnelError::Timeout)?
                .map_err(transport)?;
                let port = listener.local_addr().map_err(transport)?.port();
                let accepted = accepted.clone();
                let task =
                    tokio::spawn(async move { accept_loop(id, listener, accepted, stop).await });
                (port, Some(task))
            }
        };
        let status = TunnelStatus {
            id,
            spec,
            bound_port,
            state: TunnelState::Running,
            active_connections: 0,
            last_error: None,
        };
        rules.insert(
            id,
            Rule {
                status: status.clone(),
                cancel,
                listener,
            },
        );
        Ok(status)
    }
}

async fn stop_rule<H: client::Handler + Send + 'static>(
    id: u64,
    session: &Arc<client::Handle<H>>,
    rules: &mut BTreeMap<u64, Rule>,
    fatal: &Sender<String>,
) -> Result<(), TunnelError> {
    let rule = rules.get_mut(&id).ok_or(TunnelError::NotFound)?;
    if rule.status.state == TunnelState::Stopped {
        return Ok(());
    }
    if rule.status.spec.kind == TunnelKind::Remote {
        let result = tokio::time::timeout(
            REQUEST_TIMEOUT,
            session
                .cancel_tcpip_forward(&rule.status.spec.listen_host, rule.status.bound_port as u32),
        )
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                rule.status.last_error = Some(error.to_string());
                return Err(transport(error));
            }
            Err(_) => {
                let _ = fatal.try_send("远程转发停止请求超时，已关闭 SSH 连接以释放监听器".into());
                return Err(TunnelError::Timeout);
            }
        }
    }
    rule.cancel.send_replace(true);
    if let Some(listener) = rule.listener.take() {
        listener.abort();
        let _ = listener.await;
    }
    rule.status.state = TunnelState::Stopped;
    rule.status.active_connections = 0;
    Ok(())
}

async fn accept_loop(
    id: u64,
    listener: TcpListener,
    accepted: Sender<Accepted>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            biased;
            _ = stop.changed() => break,
            result = listener.accept() => match result {
                Ok((socket, peer)) => { let _ = accepted.try_send(Accepted::Socket(id, socket, peer)); }
                Err(error) => { let _ = accepted.try_send(Accepted::Failed(id, error.to_string())); break; }
            }
        }
    }
}

async fn cancelled<T>(
    mut cancel: watch::Receiver<bool>,
    operation: impl std::future::Future<Output = Result<T, TunnelError>>,
) -> Result<T, TunnelError> {
    if *cancel.borrow() {
        return Err(TunnelError::Closed);
    }
    tokio::select! { biased; _=cancel.changed()=>Err(TunnelError::Closed), result=operation=>result }
}

async fn serve_local<H: client::Handler + Send + 'static>(
    mut socket: TcpStream,
    peer: SocketAddr,
    spec: TunnelSpec,
    session: Arc<client::Handle<H>>,
) -> Result<(), TunnelError> {
    let (target, port) = if spec.kind == TunnelKind::Dynamic {
        tokio::time::timeout(REQUEST_TIMEOUT, socks_request(&mut socket))
            .await
            .map_err(|_| TunnelError::Timeout)??
    } else {
        (spec.target_host, spec.target_port)
    };
    let channel = match tokio::time::timeout(
        REQUEST_TIMEOUT,
        session.channel_open_direct_tcpip(
            target,
            port as u32,
            peer.ip().to_string(),
            peer.port() as u32,
        ),
    )
    .await
    {
        Ok(Ok(channel)) => channel,
        result => {
            if spec.kind == TunnelKind::Dynamic {
                let _ = socket.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await;
            }
            return Err(match result {
                Ok(Err(error)) => transport(error),
                _ => TunnelError::Timeout,
            });
        }
    };
    if spec.kind == TunnelKind::Dynamic {
        socket
            .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
            .await
            .map_err(transport)?;
    }
    let mut stream = channel.into_stream();
    tokio::io::copy_bidirectional(&mut socket, &mut stream)
        .await
        .map_err(transport)?;
    Ok(())
}

async fn serve_remote(incoming: Forwarded, spec: TunnelSpec) -> Result<(), TunnelError> {
    let mut socket = tokio::time::timeout(
        REQUEST_TIMEOUT,
        TcpStream::connect((spec.target_host.as_str(), spec.target_port)),
    )
    .await
    .map_err(|_| TunnelError::Timeout)?
    .map_err(transport)?;
    incoming.reply.accept().await;
    let mut stream = incoming.channel.into_stream();
    tokio::io::copy_bidirectional(&mut socket, &mut stream)
        .await
        .map_err(transport)?;
    Ok(())
}

async fn socks_request(socket: &mut TcpStream) -> Result<(String, u16), TunnelError> {
    let mut greeting = [0; 2];
    socket.read_exact(&mut greeting).await.map_err(transport)?;
    if greeting[0] != 5 || greeting[1] == 0 {
        return Err(TunnelError::Invalid("仅支持 SOCKS5 CONNECT".into()));
    }
    let mut methods = vec![0; greeting[1] as usize];
    socket.read_exact(&mut methods).await.map_err(transport)?;
    if !methods.contains(&0) {
        socket.write_all(&[5, 255]).await.map_err(transport)?;
        return Err(TunnelError::Invalid("SOCKS5 仅支持无认证模式".into()));
    }
    socket.write_all(&[5, 0]).await.map_err(transport)?;
    let mut header = [0; 4];
    socket.read_exact(&mut header).await.map_err(transport)?;
    if header[0] != 5 || header[1] != 1 || header[2] != 0 {
        socket
            .write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0])
            .await
            .map_err(transport)?;
        return Err(TunnelError::Invalid(
            "SOCKS5 仅支持 TCP CONNECT，不支持 BIND/UDP".into(),
        ));
    }
    let host = match header[3] {
        1 => {
            let mut ip = [0; 4];
            socket.read_exact(&mut ip).await.map_err(transport)?;
            std::net::Ipv4Addr::from(ip).to_string()
        }
        4 => {
            let mut ip = [0; 16];
            socket.read_exact(&mut ip).await.map_err(transport)?;
            std::net::Ipv6Addr::from(ip).to_string()
        }
        3 => {
            let len = socket.read_u8().await.map_err(transport)?;
            let mut name = vec![0; len as usize];
            socket.read_exact(&mut name).await.map_err(transport)?;
            String::from_utf8(name).map_err(|_| TunnelError::Invalid("SOCKS5 域名无效".into()))?
        }
        _ => {
            socket
                .write_all(&[5, 8, 0, 1, 0, 0, 0, 0, 0, 0])
                .await
                .map_err(transport)?;
            return Err(TunnelError::Invalid("SOCKS5 地址类型不支持".into()));
        }
    };
    let port = socket.read_u16().await.map_err(transport)?;
    if !valid_host(&host) || port == 0 {
        return Err(TunnelError::Invalid("SOCKS5 目标无效".into()));
    }
    Ok((host, port))
}

fn transport(error: impl std::fmt::Display) -> TunnelError {
    TunnelError::Transport(error.to_string())
}
