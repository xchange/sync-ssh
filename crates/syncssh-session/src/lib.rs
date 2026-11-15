//! Real local PTY and SSH transports. All connections verify the server key
//! before sending any credentials. Call blocking factories on a worker thread.

mod known_hosts;
mod local;
mod sftp;
mod ssh;
mod tunnel;
mod zmodem;

pub use known_hosts::{HostKey, KnownHosts};
pub use local::local_shell;
pub use sftp::{
    BatchProgress, BatchReport, JobResult, RemoteDirectory, RemoteEntry, RemoteEntryKind,
    ResumeStore, SftpError, SftpHandle, SftpOperation, TransferJob, TransferProgress,
};
pub use ssh::{connect_ssh, SshAuth, SshConfig};
pub use tunnel::{TunnelError, TunnelHandle, TunnelKind, TunnelSpec, TunnelState, TunnelStatus};
pub use zmodem::{with_zmodem, ZmodemDirection, ZmodemEvent, ZmodemHandle, ZmodemSummary};

use std::io::{Read, Write};
use std::sync::Arc;

/// Keep `guard` alive as long as a terminal tab. Dropping it closes the session.
/// The resize callback arguments are (columns, rows).
pub struct TerminalIo {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub resize: Arc<dyn Fn(u16, u16) + Send + Sync>,
    pub guard: SessionGuard,
    pub sftp: Option<SftpHandle>,
    pub tunnels: Option<TunnelHandle>,
    pub status: Option<SessionMonitor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Connected,
    Exited { exit_code: Option<u32> },
    Disconnected { reason: String },
    Closed,
}

#[derive(Clone, Debug)]
pub struct SessionMonitor(tokio::sync::watch::Sender<SessionStatus>);

impl SessionMonitor {
    pub fn state(&self) -> SessionStatus {
        self.0.borrow().clone()
    }
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<SessionStatus> {
        self.0.subscribe()
    }
    pub(crate) fn connected() -> Self {
        let (sender, _) = tokio::sync::watch::channel(SessionStatus::Connected);
        Self(sender)
    }
    pub(crate) fn finish(&self, status: SessionStatus) {
        self.0.send_if_modified(|current| {
            if matches!(current, SessionStatus::Connected) {
                *current = status;
                true
            } else {
                false
            }
        });
    }
}

/// Closing a tab cancels network tasks or terminates/reaps its local shell.
pub struct SessionGuard(Option<Box<dyn FnOnce() + Send>>);

impl SessionGuard {
    pub(crate) fn new(close: impl FnOnce() + Send + 'static) -> Self {
        Self(Some(Box::new(close)))
    }

    pub fn close(mut self) {
        if let Some(close) = self.0.take() {
            close();
        }
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Some(close) = self.0.take() {
            close();
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Unrecognized SSH server key: {0}")]
    UnknownHostKey(HostKey),
    #[error("SSH HOST KEY CHANGED for {}:{}; expected {}, received {}", .expected.host, .expected.port, .expected.fingerprint, .received.fingerprint)]
    HostKeyChanged {
        expected: HostKey,
        received: HostKey,
    },
    #[error("SSH authentication failed; check the username and credential")]
    Authentication,
    #[error("SSH connection timed out after 20 seconds")]
    Timeout,
    #[error("Invalid SSH configuration: {0}")]
    InvalidConfig(String),
    #[error("SSH transport: {0}")]
    Transport(String),
    #[error("Terminal I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("Known-hosts file is invalid: {0}")]
    KnownHosts(String),
}

pub(crate) fn dimensions(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(2, 1000), rows.clamp(2, 500))
}
