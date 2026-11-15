//! Bounded recursive transfer queues. Run the synchronous facade on a worker.
use super::*;
use std::{collections::VecDeque, fs};

const MAX_JOBS: usize = 20_000;
const MAX_DEPTH: usize = 64;
struct Expansion {
    pending: VecDeque<(TransferJob, usize)>,
    discovered: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TransferJob {
    Upload { local: PathBuf, remote: String },
    Download { remote: String, local: PathBuf },
}

impl TransferJob {
    pub fn description(&self) -> String {
        match self {
            Self::Upload { local, remote } => format!("上传 {} → {remote}", local.display()),
            Self::Download { remote, local } => format!("下载 {remote} → {}", local.display()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct JobResult {
    pub index: usize,
    pub job: TransferJob,
    pub result: Result<(), SftpError>,
}

#[derive(Clone, Debug)]
pub enum BatchProgress {
    Started {
        index: usize,
        job: TransferJob,
    },
    Progress {
        index: usize,
        progress: TransferProgress,
    },
    Finished(JobResult),
}

#[derive(Clone, Debug, Default)]
pub struct BatchReport {
    pub results: Vec<JobResult>,
    pub cancelled: bool,
}

impl BatchReport {
    pub fn failed_jobs(&self) -> Vec<TransferJob> {
        self.results
            .iter()
            .filter(|item| item.result.is_err())
            .map(|item| item.job.clone())
            .collect()
    }
}

impl SftpHandle {
    /// Transfer files and directory trees in order. Existing directories may
    /// be merged, but files are never overwritten. Failures are per item and
    /// subsequent jobs continue; cancellation leaves unstarted jobs retryable.
    /// Symbolic links and special files are reported as errors, never followed.
    pub fn transfer_batch(
        &self,
        jobs: Vec<TransferJob>,
        cancel: Arc<AtomicBool>,
        progress: impl Fn(BatchProgress) + Send + Sync + 'static,
    ) -> BatchReport {
        self.batch(jobs, cancel, progress, None)
    }

    /// Opt-in: retain only application-owned partial files. A retry verifies the
    /// entire existing prefix before continuing, and never replaces final files.
    pub fn transfer_batch_resumable(
        &self,
        jobs: Vec<TransferJob>,
        cancel: Arc<AtomicBool>,
        checkpoints: ResumeStore,
        progress: impl Fn(BatchProgress) + Send + Sync + 'static,
    ) -> BatchReport {
        self.batch(jobs, cancel, progress, Some(checkpoints))
    }

    fn batch(
        &self,
        jobs: Vec<TransferJob>,
        cancel: Arc<AtomicBool>,
        progress: impl Fn(BatchProgress) + Send + Sync + 'static,
        checkpoints: Option<ResumeStore>,
    ) -> BatchReport {
        let progress = Arc::new(progress);
        let mut report = BatchReport::default();
        let discovered = jobs.len();
        let mut expansion = Expansion {
            pending: jobs.into_iter().map(|job| (job, 0usize)).collect(),
            discovered,
        };
        while let Some((job, depth)) = expansion.pending.pop_front() {
            let index = report.results.len();
            progress(BatchProgress::Started {
                index,
                job: job.clone(),
            });
            let result = if cancel.load(Ordering::Acquire) {
                report.cancelled = true;
                Err(SftpError::Cancelled)
            } else if self.is_closed() {
                Err(SftpError::Closed)
            } else if depth > MAX_DEPTH || expansion.discovered > MAX_JOBS {
                Err(SftpError::TreeLimit)
            } else {
                self.batch_item(
                    &job,
                    depth,
                    &mut expansion,
                    cancel.clone(),
                    checkpoints.clone(),
                    {
                        let progress = progress.clone();
                        move |value| {
                            progress(BatchProgress::Progress {
                                index,
                                progress: value,
                            })
                        }
                    },
                )
            };
            report.cancelled |= matches!(result, Err(SftpError::Cancelled));
            let result = JobResult { index, job, result };
            progress(BatchProgress::Finished(result.clone()));
            report.results.push(result);
        }
        report
    }

    fn batch_item(
        &self,
        job: &TransferJob,
        depth: usize,
        expansion: &mut Expansion,
        cancel: Arc<AtomicBool>,
        checkpoints: Option<ResumeStore>,
        progress: impl Fn(TransferProgress) + Send + 'static,
    ) -> Result<(), SftpError> {
        match job {
            TransferJob::Upload { local, remote } => {
                if !local.is_absolute() {
                    return Err(SftpError::InvalidPath);
                }
                validate_destination(remote)?;
                let metadata = fs::symlink_metadata(local).map_err(local_error)?;
                if metadata.is_file() {
                    if let Some(checkpoints) = checkpoints {
                        return self.transfer_resumable(job.clone(), checkpoints, cancel, progress);
                    }
                    return self.upload(local.clone(), remote.clone(), cancel, progress);
                }
                if !metadata.is_dir() {
                    return Err(SftpError::NotRegularFile);
                }
                if depth >= MAX_DEPTH {
                    return Err(SftpError::TreeLimit);
                }
                let mut children = Vec::new();
                for entry in fs::read_dir(local).map_err(local_error)? {
                    if cancel.load(Ordering::Acquire) {
                        return Err(SftpError::Cancelled);
                    }
                    let entry = entry.map_err(local_error)?;
                    let name = entry
                        .file_name()
                        .into_string()
                        .map_err(|_| SftpError::InvalidPath)?;
                    if !safe_name(&name) {
                        return Err(SftpError::InvalidPath);
                    }
                    children.push(TransferJob::Upload {
                        local: entry.path(),
                        remote: child_path(remote, &name),
                    });
                    if expansion.discovered + children.len() > MAX_JOBS {
                        return Err(SftpError::TreeLimit);
                    }
                }
                match self.metadata(remote.clone(), cancel.clone()) {
                    Ok(entry) if entry.kind == RemoteEntryKind::Directory => {}
                    Ok(_) => return Err(SftpError::AlreadyExists),
                    Err(SftpError::NotFound) => self.mkdir(remote.clone(), cancel.clone())?,
                    Err(error) => return Err(error),
                }
                children.sort_by_key(TransferJob::description);
                expansion.discovered += children.len();
                // Keep each tree contiguous while retaining the original batch order.
                for child in children.into_iter().rev() {
                    expansion.pending.push_front((child, depth + 1));
                }
                Ok(())
            }
            TransferJob::Download { remote, local } => {
                if !local.is_absolute()
                    || !local
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(portable_name)
                {
                    return Err(SftpError::InvalidPath);
                }
                let entry = self.metadata(remote.clone(), cancel.clone())?;
                if entry.kind == RemoteEntryKind::File {
                    if let Some(checkpoints) = checkpoints {
                        return self.transfer_resumable(job.clone(), checkpoints, cancel, progress);
                    }
                    return self.download(local.clone(), remote.clone(), cancel, progress);
                }
                if entry.kind != RemoteEntryKind::Directory {
                    return Err(SftpError::NotRegularFile);
                }
                if depth >= MAX_DEPTH {
                    return Err(SftpError::TreeLimit);
                }
                let directory = self.list_with_cancel(remote.clone(), cancel.clone())?;
                if expansion.discovered + directory.entries.len() > MAX_JOBS {
                    return Err(SftpError::TreeLimit);
                }
                // Validate portable names before creating a local directory or
                // any descendants; separators never come from a remote name.
                for child in &directory.entries {
                    if !portable_name(&child.name) {
                        return Err(SftpError::InvalidPath);
                    }
                }
                match fs::symlink_metadata(local) {
                    Ok(meta) if meta.is_dir() => {}
                    Ok(_) => return Err(SftpError::AlreadyExists),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        fs::create_dir(local).map_err(local_error)?;
                    }
                    Err(error) => return Err(local_error(error)),
                }
                expansion.discovered += directory.entries.len();
                for child in directory.entries.into_iter().rev() {
                    expansion.pending.push_front((
                        TransferJob::Download {
                            remote: child.path,
                            local: local.join(child.name),
                        },
                        depth + 1,
                    ));
                }
                Ok(())
            }
        }
    }
}

fn portable_name(name: &str) -> bool {
    if !safe_name(name)
        || name.ends_with(['.', ' '])
        || name.contains([':', '<', '>', '"', '|', '?', '*'])
    {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) {
        return false;
    }
    !["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|tail| {
            matches!(
                tail,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}
