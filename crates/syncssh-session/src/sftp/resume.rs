//! Explicit, in-memory checkpoints. Arbitrary .part files are never adopted.
//! Resume validates size/mtime and compares every existing byte before writing.
use super::*;
use std::{collections::HashMap, time::SystemTime};
use tokio::io::AsyncSeekExt;

type Slot = Arc<tokio::sync::Mutex<Option<Checkpoint>>>;
#[derive(Default)]
struct Store {
    jobs: Mutex<HashMap<TransferJob, Slot>>,
}
impl Drop for Store {
    fn drop(&mut self) {
        // TempPath cleanup performs disk I/O, so the last UI owner does not do it.
        if let Ok(jobs) = self.jobs.get_mut() {
            let jobs = std::mem::take(jobs);
            if !jobs.is_empty() {
                std::thread::spawn(move || drop(jobs));
            }
        }
    }
}
/// Keep this with the queue across reconnects to the same server/account.
/// Checkpoints are intentionally not restored from untrusted disk metadata.
/// Local partials are cleaned when the store is dropped. Remote partials retain
/// their private `.syncssh-upload-*.part` names until resumed or removed by the user.
#[derive(Clone, Default)]
pub struct ResumeStore {
    inner: Arc<Store>,
}
impl ResumeStore {
    fn slot(&self, job: &TransferJob) -> Slot {
        self.inner
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(job.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
            .clone()
    }
}
struct OwnedPartial {
    file: tempfile::NamedTempFile,
}
impl OwnedPartial {
    fn new(mut file: tempfile::NamedTempFile) -> Self {
        // A cleaner or user can replace the name while a transfer is paused.
        // Default TempPath Drop would then delete a file we do not own.
        file.disable_cleanup(true);
        Self { file }
    }
    fn path(&self) -> &Path {
        self.file.path()
    }
    fn matches_path(&self) -> bool {
        let Ok(meta) = std::fs::symlink_metadata(self.path()) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        let same = (|| {
            let original = same_file::Handle::from_file(self.file.as_file().try_clone()?)?;
            let current = same_file::Handle::from_path(self.path())?;
            Ok::<_, std::io::Error>(original == current)
        })();
        same.unwrap_or(false)
    }
    fn reopen(&self) -> std::io::Result<std::fs::File> {
        if !self.matches_path() {
            return Err(std::io::Error::other("checkpoint file replaced"));
        }
        self.file.reopen()
    }
}
impl Drop for OwnedPartial {
    fn drop(&mut self) {
        if self.matches_path() {
            let _ = std::fs::remove_file(self.path());
        }
    }
}
enum Checkpoint {
    Upload {
        temporary: String,
        size: u64,
        modified: SystemTime,
    },
    Download {
        temporary: Arc<OwnedPartial>,
        size: u64,
        modified: u32,
    },
}
impl SftpHandle {
    /// An explicit opt-in to retain partial data on cancellation/disconnection.
    pub fn transfer_resumable(
        &self,
        job: TransferJob,
        checkpoints: ResumeStore,
        cancel: Arc<AtomicBool>,
        progress: impl Fn(TransferProgress) + Send + 'static,
    ) -> Result<(), SftpError> {
        let (reply, receiver) = flume::bounded(1);
        self.submit(RequestKind::Resume {
            job,
            checkpoints,
            cancel,
            progress: Box::new(progress),
            reply,
        })?;
        self.wait(receiver, None)
    }
}
pub(super) async fn transfer(
    session: &RawSftpSession,
    job: TransferJob,
    store: ResumeStore,
    progress: Progress,
    control: &Control<'_>,
) -> Result<(), SftpError> {
    let slot = store.slot(&job);
    // A reconnect may deliver a new worker before the previous channel closes.
    // Serialize access to its checkpoint until all outstanding local I/O settles.
    let mut checkpoint = control.run(async { Ok(slot.lock().await) }).await?;
    let result = match &job {
        TransferJob::Upload { local, remote } => {
            upload_resume(session, local, remote, &mut checkpoint, progress, control).await
        }
        TransferJob::Download { local, remote } => {
            download_resume(session, local, remote, &mut checkpoint, progress, control).await
        }
    };
    if result.is_ok() {
        checkpoint.take();
    }
    let empty = checkpoint.is_none();
    drop(checkpoint);
    if empty {
        let mut jobs = store.inner.jobs.lock().unwrap_or_else(|e| e.into_inner());
        // map + this call only; a waiting attempt must keep the same slot.
        if Arc::strong_count(&slot) == 2 {
            jobs.remove(&job);
        }
    }
    result
}
async fn remote_read(
    session: &RawSftpSession,
    handle: &str,
    offset: u64,
    length: usize,
    control: &Control<'_>,
) -> Result<Vec<u8>, SftpError> {
    control
        .run(async {
            match session.read(handle, offset, length as u32).await {
                Ok(data) if !data.data.is_empty() && data.data.len() <= length => Ok(data.data),
                Ok(_) => Err(SftpError::Protocol),
                Err(ProtocolError::Status(s)) if s.status_code == StatusCode::Eof => {
                    Err(SftpError::ResumeMismatch)
                }
                Err(e) => Err(protocol_error(e)),
            }
        })
        .await
}
async fn local_stamp(
    file: &tokio::fs::File,
    control: &Control<'_>,
) -> Result<(u64, SystemTime), SftpError> {
    let meta = control
        .run(async { file.metadata().await.map_err(local_error) })
        .await?;
    if !meta.is_file() {
        return Err(SftpError::NotRegularFile);
    }
    Ok((meta.len(), meta.modified().map_err(local_error)?))
}
fn remote_stamp(attrs: &FileAttributes) -> Result<(u64, u32), SftpError> {
    if kind(attrs) != RemoteEntryKind::File {
        return Err(SftpError::NotRegularFile);
    }
    Ok((
        attrs.size.ok_or(SftpError::ResumeMetadata)?,
        attrs.mtime.ok_or(SftpError::ResumeMetadata)?,
    ))
}
async fn ensure_local_absent(local: &Path, control: &Control<'_>) -> Result<(), SftpError> {
    match control
        .run(async {
            tokio::fs::symlink_metadata(local)
                .await
                .map_err(local_error)
        })
        .await
    {
        Err(SftpError::NotFound) => Ok(()),
        Ok(_) => Err(SftpError::AlreadyExists),
        Err(e) => Err(e),
    }
}
async fn create_upload(
    session: &RawSftpSession,
    remote: &str,
    control: &Control<'_>,
) -> Result<(String, String), SftpError> {
    let (parent, _) = remote.rsplit_once('/').ok_or(SftpError::InvalidPath)?;
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
    let handle = tokio::time::timeout(
        REQUEST_TIMEOUT,
        session.open(
            &temporary,
            OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
            attrs,
        ),
    )
    .await
    .map_err(|_| SftpError::Timeout)?
    .map_err(protocol_error)?
    .handle;
    Ok((handle, temporary))
}
async fn upload_resume(
    session: &RawSftpSession,
    local: &Path,
    remote: &str,
    checkpoint: &mut Option<Checkpoint>,
    progress: Progress,
    control: &Control<'_>,
) -> Result<(), SftpError> {
    managed_path(remote)?;
    if !local.is_absolute() {
        return Err(SftpError::InvalidPath);
    }
    if !control
        .run(async {
            tokio::fs::symlink_metadata(local)
                .await
                .map_err(local_error)
        })
        .await?
        .is_file()
    {
        return Err(SftpError::NotRegularFile);
    }
    let mut source = control
        .run(async { tokio::fs::File::open(local).await.map_err(local_error) })
        .await?;
    let (size, modified) = local_stamp(&source, control).await?;
    absent(session, remote, control).await?;
    let mut prefix = 0u64;
    let mut old_temporary = None;
    let mut buffer = vec![0; CHUNK];
    if let Some(saved) = checkpoint.as_ref() {
        let Checkpoint::Upload {
            temporary,
            size: saved_size,
            modified: saved_modified,
        } = saved
        else {
            return Err(SftpError::ResumeMismatch);
        };
        if (*saved_size, *saved_modified) != (size, modified) {
            return Err(SftpError::ResumeMismatch);
        }
        let attrs = control
            .run(async { session.lstat(temporary).await.map_err(protocol_error) })
            .await?
            .attrs;
        if kind(&attrs) != RemoteEntryKind::File {
            return Err(SftpError::ResumeMismatch);
        }
        // The server has no stable file identity in SFTP v3. Never reopen an old
        // path writable: it may have been substituted with somebody else's hard link.
        let old = control
            .run(async {
                session
                    .open(temporary, OpenFlags::READ, FileAttributes::empty())
                    .await
                    .map_err(protocol_error)
            })
            .await?
            .handle;
        let verified = async {
            let attrs = control
                .run(async { session.fstat(&old).await.map_err(protocol_error) })
                .await?
                .attrs;
            if kind(&attrs) != RemoteEntryKind::File {
                return Err(SftpError::ResumeMismatch);
            }
            prefix = attrs.size.ok_or(SftpError::ResumeMetadata)?;
            if prefix > size {
                return Err(SftpError::ResumeMismatch);
            }
            let mut count = 0;
            while count < prefix {
                let bytes = remote_read(
                    session,
                    &old,
                    count,
                    CHUNK.min((prefix - count) as usize),
                    control,
                )
                .await?;
                control
                    .run(async {
                        source
                            .read_exact(&mut buffer[..bytes.len()])
                            .await
                            .map_err(local_error)
                    })
                    .await?;
                if buffer[..bytes.len()] != bytes {
                    return Err(SftpError::ResumeMismatch);
                }
                count += bytes.len() as u64;
            }
            Ok(())
        }
        .await;
        let _ = tokio::time::timeout(Duration::from_secs(2), session.close(&old)).await;
        verified?;
        old_temporary = Some(temporary.clone());
    }
    let (handle, temporary) = create_upload(session, remote, control).await?;
    *checkpoint = Some(Checkpoint::Upload {
        temporary,
        size,
        modified,
    });
    if let Some(old) = old_temporary {
        // Unlinking the old name cannot modify a possible hard-link target.
        let _ = tokio::time::timeout(Duration::from_secs(2), session.remove(old)).await;
    }
    let progress = Mutex::new(progress);
    let result = async {
        // Rebuild the verified prefix in our fresh exclusively-created file.
        // This is intentionally extra traffic rather than trusting a mutable remote name.
        source
            .seek(std::io::SeekFrom::Start(0))
            .await
            .map_err(local_error)?;
        let mut count = 0u64;
        while count < prefix {
            let length = CHUNK.min((prefix - count) as usize);
            control
                .run(async {
                    source
                        .read_exact(&mut buffer[..length])
                        .await
                        .map_err(local_error)
                })
                .await?;
            control
                .run(async {
                    session
                        .write(&handle, count, buffer[..length].to_vec())
                        .await
                        .map_err(protocol_error)
                })
                .await?;
            count += length as u64;
        }
        report(
            &progress,
            TransferProgress {
                transferred: count,
                total: Some(size),
            },
        );
        while count < size {
            let length = control
                .run(async {
                    source
                        .read(&mut buffer[..CHUNK.min((size - count) as usize)])
                        .await
                        .map_err(local_error)
                })
                .await?;
            if length == 0 {
                return Err(SftpError::FileChanged);
            }
            control
                .run(async {
                    session
                        .write(&handle, count, buffer[..length].to_vec())
                        .await
                        .map_err(protocol_error)
                })
                .await?;
            count += length as u64;
            report(
                &progress,
                TransferProgress {
                    transferred: count,
                    total: Some(size),
                },
            );
        }
        if local_stamp(&source, control).await? != (size, modified) {
            return Err(SftpError::FileChanged);
        }
        let current = control
            .run(async { session.fstat(&handle).await.map_err(protocol_error) })
            .await?
            .attrs;
        if current.size != Some(size) {
            return Err(SftpError::ResumeMismatch);
        }
        control
            .run(async { session.close(&handle).await.map_err(protocol_error) })
            .await?;
        absent(session, remote, control).await?;
        control.check()?;
        let Some(Checkpoint::Upload { temporary, .. }) = checkpoint.as_ref() else {
            unreachable!()
        };
        commit(session.rename(temporary, remote)).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::time::timeout(Duration::from_secs(2), session.close(&handle)).await;
    }
    result
}
async fn download_resume(
    session: &RawSftpSession,
    local: &Path,
    remote: &str,
    checkpoint: &mut Option<Checkpoint>,
    progress: Progress,
    control: &Control<'_>,
) -> Result<(), SftpError> {
    managed_path(remote)?;
    if !local.is_absolute() {
        return Err(SftpError::InvalidPath);
    }
    ensure_local_absent(local, control).await?;
    let attrs = control
        .run(async { session.lstat(remote).await.map_err(protocol_error) })
        .await?
        .attrs;
    let stamp = remote_stamp(&attrs)?;
    let handle = control
        .run(async {
            session
                .open(remote, OpenFlags::READ, FileAttributes::empty())
                .await
                .map_err(protocol_error)
        })
        .await?
        .handle;
    let current = control
        .run(async { session.fstat(&handle).await.map_err(protocol_error) })
        .await?
        .attrs;
    if remote_stamp(&current)? != stamp {
        return Err(SftpError::FileChanged);
    }
    if checkpoint.is_none() {
        let parent = local.parent().ok_or(SftpError::InvalidPath)?.to_owned();
        let temporary = control
            .run(async {
                tokio::task::spawn_blocking(move || {
                    tempfile::Builder::new()
                        .prefix(".syncssh-download-")
                        .suffix(".part")
                        .tempfile_in(parent)
                })
                .await
                .map_err(|_| SftpError::LocalIo)?
                .map_err(local_error)
            })
            .await?;
        *checkpoint = Some(Checkpoint::Download {
            temporary: Arc::new(OwnedPartial::new(temporary)),
            size: stamp.0,
            modified: stamp.1,
        });
    }
    let Some(Checkpoint::Download {
        temporary,
        size,
        modified,
    }) = checkpoint.as_ref()
    else {
        return Err(SftpError::ResumeMismatch);
    };
    if (*size, *modified) != stamp {
        return Err(SftpError::ResumeMismatch);
    }
    let temporary_path: PathBuf = temporary.path().to_path_buf();
    let owned = temporary.clone();
    let file = control
        .run(async {
            tokio::task::spawn_blocking(move || {
                owned.reopen().map_err(|_| SftpError::ResumeMismatch)
            })
            .await
            .map_err(|_| SftpError::LocalIo)?
        })
        .await?;
    let mut target = tokio::fs::File::from_std(file);
    let prefix = target.metadata().await.map_err(local_error)?.len();
    if prefix > *size {
        return Err(SftpError::ResumeMismatch);
    }
    let owned = temporary.clone();
    let size = *size;
    let modified = *modified;
    let progress = Mutex::new(progress);
    let result = async {
        let mut count = 0u64;
        let mut buffer = vec![0; CHUNK];
        while count < prefix {
            let bytes = remote_read(
                session,
                &handle,
                count,
                CHUNK.min((prefix - count) as usize),
                control,
            )
            .await?;
            control
                .run(async {
                    target
                        .read_exact(&mut buffer[..bytes.len()])
                        .await
                        .map_err(local_error)
                })
                .await?;
            if buffer[..bytes.len()] != bytes {
                return Err(SftpError::ResumeMismatch);
            }
            count += bytes.len() as u64;
        }
        target
            .seek(std::io::SeekFrom::Start(count))
            .await
            .map_err(local_error)?;
        report(
            &progress,
            TransferProgress {
                transferred: count,
                total: Some(size),
            },
        );
        while count < size {
            let bytes = remote_read(
                session,
                &handle,
                count,
                CHUNK.min((size - count) as usize),
                control,
            )
            .await?;
            control
                .run(async { target.write_all(&bytes).await.map_err(local_error) })
                .await?;
            count += bytes.len() as u64;
            report(
                &progress,
                TransferProgress {
                    transferred: count,
                    total: Some(size),
                },
            );
        }
        let attrs = control
            .run(async { session.fstat(&handle).await.map_err(protocol_error) })
            .await?
            .attrs;
        if remote_stamp(&attrs)? != (size, modified) {
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
    // Finish any cancelled asynchronous file write before a new attempt sees the checkpoint.
    drop(target.into_std().await);
    if let Err(e) = result {
        let _ = tokio::time::timeout(Duration::from_secs(2), session.close(&handle)).await;
        return Err(e);
    }
    control.check()?;
    let destination = local.to_owned();
    // A hard link publishes atomically with no replacement and keeps the checkpoint on failure.
    tokio::task::spawn_blocking(move || {
        // Reopen checks original file identity before publication.
        let _same_file = owned.reopen().map_err(|_| SftpError::ResumeMismatch)?;
        std::fs::hard_link(temporary_path, destination).map_err(local_error)
    })
    .await
    .map_err(|_| SftpError::LocalIo)?
}
