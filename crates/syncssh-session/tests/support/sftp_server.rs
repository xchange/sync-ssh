//! An isolated test-only SSH/SFTP server. Paths resolve exclusively inside a fresh temp directory.
#![allow(dead_code)]
use russh::keys::{Algorithm, PrivateKey};
use russh::server::{self, Server as _, Session};
use russh::{Channel, ChannelId, Pty};
use russh_sftp::protocol::{
    Attrs, Data, File as Entry, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode,
};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use syncssh_session::{HostKey, KnownHosts, SshAuth, SshConfig};

pub const USER: &str = "sftp-tester";
pub const PASSWORD: &str = "test-only-sftp-password";

#[derive(Clone)]
pub struct Options {
    pub enabled: bool,
    pub chunk_delay: Duration,
    pub open_response_delay: Duration,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            enabled: true,
            chunk_delay: Duration::ZERO,
            open_response_delay: Duration::ZERO,
        }
    }
}

pub struct Fixture {
    pub root: tempfile::TempDir,
    pub port: u16,
    pub key: HostKey,
    pub authentications: Arc<AtomicUsize>,
    pub connections: Arc<AtomicUsize>,
    pub resize: std::sync::mpsc::Receiver<(u32, u32)>,
    pub upload_opened: std::sync::mpsc::Receiver<String>,
    pub closed_subsystems: Arc<AtomicUsize>,
    stop: flume::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    pub fn new(options: Options) -> Self {
        Self::start(tempfile::tempdir().unwrap(), options)
    }
    pub fn new_in(parent: &Path, options: Options) -> Self {
        fs::create_dir_all(parent).unwrap();
        Self::start(
            tempfile::Builder::new()
                .prefix("sftp-server-")
                .tempdir_in(parent)
                .unwrap(),
            options,
        )
    }
    fn start(root: tempfile::TempDir, options: Options) -> Self {
        fs::create_dir(root.path().join("documents")).unwrap();
        fs::write(root.path().join("欢迎.txt"), "Hello SFTP · 中文\n").unwrap();
        fs::write(
            root.path().join("documents/readme.txt"),
            "Isolated test fixture\n",
        )
        .unwrap();
        fs::write(root.path().join("existing.bin"), b"must remain unchanged").unwrap();
        let host = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let public = host.public_key().to_openssh().unwrap();
        let authentications = Arc::new(AtomicUsize::new(0));
        let connections = Arc::new(AtomicUsize::new(0));
        let (resize_tx, resize) = std::sync::mpsc::channel();
        let (upload_opened_tx, upload_opened) = std::sync::mpsc::channel();
        let closed_subsystems = Arc::new(AtomicUsize::new(0));
        let mut server = Factory {
            root: root.path().to_owned(),
            options,
            authentications: authentications.clone(),
            connections: connections.clone(),
            resize: resize_tx,
            upload_opened: upload_opened_tx,
            closed_subsystems: closed_subsystems.clone(),
        };
        let (stop, receiver) = flume::bounded(1);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let socket = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
                ready_tx.send(socket.local_addr().unwrap().port()).unwrap();
                let config = Arc::new(server::Config { keys: vec![host], auth_rejection_time: Duration::ZERO, auth_rejection_time_initial: Some(Duration::ZERO), ..Default::default() });
                tokio::select! { _ = server.run_on_socket(config, &socket) => {}, _ = receiver.recv_async() => {} }
            });
        });
        let port = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let key = HostKey::from_openssh("127.0.0.1", port, &public).unwrap();
        Self {
            root,
            port,
            key,
            authentications,
            connections,
            resize,
            upload_opened,
            closed_subsystems,
            stop,
            thread: Some(thread),
        }
    }
    pub fn config(&self) -> SshConfig {
        SshConfig {
            host: "127.0.0.1".into(),
            port: self.port,
            username: USER.into(),
            auth: SshAuth::Password(PASSWORD.into()),
        }
    }
    pub fn trusted(&self) -> KnownHosts {
        let mut known = KnownHosts::memory();
        known.trust(self.key.clone()).unwrap();
        known
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

#[derive(Clone)]
struct Factory {
    root: PathBuf,
    options: Options,
    authentications: Arc<AtomicUsize>,
    connections: Arc<AtomicUsize>,
    resize: std::sync::mpsc::Sender<(u32, u32)>,
    upload_opened: std::sync::mpsc::Sender<String>,
    closed_subsystems: Arc<AtomicUsize>,
}
impl server::Server for Factory {
    type Handler = SshHandler;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self::Handler {
        self.connections.fetch_add(1, Ordering::SeqCst);
        SshHandler {
            factory: self.clone(),
            channels: HashMap::new(),
            shells: HashSet::new(),
        }
    }
}
struct SshHandler {
    factory: Factory,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
    shells: HashSet<ChannelId>,
}
impl server::Handler for SshHandler {
    type Error = russh::Error;
    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        self.factory.authentications.fetch_add(1, Ordering::SeqCst);
        Ok(if user == USER && password == PASSWORD {
            server::Auth::Accept
        } else {
            server::Auth::reject()
        })
    }
    async fn channel_open_session(
        &mut self,
        channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
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
        session.channel_success(channel)?;
        Ok(())
    }
    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.shells.insert(channel);
        // Shell input is consumed by Handler::data. Keeping an unused Channel
        // receiver here would eventually fill its bounded queue and stop SSH.
        self.channels.remove(&channel);
        session.channel_success(channel)?;
        session.data(channel, b"SFTP FIXTURE READY\r\n".to_vec())?;
        Ok(())
    }
    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.shells.contains(&channel) {
            if std::env::var_os("SYNCSSH_FIXTURE_COMMANDS").is_some() {
                let command = String::from_utf8_lossy(data);
                if command.contains("SYNCSSH_FIXTURE_DISCONNECT") {
                    session.disconnect(
                        russh::Disconnect::ByApplication,
                        "fixture disconnect",
                        "en",
                    )?;
                    return Ok(());
                }
                if command.contains("SYNCSSH_FIXTURE_EXIT") {
                    session.exit_status_request(channel, 0)?;
                    session.eof(channel)?;
                    session.close(channel)?;
                    return Ok(());
                }
            }
            session.data(channel, data.to_vec())?;
        }
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
        let _ = self.factory.resize.send((cols, rows));
        Ok(())
    }
    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name == "sftp" && self.factory.options.enabled {
            let channel_stream = self.channels.remove(&channel).unwrap();
            session.channel_success(channel)?;
            russh_sftp::server::run(
                channel_stream.into_stream(),
                Filesystem::new(
                    self.factory.root.clone(),
                    self.factory.options.chunk_delay,
                    self.factory.options.open_response_delay,
                    self.factory.upload_opened.clone(),
                ),
            )
            .await;
        } else {
            session.channel_failure(channel)?;
        }
        Ok(())
    }
    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        if !self.shells.remove(&channel) {
            self.factory
                .closed_subsystems
                .fetch_add(1, Ordering::SeqCst);
        }
        self.channels.remove(&channel);
        Ok(())
    }
}

struct Filesystem {
    root: PathBuf,
    files: HashMap<String, fs::File>,
    directories: HashMap<String, Option<Vec<Entry>>>,
    next: u64,
    delay: Duration,
    open_response_delay: Duration,
    upload_opened: std::sync::mpsc::Sender<String>,
}
impl Filesystem {
    fn new(
        root: PathBuf,
        delay: Duration,
        open_response_delay: Duration,
        upload_opened: std::sync::mpsc::Sender<String>,
    ) -> Self {
        Self {
            root,
            files: HashMap::new(),
            directories: HashMap::new(),
            next: 0,
            delay,
            open_response_delay,
            upload_opened,
        }
    }
    fn id(&mut self) -> String {
        self.next += 1;
        self.next.to_string()
    }
    // Test protocol paths are always virtual POSIX paths. Never resolve host absolute paths.
    fn path(&self, path: &str) -> Result<PathBuf, StatusCode> {
        self.resolve(path, false)
    }
    fn leaf_path(&self, path: &str) -> Result<PathBuf, StatusCode> {
        self.resolve(path, true)
    }
    fn resolve(&self, path: &str, allow_leaf_link: bool) -> Result<PathBuf, StatusCode> {
        if path.contains(['\\', '\0', ':']) {
            return Err(StatusCode::PermissionDenied);
        }
        let mut parts = Vec::new();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                other => parts.push(other),
            }
        }
        let mut local = self.root.clone();
        let count = parts.len();
        for (index, part) in parts.into_iter().enumerate() {
            local.push(part);
            if !(allow_leaf_link && index + 1 == count)
                && fs::symlink_metadata(&local).is_ok_and(|m| m.file_type().is_symlink())
            {
                return Err(StatusCode::PermissionDenied);
            }
        }
        Ok(local)
    }
    fn virtual_path(&self, local: &Path) -> String {
        let relative = local
            .strip_prefix(&self.root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        format!("/{relative}")
    }
    fn attr(&self, path: &str) -> Result<FileAttributes, StatusCode> {
        Ok((&fs::metadata(self.path(path)?).map_err(io_error)?).into())
    }
}
fn io_error(error: std::io::Error) -> StatusCode {
    match error.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}
fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: String::new(),
        language_tag: "en".into(),
    }
}
impl russh_sftp::server::Handler for Filesystem {
    type Error = StatusCode;
    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }
    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let path = self.path(&path)?;
        fs::metadata(&path).map_err(io_error)?;
        Ok(Name {
            id,
            files: vec![Entry::dummy(self.virtual_path(&path))],
        })
    }
    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        Ok(Attrs {
            id,
            attrs: self.attr(&path)?,
        })
    }
    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        Ok(Attrs {
            id,
            attrs: (&fs::symlink_metadata(self.leaf_path(&path)?).map_err(io_error)?).into(),
        })
    }
    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let file = self.files.get(&handle).ok_or(StatusCode::Failure)?;
        Ok(Attrs {
            id,
            attrs: (&file.metadata().map_err(io_error)?).into(),
        })
    }
    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let files = fs::read_dir(self.path(&path)?)
            .map_err(io_error)?
            .map(|entry| {
                let entry = entry.map_err(io_error)?;
                Ok(Entry::new(
                    entry
                        .file_name()
                        .into_string()
                        .map_err(|_| StatusCode::Failure)?,
                    (&fs::symlink_metadata(entry.path()).map_err(io_error)?).into(),
                ))
            })
            .collect::<Result<Vec<_>, StatusCode>>()?;
        let handle = self.id();
        self.directories.insert(handle.clone(), Some(files));
        Ok(Handle { id, handle })
    }
    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let files = self
            .directories
            .get_mut(&handle)
            .ok_or(StatusCode::Failure)?
            .take()
            .ok_or(StatusCode::Eof)?;
        if files.is_empty() {
            return Err(StatusCode::Eof);
        }
        Ok(Name { id, files })
    }
    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let file = fs::OpenOptions::from(pflags)
            .open(self.path(&filename)?)
            .map_err(io_error)?;
        let handle = self.id();
        self.files.insert(handle.clone(), file);
        if pflags.contains(OpenFlags::CREATE) {
            let _ = self.upload_opened.send(filename);
            if !self.open_response_delay.is_zero() {
                // The file already exists, but the client has not received its handle yet.
                tokio::time::sleep(self.open_response_delay).await;
            }
        }
        Ok(Handle { id, handle })
    }
    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.files.remove(&handle);
        self.directories.remove(&handle);
        Ok(ok(id))
    }
    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let file = self.files.get_mut(&handle).ok_or(StatusCode::Failure)?;
        file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        let mut data = vec![0; (len as usize).min(64 * 1024)];
        let count = file.read(&mut data).map_err(io_error)?;
        if count == 0 {
            return Err(StatusCode::Eof);
        }
        data.truncate(count);
        Ok(Data { id, data })
    }
    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let file = self.files.get_mut(&handle).ok_or(StatusCode::Failure)?;
        file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        file.write_all(&data).map_err(io_error)?;
        Ok(ok(id))
    }
    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        fs::remove_file(self.leaf_path(&filename)?).map_err(io_error)?;
        Ok(ok(id))
    }
    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let path = self.path(&path)?;
        fs::create_dir(&path).map_err(io_error)?;
        #[cfg(unix)]
        if let Some(mode) = attrs.permissions {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))
                .map_err(io_error)?;
        }
        Ok(ok(id))
    }
    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        fs::remove_dir(self.path(&path)?).map_err(io_error)?;
        Ok(ok(id))
    }
    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let path = self.path(&path)?;
        #[cfg(unix)]
        if let Some(mode) = attrs.permissions {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(mode & 0o7777))
                .map_err(io_error)?;
        }
        if let Some(size) = attrs.size {
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(io_error)?
                .set_len(size)
                .map_err(io_error)?;
        }
        Ok(ok(id))
    }
    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let source = self.leaf_path(&oldpath)?;
        let target = self.leaf_path(&newpath)?;
        if fs::symlink_metadata(&target).is_ok() {
            return Err(StatusCode::Failure);
        }
        if fs::symlink_metadata(&source).map_err(io_error)?.is_dir() {
            fs::rename(source, target).map_err(io_error)?;
        } else {
            // File publication has race-free no-clobber semantics.
            fs::hard_link(&source, &target).map_err(io_error)?;
            fs::remove_file(source).map_err(io_error)?;
        }
        Ok(ok(id))
    }
}
