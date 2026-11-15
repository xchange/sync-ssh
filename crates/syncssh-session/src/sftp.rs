//! SFTP over a lazily opened channel on the already authenticated SSH connection.
//! The synchronous facade is for background workers; terminal traffic stays independent.
mod batch;
mod resume;
pub use batch::{BatchProgress, BatchReport, JobResult, TransferJob};
pub use resume::ResumeStore;

use flume::{Receiver, Sender};
use russh::{client, ChannelMsg};
use russh_sftp::{
    client::{error::Error as ProtocolError, RawSftpSession},
    protocol::{FileAttributes, FileType, OpenFlags, StatusCode},
};
use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CHUNK: usize = 32 * 1024;
const MAX_ENTRIES: usize = 10_000;
const MAX_PATH: usize = 4096;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum SftpError {
    #[error("SSH 会话已关闭，请重新连接")]
    Closed,
    #[error("当前 SFTP 操作尚未结束，请稍候")]
    Busy,
    #[error("传输已取消")]
    Cancelled,
    #[error("SFTP 请求超时，请检查网络和服务器")]
    Timeout,
    #[error("服务器未启用 SFTP 子系统，终端仍可继续使用")]
    Unsupported,
    #[error("目标已存在；为避免覆盖，请选择其他文件名")]
    AlreadyExists,
    #[error("文件或目录不存在")]
    NotFound,
    #[error("没有读取或写入该路径的权限")]
    PermissionDenied,
    #[error("路径无效，或文件名包含不支持的字符")]
    InvalidPath,
    #[error("仅支持普通文件传输，不支持目录、符号链接或特殊文件")]
    NotRegularFile,
    #[error("目录非空；仅允许删除空目录")]
    DirectoryNotEmpty,
    #[error("仅支持普通目录，不跟随符号链接")]
    NotDirectory,
    #[error("权限须为 0000 至 7777 的八进制值")]
    InvalidPermissions,
    #[error("递归目录层级超过 64 层或批次条目超过 20,000 项")]
    TreeLimit,
    #[error("远端目录超过 10,000 个条目，请进入更小的目录")]
    TooManyEntries,
    #[error("文件在传输过程中发生变化，请重试")]
    FileChanged,
    #[error("断点与源文件不一致，已拒绝续传；请选择新目标或恢复原文件")]
    ResumeMismatch,
    #[error("服务器未提供文件大小或修改时间，无法安全续传")]
    ResumeMetadata,
    #[error("本地文件操作失败，请检查目录、权限及磁盘空间")]
    LocalIo,
    #[error("SFTP 操作失败，请检查服务器路径、权限和连接状态")]
    Protocol,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}
#[derive(Clone, Debug)]
pub struct RemoteEntry {
    pub name: String,
    pub path: String,
    pub kind: RemoteEntryKind,
    pub size: Option<u64>,
    pub modified: Option<u64>,
    pub permissions: Option<u32>,
}
#[derive(Clone, Debug)]
pub struct RemoteDirectory {
    pub path: String,
    pub entries: Vec<RemoteEntry>,
}
#[derive(Clone, Copy, Debug)]
pub struct TransferProgress {
    pub transferred: u64,
    pub total: Option<u64>,
}
/// Mutations are explicit; the GUI must confirm destructive removal before submitting it.
#[derive(Clone, Debug)]
pub enum SftpOperation {
    CreateDirectory { path: String },
    Rename { source: String, destination: String },
    RemoveFile { path: String },
    RemoveEmptyDirectory { path: String },
    Chmod { path: String, permissions: u32 },
}

type Progress = Box<dyn Fn(TransferProgress) + Send>;
fn report(progress: &Mutex<Progress>, value: TransferProgress) {
    if let Ok(callback) = progress.lock() {
        callback(value);
    }
}

struct State {
    closed: Arc<AtomicBool>,
    busy: AtomicBool,
}
struct Permit(Arc<State>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}
#[derive(Clone)]
pub struct SftpHandle {
    sender: Sender<Request>,
    state: Arc<State>,
}
pub(crate) struct Request {
    kind: RequestKind,
    _permit: Permit,
}
enum RequestKind {
    Resume {
        job: TransferJob,
        checkpoints: ResumeStore,
        cancel: Arc<AtomicBool>,
        progress: Progress,
        reply: Sender<Result<(), SftpError>>,
    },
    Metadata {
        path: String,
        cancel: Arc<AtomicBool>,
        reply: Sender<Result<RemoteEntry, SftpError>>,
    },
    Manage {
        operation: SftpOperation,
        cancel: Arc<AtomicBool>,
        reply: Sender<Result<(), SftpError>>,
    },
    List {
        path: String,
        cancel: Arc<AtomicBool>,
        reply: Sender<Result<RemoteDirectory, SftpError>>,
    },
    Upload {
        local: PathBuf,
        remote: String,
        cancel: Arc<AtomicBool>,
        progress: Progress,
        reply: Sender<Result<(), SftpError>>,
    },
    Download {
        local: PathBuf,
        remote: String,
        cancel: Arc<AtomicBool>,
        progress: Progress,
        reply: Sender<Result<(), SftpError>>,
    },
}

pub(crate) fn channel() -> (SftpHandle, Receiver<Request>, Arc<AtomicBool>) {
    let (sender, receiver) = flume::bounded(1);
    let closed = Arc::new(AtomicBool::new(false));
    let state = Arc::new(State {
        closed: closed.clone(),
        busy: AtomicBool::new(false),
    });
    (SftpHandle { sender, state }, receiver, closed)
}
impl SftpHandle {
    pub fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::Acquire) || self.sender.is_disconnected()
    }
    fn permit(&self) -> Result<Permit, SftpError> {
        if self.is_closed() {
            return Err(SftpError::Closed);
        }
        self.state
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| SftpError::Busy)?;
        Ok(Permit(self.state.clone()))
    }
    fn submit(&self, kind: RequestKind) -> Result<(), SftpError> {
        let request = Request {
            kind,
            _permit: self.permit()?,
        };
        self.sender.try_send(request).map_err(|e| match e {
            flume::TrySendError::Full(_) => SftpError::Busy,
            flume::TrySendError::Disconnected(_) => SftpError::Closed,
        })
    }
    fn wait<T>(
        &self,
        receiver: Receiver<Result<T, SftpError>>,
        deadline: Option<Instant>,
    ) -> Result<T, SftpError> {
        loop {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(value) => return value,
                Err(flume::RecvTimeoutError::Disconnected) => return Err(SftpError::Closed),
                Err(flume::RecvTimeoutError::Timeout) => {}
            }
            if self.is_closed() {
                return Err(SftpError::Closed);
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(SftpError::Timeout);
            }
        }
    }
    /// Blocking; call only from a worker thread.
    pub fn list(&self, path: String) -> Result<RemoteDirectory, SftpError> {
        self.list_with_cancel(path, Arc::new(AtomicBool::new(false)))
    }
    pub fn list_with_cancel(
        &self,
        path: String,
        cancel: Arc<AtomicBool>,
    ) -> Result<RemoteDirectory, SftpError> {
        validate_path(&path)?;
        let (reply, receiver) = flume::bounded(1);
        self.submit(RequestKind::List {
            path,
            cancel,
            reply,
        })?;
        self.wait(receiver, Some(Instant::now() + Duration::from_secs(65)))
    }
    pub fn metadata(
        &self,
        path: String,
        cancel: Arc<AtomicBool>,
    ) -> Result<RemoteEntry, SftpError> {
        validate_path(&path)?;
        let (reply, receiver) = flume::bounded(1);
        self.submit(RequestKind::Metadata {
            path,
            cancel,
            reply,
        })?;
        self.wait(receiver, Some(Instant::now() + Duration::from_secs(65)))
    }
    pub fn manage(
        &self,
        operation: SftpOperation,
        cancel: Arc<AtomicBool>,
    ) -> Result<(), SftpError> {
        let (reply, receiver) = flume::bounded(1);
        self.submit(RequestKind::Manage {
            operation,
            cancel,
            reply,
        })?;
        self.wait(receiver, Some(Instant::now() + Duration::from_secs(65)))
    }
    pub fn mkdir(&self, path: String, cancel: Arc<AtomicBool>) -> Result<(), SftpError> {
        self.manage(SftpOperation::CreateDirectory { path }, cancel)
    }
    pub fn rename_no_replace(
        &self,
        source: String,
        destination: String,
        cancel: Arc<AtomicBool>,
    ) -> Result<(), SftpError> {
        self.manage(
            SftpOperation::Rename {
                source,
                destination,
            },
            cancel,
        )
    }
    pub fn remove_file(&self, path: String, cancel: Arc<AtomicBool>) -> Result<(), SftpError> {
        self.manage(SftpOperation::RemoveFile { path }, cancel)
    }
    pub fn remove_empty_directory(
        &self,
        path: String,
        cancel: Arc<AtomicBool>,
    ) -> Result<(), SftpError> {
        self.manage(SftpOperation::RemoveEmptyDirectory { path }, cancel)
    }
    pub fn chmod(
        &self,
        path: String,
        permissions: u32,
        cancel: Arc<AtomicBool>,
    ) -> Result<(), SftpError> {
        self.manage(SftpOperation::Chmod { path, permissions }, cancel)
    }
    /// Upload a regular file without replacing any existing remote path.
    pub fn upload(
        &self,
        local: PathBuf,
        remote: String,
        cancel: Arc<AtomicBool>,
        progress: impl Fn(TransferProgress) + Send + 'static,
    ) -> Result<(), SftpError> {
        validate_destination(&remote)?;
        let (reply, receiver) = flume::bounded(1);
        self.submit(RequestKind::Upload {
            local,
            remote,
            cancel: cancel.clone(),
            progress: Box::new(progress),
            reply,
        })?;
        self.wait(receiver, None)
    }
    /// Download into a private temporary file and publish without replacing an existing path.
    pub fn download(
        &self,
        local: PathBuf,
        remote: String,
        cancel: Arc<AtomicBool>,
        progress: impl Fn(TransferProgress) + Send + 'static,
    ) -> Result<(), SftpError> {
        validate_path(&remote)?;
        let (reply, receiver) = flume::bounded(1);
        self.submit(RequestKind::Download {
            local,
            remote,
            cancel: cancel.clone(),
            progress: Box::new(progress),
            reply,
        })?;
        self.wait(receiver, None)
    }
}

struct Control<'a> {
    closed: &'a AtomicBool,
    cancel: &'a AtomicBool,
}
impl Control<'_> {
    fn check(&self) -> Result<(), SftpError> {
        if self.closed.load(Ordering::Acquire) {
            Err(SftpError::Closed)
        } else if self.cancel.load(Ordering::Acquire) {
            Err(SftpError::Cancelled)
        } else {
            Ok(())
        }
    }
    async fn interrupted(&self) -> SftpError {
        loop {
            if let Err(error) = self.check() {
                return error;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    async fn run<T>(
        &self,
        future: impl Future<Output = Result<T, SftpError>>,
    ) -> Result<T, SftpError> {
        self.check()?;
        tokio::select! {
            biased;
            error = self.interrupted() => Err(error),
            result = tokio::time::timeout(REQUEST_TIMEOUT, future) => result.map_err(|_| SftpError::Timeout)?,
        }
    }
}

// Raw russh Channels do not close on drop. During subsystem negotiation this guard adopts
// the library's stream close-on-drop behavior, including when a timeout/cancellation drops us.
struct PendingChannel(Option<russh::Channel<client::Msg>>);
impl Drop for PendingChannel {
    fn drop(&mut self) {
        if let Some(channel) = self.0.take() {
            drop(channel.into_stream());
        }
    }
}

async fn open_session<H: client::Handler + Send + 'static>(
    ssh: &client::Handle<H>,
) -> Result<RawSftpSession, SftpError> {
    let channel = ssh
        .channel_open_session()
        .await
        .map_err(|_| SftpError::Closed)?;
    let mut pending = PendingChannel(Some(channel));
    pending
        .0
        .as_ref()
        .unwrap()
        .request_subsystem(true, "sftp")
        .await
        .map_err(|_| SftpError::Unsupported)?;
    loop {
        match pending.0.as_mut().unwrap().wait().await {
            Some(ChannelMsg::Success) => break,
            Some(ChannelMsg::Failure | ChannelMsg::Eof | ChannelMsg::Close) | None => {
                return Err(SftpError::Unsupported)
            }
            Some(ChannelMsg::Data { .. } | ChannelMsg::ExtendedData { .. }) => {
                return Err(SftpError::Protocol)
            }
            _ => {}
        }
    }
    let session = RawSftpSession::new_with_config(
        pending.0.take().unwrap().into_stream(),
        russh_sftp::client::Config {
            max_packet_len: 256 * 1024,
            max_concurrent_reads: 1,
            max_concurrent_writes: 1,
            request_timeout_secs: 30,
            ..Default::default()
        },
    );
    let version = session.init().await.map_err(protocol_error)?;
    if version.version != 3 {
        let _ = session.close_session();
        return Err(SftpError::Unsupported);
    }
    Ok(session)
}

pub(crate) async fn serve<H: client::Handler + Send + 'static>(
    ssh: Arc<client::Handle<H>>,
    requests: Receiver<Request>,
    closed: Arc<AtomicBool>,
) {
    let never_cancel = AtomicBool::new(false);
    let idle = Control {
        closed: &closed,
        cancel: &never_cancel,
    };
    let mut session = None;
    loop {
        let request = tokio::select! {
            biased;
            _ = idle.interrupted() => break,
            request = requests.recv_async() => match request { Ok(r) => r, Err(_) => break },
        };
        let setup_cancel = match &request.kind {
            RequestKind::Resume { cancel, .. }
            | RequestKind::List { cancel, .. }
            | RequestKind::Metadata { cancel, .. }
            | RequestKind::Manage { cancel, .. }
            | RequestKind::Upload { cancel, .. }
            | RequestKind::Download { cancel, .. } => cancel.clone(),
        };
        let setup = Control {
            closed: &closed,
            cancel: &setup_cancel,
        };
        let result = if session.is_none() {
            match setup.run(open_session(&ssh)).await {
                Ok(value) => {
                    session = Some(value);
                    Ok(())
                }
                Err(error) => Err(error),
            }
        } else {
            Ok(())
        };
        let completion = match request.kind {
            RequestKind::Resume {
                job,
                checkpoints,
                cancel,
                progress,
                reply,
            } => {
                let control = Control {
                    closed: &closed,
                    cancel: &cancel,
                };
                let result = match result {
                    Ok(()) => {
                        resume::transfer(
                            session.as_ref().unwrap(),
                            job,
                            checkpoints,
                            progress,
                            &control,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                Completion::Transfer(reply, result)
            }
            RequestKind::Metadata {
                path,
                cancel,
                reply,
            } => {
                let control = Control {
                    closed: &closed,
                    cancel: &cancel,
                };
                let result = match result {
                    Ok(()) => metadata(session.as_ref().unwrap(), path, &control).await,
                    Err(error) => Err(error),
                };
                Completion::Metadata(reply, result)
            }
            RequestKind::Manage {
                operation,
                cancel,
                reply,
            } => {
                let control = Control {
                    closed: &closed,
                    cancel: &cancel,
                };
                let result = match result {
                    Ok(()) => manage(session.as_ref().unwrap(), operation, &control).await,
                    Err(error) => Err(error),
                };
                Completion::Transfer(reply, result)
            }
            RequestKind::List {
                path,
                cancel,
                reply,
            } => {
                let control = Control {
                    closed: &closed,
                    cancel: &cancel,
                };
                let result = match result {
                    Ok(()) => {
                        control
                            .run(list(session.as_ref().unwrap(), path, &control))
                            .await
                    }
                    Err(error) => Err(error),
                };
                Completion::List(reply, result)
            }
            RequestKind::Upload {
                local,
                remote,
                cancel,
                progress,
                reply,
            } => {
                let control = Control {
                    closed: &closed,
                    cancel: &cancel,
                };
                let result = match result {
                    Ok(()) => {
                        upload(session.as_ref().unwrap(), local, remote, progress, &control).await
                    }
                    Err(error) => Err(error),
                };
                Completion::Transfer(reply, result)
            }
            RequestKind::Download {
                local,
                remote,
                cancel,
                progress,
                reply,
            } => {
                let control = Control {
                    closed: &closed,
                    cancel: &cancel,
                };
                let result = match result {
                    Ok(()) => {
                        download(session.as_ref().unwrap(), local, remote, progress, &control).await
                    }
                    Err(error) => Err(error),
                };
                Completion::Transfer(reply, result)
            }
        };
        // Any cancelled in-flight request can leave an unclaimed remote handle. Opening a fresh
        // subsystem for the next request releases all such handles without disturbing the shell.
        if let Some(previous) = session.take() {
            let _ = previous.close_session();
        }
        // A completion permits an immediate refresh/new transfer; release the busy permit first.
        drop(request._permit);
        match completion {
            Completion::Metadata(reply, result) => {
                let _ = reply.try_send(result);
            }
            Completion::List(reply, result) => {
                let _ = reply.try_send(result);
            }
            Completion::Transfer(reply, result) => {
                let _ = reply.try_send(result);
            }
        }
    }
    if let Some(session) = session {
        let _ = session.close_session();
    }
}

enum Completion {
    Metadata(
        Sender<Result<RemoteEntry, SftpError>>,
        Result<RemoteEntry, SftpError>,
    ),
    List(
        Sender<Result<RemoteDirectory, SftpError>>,
        Result<RemoteDirectory, SftpError>,
    ),
    Transfer(Sender<Result<(), SftpError>>, Result<(), SftpError>),
}

fn protocol_error(error: ProtocolError) -> SftpError {
    match error {
        ProtocolError::Timeout => SftpError::Timeout,
        ProtocolError::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => SftpError::NotFound,
            StatusCode::PermissionDenied => SftpError::PermissionDenied,
            StatusCode::NoConnection | StatusCode::ConnectionLost => SftpError::Closed,
            StatusCode::OpUnsupported => SftpError::Unsupported,
            _ => SftpError::Protocol,
        },
        _ => SftpError::Protocol,
    }
}
fn local_error(error: std::io::Error) -> SftpError {
    match error.kind() {
        std::io::ErrorKind::AlreadyExists => SftpError::AlreadyExists,
        std::io::ErrorKind::NotFound => SftpError::NotFound,
        std::io::ErrorKind::PermissionDenied => SftpError::PermissionDenied,
        _ => SftpError::LocalIo,
    }
}
fn validate_path(path: &str) -> Result<(), SftpError> {
    if path.is_empty() || path.len() > MAX_PATH || path.chars().any(char::is_control) {
        Err(SftpError::InvalidPath)
    } else {
        Ok(())
    }
}
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 255
        && !name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
}
fn validate_destination(path: &str) -> Result<(), SftpError> {
    validate_path(path)?;
    if safe_name(path.rsplit('/').next().unwrap_or("")) {
        Ok(())
    } else {
        Err(SftpError::InvalidPath)
    }
}
fn child_path(parent: &str, name: &str) -> String {
    format!("{}/{}", parent.trim_end_matches('/'), name)
}
fn kind(attrs: &FileAttributes) -> RemoteEntryKind {
    match attrs.file_type() {
        FileType::Dir => RemoteEntryKind::Directory,
        FileType::File => RemoteEntryKind::File,
        FileType::Symlink => RemoteEntryKind::Symlink,
        FileType::Other => RemoteEntryKind::Other,
    }
}
async fn canonical(
    session: &RawSftpSession,
    path: &str,
    control: &Control<'_>,
) -> Result<String, SftpError> {
    let names = control
        .run(async { session.realpath(path).await.map_err(protocol_error) })
        .await?;
    let path = names
        .files
        .into_iter()
        .next()
        .ok_or(SftpError::Protocol)?
        .filename;
    validate_path(&path)?;
    if !path.starts_with('/') {
        return Err(SftpError::InvalidPath);
    }
    Ok(path)
}
async fn absent(
    session: &RawSftpSession,
    path: &str,
    control: &Control<'_>,
) -> Result<(), SftpError> {
    match control
        .run(async { session.lstat(path).await.map_err(protocol_error) })
        .await
    {
        Err(SftpError::NotFound) => Ok(()),
        Ok(_) => Err(SftpError::AlreadyExists),
        Err(error) => Err(error),
    }
}
async fn list(
    session: &RawSftpSession,
    path: String,
    control: &Control<'_>,
) -> Result<RemoteDirectory, SftpError> {
    let path = canonical(session, &path, control).await?;
    let handle = control
        .run(async { session.opendir(&path).await.map_err(protocol_error) })
        .await?
        .handle;
    let result = async {
        let mut entries = Vec::new();
        let mut scanned = 0usize;
        loop {
            let response = control
                .run(async {
                    match session.readdir(&handle).await {
                        Err(ProtocolError::Status(s)) if s.status_code == StatusCode::Eof => {
                            Ok(None)
                        }
                        value => value.map(Some).map_err(protocol_error),
                    }
                })
                .await?;
            let Some(batch) = response else { break };
            if batch.files.is_empty() {
                return Err(SftpError::Protocol);
            }
            scanned = scanned.saturating_add(batch.files.len());
            if scanned > MAX_ENTRIES + 2 {
                return Err(SftpError::TooManyEntries);
            }
            for file in batch.files {
                if matches!(file.filename.as_str(), "." | "..") {
                    continue;
                }
                if !safe_name(&file.filename) {
                    return Err(SftpError::InvalidPath);
                }
                if entries.len() >= MAX_ENTRIES {
                    return Err(SftpError::TooManyEntries);
                }
                entries.push(RemoteEntry {
                    path: child_path(&path, &file.filename),
                    name: file.filename,
                    kind: kind(&file.attrs),
                    size: file.attrs.size,
                    modified: file.attrs.mtime.map(u64::from),
                    permissions: file.attrs.permissions,
                });
            }
        }
        entries.sort_by(|a, b| {
            (a.kind != RemoteEntryKind::Directory)
                .cmp(&(b.kind != RemoteEntryKind::Directory))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(RemoteDirectory { path, entries })
    }
    .await;
    let close = control
        .run(async { session.close(handle).await.map_err(protocol_error) })
        .await;
    result.and_then(|value| close.map(|_| value))
}

async fn metadata(
    session: &RawSftpSession,
    path: String,
    control: &Control<'_>,
) -> Result<RemoteEntry, SftpError> {
    validate_path(&path)?;
    let attrs = control
        .run(async { session.lstat(&path).await.map_err(protocol_error) })
        .await?
        .attrs;
    Ok(RemoteEntry {
        name: path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .into(),
        kind: kind(&attrs),
        size: attrs.size,
        modified: attrs.mtime.map(u64::from),
        permissions: attrs.permissions,
        path,
    })
}

fn managed_path(path: &str) -> Result<(), SftpError> {
    validate_destination(path)?;
    if !path.starts_with('/') || path.split('/').any(|part| matches!(part, "." | "..")) {
        return Err(SftpError::InvalidPath);
    }
    Ok(())
}

async fn commit<T>(
    future: impl Future<Output = Result<T, ProtocolError>>,
) -> Result<(), SftpError> {
    // Once a mutation is sent, await its acknowledgement. Reporting cancellation
    // after a successful delete/rename would misrepresent the remote state.
    tokio::time::timeout(REQUEST_TIMEOUT, future)
        .await
        .map_err(|_| SftpError::Timeout)?
        .map_err(protocol_error)
        .map(|_| ())
}

async fn manage(
    session: &RawSftpSession,
    operation: SftpOperation,
    control: &Control<'_>,
) -> Result<(), SftpError> {
    match operation {
        SftpOperation::CreateDirectory { path } => {
            managed_path(&path)?;
            absent(session, &path, control).await?;
            let mut attrs = FileAttributes::empty();
            attrs.permissions = Some(0o755);
            control.check()?;
            commit(session.mkdir(path, attrs)).await
        }
        SftpOperation::Rename {
            source,
            destination,
        } => {
            managed_path(&source)?;
            managed_path(&destination)?;
            metadata(session, source.clone(), control).await?;
            absent(session, &destination, control).await?;
            control.check()?;
            // SFTP v3 RENAME has no-replace semantics. Do not use posix-rename.
            commit(session.rename(source, destination)).await
        }
        SftpOperation::RemoveFile { path } => {
            managed_path(&path)?;
            let entry = metadata(session, path.clone(), control).await?;
            if !matches!(entry.kind, RemoteEntryKind::File | RemoteEntryKind::Symlink) {
                return Err(SftpError::NotRegularFile);
            }
            control.check()?;
            commit(session.remove(path)).await
        }
        SftpOperation::RemoveEmptyDirectory { path } => {
            managed_path(&path)?;
            if metadata(session, path.clone(), control).await?.kind != RemoteEntryKind::Directory {
                return Err(SftpError::NotDirectory);
            }
            if !list(session, path.clone(), control)
                .await?
                .entries
                .is_empty()
            {
                return Err(SftpError::DirectoryNotEmpty);
            }
            control.check()?;
            commit(session.rmdir(path)).await
        }
        SftpOperation::Chmod { path, permissions } => {
            managed_path(&path)?;
            if permissions > 0o7777 {
                return Err(SftpError::InvalidPermissions);
            }
            let entry = metadata(session, path.clone(), control).await?;
            if !matches!(
                entry.kind,
                RemoteEntryKind::File | RemoteEntryKind::Directory
            ) {
                return Err(SftpError::NotRegularFile);
            }
            let mut attrs = FileAttributes::empty();
            attrs.permissions = Some(permissions);
            control.check()?;
            commit(session.setstat(path, attrs)).await
        }
    }
}

async fn upload(
    session: &RawSftpSession,
    local: PathBuf,
    remote: String,
    progress: Progress,
    control: &Control<'_>,
) -> Result<(), SftpError> {
    control.check()?;
    validate_destination(&remote)?;
    let source_meta = control
        .run(async {
            tokio::fs::symlink_metadata(&local)
                .await
                .map_err(local_error)
        })
        .await?;
    if !source_meta.file_type().is_file() {
        return Err(SftpError::NotRegularFile);
    }
    let mut source = control
        .run(async { tokio::fs::File::open(&local).await.map_err(local_error) })
        .await?;
    let metadata = control
        .run(async { source.metadata().await.map_err(local_error) })
        .await?;
    if !metadata.is_file() {
        return Err(SftpError::NotRegularFile);
    }
    let total = metadata.len();
    absent(session, &remote, control).await?;
    let (parent, _) = remote.rsplit_once('/').unwrap_or((".", &remote));
    let parent = canonical(
        session,
        if parent.is_empty() { "/" } else { parent },
        control,
    )
    .await?;
    let temporary = child_path(
        &parent,
        &format!(".syncssh-upload-{:032x}.part", rand::random::<u128>()),
    );
    let mut attrs = FileAttributes::empty();
    attrs.permissions = Some(0o600);
    control.check()?;
    // Once an exclusive CREATE is sent, await its acknowledgement before honouring Cancel.
    // Otherwise the server may create a file after this future has already returned, leaving
    // no confirmed handle/ownership for cleanup. Never remove an unconfirmed (or colliding) path.
    let handle = tokio::time::timeout(
        REQUEST_TIMEOUT,
        session.open(
            &temporary,
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
            attrs,
        ),
    )
    .await
    .map_err(|_| SftpError::Timeout)?
    .map_err(protocol_error)?
    .handle;
    let progress = Mutex::new(progress);
    let result = async {
        control.check()?;
        let mut count = 0u64;
        let mut buffer = vec![0; CHUNK];
        report(
            &progress,
            TransferProgress {
                transferred: 0,
                total: Some(total),
            },
        );
        loop {
            let length = control
                .run(async { source.read(&mut buffer).await.map_err(local_error) })
                .await?;
            if length == 0 {
                break;
            }
            control
                .run(async {
                    session
                        .write(&handle, count, buffer[..length].to_vec())
                        .await
                        .map_err(protocol_error)
                })
                .await?;
            count = count
                .checked_add(length as u64)
                .ok_or(SftpError::FileChanged)?;
            if count > total {
                return Err(SftpError::FileChanged);
            }
            report(
                &progress,
                TransferProgress {
                    transferred: count,
                    total: Some(total),
                },
            );
        }
        if count != total {
            return Err(SftpError::FileChanged);
        }
        control
            .run(async { session.close(&handle).await.map_err(protocol_error) })
            .await?;
        absent(session, &remote, control).await?;
        // Standard SFTP v3 RENAME fails if newpath exists. Do not use the overwriting POSIX extension.
        control.check()?;
        // Once publication begins, await its acknowledgement even if the user presses Cancel.
        // Claiming cancellation after a successful rename would be misleading.
        tokio::time::timeout(REQUEST_TIMEOUT, session.rename(&temporary, &remote))
            .await
            .map_err(|_| SftpError::Timeout)?
            .map_err(protocol_error)?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            let _ = session.close(&handle).await;
            let _ = session.remove(&temporary).await;
        })
        .await;
    }
    result
}

async fn download(
    session: &RawSftpSession,
    local: PathBuf,
    remote: String,
    progress: Progress,
    control: &Control<'_>,
) -> Result<(), SftpError> {
    control.check()?;
    validate_path(&remote)?;
    let metadata = control
        .run(async { session.lstat(&remote).await.map_err(protocol_error) })
        .await?
        .attrs;
    if kind(&metadata) != RemoteEntryKind::File {
        return Err(SftpError::NotRegularFile);
    }
    match control
        .run(async {
            tokio::fs::symlink_metadata(&local)
                .await
                .map_err(local_error)
        })
        .await
    {
        Ok(_) => return Err(SftpError::AlreadyExists),
        Err(SftpError::NotFound) => {}
        Err(error) => return Err(error),
    }
    let parent = local
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_owned();
    let temporary = control
        .run(async {
            tokio::task::spawn_blocking(move || tempfile::NamedTempFile::new_in(parent))
                .await
                .map_err(|_| SftpError::LocalIo)?
                .map_err(local_error)
        })
        .await?;
    let (file, path) = temporary.into_parts();
    let mut target = tokio::fs::File::from_std(file);
    let handle = control
        .run(async {
            session
                .open(&remote, OpenFlags::READ, FileAttributes::empty())
                .await
                .map_err(protocol_error)
        })
        .await?
        .handle;
    let progress = Mutex::new(progress);
    let result = async {
        let current = control
            .run(async { session.fstat(&handle).await.map_err(protocol_error) })
            .await?
            .attrs;
        if kind(&current) != RemoteEntryKind::File {
            return Err(SftpError::NotRegularFile);
        }
        let total = current.size;
        let mut count = 0u64;
        report(
            &progress,
            TransferProgress {
                transferred: 0,
                total,
            },
        );
        loop {
            let data = control
                .run(async {
                    match session.read(&handle, count, CHUNK as u32).await {
                        Err(ProtocolError::Status(status))
                            if status.status_code == StatusCode::Eof =>
                        {
                            Ok(None)
                        }
                        value => value.map(|d| Some(d.data)).map_err(protocol_error),
                    }
                })
                .await?;
            let Some(data) = data else {
                break;
            };
            if data.is_empty() || data.len() > CHUNK {
                return Err(SftpError::Protocol);
            }
            control
                .run(async { target.write_all(&data).await.map_err(local_error) })
                .await?;
            count = count
                .checked_add(data.len() as u64)
                .ok_or(SftpError::FileChanged)?;
            if total.is_some_and(|n| count > n) {
                return Err(SftpError::FileChanged);
            }
            report(
                &progress,
                TransferProgress {
                    transferred: count,
                    total,
                },
            );
        }
        if total.is_some_and(|n| n != count) {
            return Err(SftpError::FileChanged);
        }
        control
            .run(async { target.flush().await.map_err(local_error) })
            .await?;
        control
            .run(async { target.sync_all().await.map_err(local_error) })
            .await?;
        control
            .run(async { session.close(&handle).await.map_err(protocol_error) })
            .await?;
        Ok(())
    }
    .await;
    // Wait for any outstanding filesystem operation before TempPath is removed or renamed.
    // In particular Windows cannot remove a temporary file while an async write still owns it.
    drop(target.into_std().await);
    if let Err(error) = result {
        let _ = tokio::time::timeout(Duration::from_secs(1), session.close(&handle)).await;
        return Err(error);
    }
    control.check()?;
    // Moving the TempPath into the worker ensures failed publication removes the partial file.
    tokio::task::spawn_blocking(move || {
        path.persist_noclobber(local)
            .map_err(|e| local_error(e.error))
    })
    .await
    .map_err(|_| SftpError::LocalIo)?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn entry_names_and_remote_destinations_are_unambiguous() {
        for name in [
            "",
            ".",
            "..",
            "../secret",
            "a/b",
            "a\\b",
            "line\nname",
            "bad\0name",
        ] {
            assert!(!safe_name(name));
        }
        for name in ["中文.txt", "file with spaces", "a;echo.txt", ".hidden"] {
            assert!(safe_name(name));
        }
        assert_eq!(validate_destination("/root/"), Err(SftpError::InvalidPath));
        assert_eq!(child_path("/", "file"), "/file");
        assert_eq!(child_path("/home/user/", "file"), "/home/user/file");
    }
    #[test]
    fn handle_closure_and_busy_are_explicit() {
        let (handle, receiver, closed) = channel();
        let permit = handle.permit().unwrap();
        assert!(matches!(handle.permit(), Err(SftpError::Busy)));
        drop(permit);
        assert!(handle.permit().is_ok());
        closed.store(true, Ordering::Release);
        assert!(handle.is_closed());
        assert!(matches!(handle.list(".".into()), Err(SftpError::Closed)));
        drop(receiver);
    }
    #[test]
    fn server_error_messages_are_not_exposed() {
        let error = ProtocolError::Status(russh_sftp::protocol::Status {
            id: 1,
            status_code: StatusCode::PermissionDenied,
            error_message: "password-secret".into(),
            language_tag: "".into(),
        });
        assert!(!protocol_error(error)
            .to_string()
            .contains("password-secret"));
    }
}
