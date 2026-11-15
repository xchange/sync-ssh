#[path = "support/sftp_server.rs"]
mod support;
use std::{
    fs,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use support::{Fixture, Options};
use syncssh_session::{connect_ssh, ResumeStore, SftpError, TransferJob};
fn token() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}
fn bytes() -> Vec<u8> {
    (0..2 * 1024 * 1024 + 17).map(|i| (i % 251) as u8).collect()
}
fn partial(root: &std::path::Path, prefix: &str) -> std::path::PathBuf {
    fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with(prefix))
        .expect("owned partial exists")
}
#[test]
fn explicit_upload_and_download_resume_after_reconnect_verify_prefix_and_never_overwrite() {
    let server = Fixture::new(Options {
        chunk_delay: Duration::from_millis(1),
        ..Default::default()
    });
    let terminal = connect_ssh(server.config(), server.trusted(), 80, 24).unwrap();
    let h = terminal.sftp.clone().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("中文源.bin");
    let payload = bytes();
    fs::write(&source, &payload).unwrap();
    let store = ResumeStore::default();
    let up = TransferJob::Upload {
        local: source.clone(),
        remote: "/续传.bin".into(),
    };
    let stop = token();
    let callback = stop.clone();
    assert_eq!(
        h.transfer_resumable(up.clone(), store.clone(), stop, move |p| {
            if p.transferred >= 128 * 1024 {
                callback.store(true, Ordering::Release);
            }
        }),
        Err(SftpError::Cancelled)
    );
    let remote_partial = partial(server.root.path(), ".syncssh-upload-");
    let offset = fs::metadata(&remote_partial).unwrap().len();
    assert!(offset > 0 && offset < payload.len() as u64);
    assert!(!server.root.path().join("续传.bin").exists());
    drop(terminal.guard);
    let terminal = connect_ssh(server.config(), server.trusted(), 80, 24).unwrap();
    let h = terminal.sftp.clone().unwrap();
    let first = Arc::new(AtomicU64::new(u64::MAX));
    let seen = first.clone();
    h.transfer_resumable(up, store.clone(), token(), move |p| {
        let _ = seen.compare_exchange(u64::MAX, p.transferred, Ordering::AcqRel, Ordering::Acquire);
    })
    .unwrap();
    assert_eq!(first.load(Ordering::Acquire), offset);
    assert_eq!(
        fs::read(server.root.path().join("续传.bin")).unwrap(),
        payload
    );
    assert!(!remote_partial.exists());
    let destination = temp.path().join("下载.bin");
    let down = TransferJob::Download {
        remote: "/续传.bin".into(),
        local: destination.clone(),
    };
    let stop = token();
    let callback = stop.clone();
    assert_eq!(
        h.transfer_resumable(down.clone(), store.clone(), stop, move |p| {
            if p.transferred >= 128 * 1024 {
                callback.store(true, Ordering::Release);
            }
        }),
        Err(SftpError::Cancelled)
    );
    let local_partial = partial(temp.path(), ".syncssh-download-");
    let offset = fs::metadata(&local_partial).unwrap().len();
    assert!(offset > 0);
    assert!(!destination.exists());
    // Publication is still no-clobber on a resume attempt.
    fs::write(&destination, b"keep this").unwrap();
    assert_eq!(
        h.transfer_resumable(down.clone(), store.clone(), token(), |_| {}),
        Err(SftpError::AlreadyExists)
    );
    assert_eq!(fs::read(&destination).unwrap(), b"keep this");
    fs::remove_file(&destination).unwrap();
    drop(terminal.guard);
    let terminal = connect_ssh(server.config(), server.trusted(), 80, 24).unwrap();
    let h = terminal.sftp.clone().unwrap();
    let first = Arc::new(AtomicU64::new(u64::MAX));
    let seen = first.clone();
    h.transfer_resumable(down, store, token(), move |p| {
        let _ = seen.compare_exchange(u64::MAX, p.transferred, Ordering::AcqRel, Ordering::Acquire);
    })
    .unwrap();
    assert_eq!(first.load(Ordering::Acquire), offset);
    assert_eq!(fs::read(destination).unwrap(), payload);
    assert!(!local_partial.exists());
}
#[test]
fn changed_source_and_corrupt_partials_are_rejected_without_publishing() {
    let server = Fixture::new(Options::default());
    let terminal = connect_ssh(server.config(), server.trusted(), 80, 24).unwrap();
    let h = terminal.sftp.clone().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source.bin");
    fs::write(&source, bytes()).unwrap();
    let store = ResumeStore::default();
    let up = TransferJob::Upload {
        local: source.clone(),
        remote: "/target.bin".into(),
    };
    let stop = token();
    let callback = stop.clone();
    assert_eq!(
        h.transfer_resumable(up.clone(), store.clone(), stop, move |p| {
            if p.transferred > 0 {
                callback.store(true, Ordering::Release);
            }
        }),
        Err(SftpError::Cancelled)
    );
    let part = partial(server.root.path(), ".syncssh-upload-");
    let original = fs::read(&part).unwrap();
    let mut corrupt = original.clone();
    corrupt[0] ^= 255;
    fs::write(&part, &corrupt).unwrap();
    assert_eq!(
        h.transfer_resumable(up.clone(), store.clone(), token(), |_| {}),
        Err(SftpError::ResumeMismatch)
    );
    assert_eq!(fs::read(&part).unwrap(), corrupt);
    assert!(!server.root.path().join("target.bin").exists());
    fs::write(&part, &original).unwrap();
    fs::write(&source, b"changed source").unwrap();
    assert_eq!(
        h.transfer_resumable(up, store.clone(), token(), |_| {}),
        Err(SftpError::ResumeMismatch)
    );
    fs::write(server.root.path().join("remote.bin"), bytes()).unwrap();
    let destination = temp.path().join("download.bin");
    let down = TransferJob::Download {
        remote: "/remote.bin".into(),
        local: destination.clone(),
    };
    let stop = token();
    let callback = stop.clone();
    assert_eq!(
        h.transfer_resumable(down.clone(), store.clone(), stop, move |p| {
            if p.transferred > 0 {
                callback.store(true, Ordering::Release);
            }
        }),
        Err(SftpError::Cancelled)
    );
    let part = partial(temp.path(), ".syncssh-download-");
    let mut corrupt = fs::read(&part).unwrap();
    corrupt[0] ^= 255;
    fs::write(&part, &corrupt).unwrap();
    assert_eq!(
        h.transfer_resumable(down.clone(), store.clone(), token(), |_| {}),
        Err(SftpError::ResumeMismatch)
    );
    assert!(!destination.exists());
    fs::write(server.root.path().join("remote.bin"), b"remote changed").unwrap();
    assert_eq!(
        h.transfer_resumable(down, store, token(), |_| {}),
        Err(SftpError::ResumeMismatch)
    );
}
#[test]
fn arbitrary_partial_is_not_adopted_and_symlink_is_rejected() {
    let server = Fixture::new(Options::default());
    let terminal = connect_ssh(server.config(), server.trusted(), 80, 24).unwrap();
    let h = terminal.sftp.clone().unwrap();
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        server.root.path().join(".syncssh-upload-unowned.part"),
        b"unowned",
    )
    .unwrap();
    let source = temp.path().join("source");
    fs::write(&source, b"fresh").unwrap();
    h.transfer_resumable(
        TransferJob::Upload {
            local: source,
            remote: "/fresh".into(),
        },
        ResumeStore::default(),
        token(),
        |_| {},
    )
    .unwrap();
    assert_eq!(
        fs::read(server.root.path().join(".syncssh-upload-unowned.part")).unwrap(),
        b"unowned"
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            server.root.path().join("fresh"),
            server.root.path().join("link"),
        )
        .unwrap();
        assert_eq!(
            h.transfer_resumable(
                TransferJob::Download {
                    remote: "/link".into(),
                    local: temp.path().join("link-copy")
                },
                ResumeStore::default(),
                token(),
                |_| {}
            ),
            Err(SftpError::NotRegularFile)
        );
    }
}

#[test]
fn reconnect_during_active_download_serializes_checkpoint_until_old_io_settles() {
    let server = Fixture::new(Options {
        chunk_delay: Duration::from_millis(5),
        ..Default::default()
    });
    fs::write(server.root.path().join("large"), bytes()).unwrap();
    let terminal = connect_ssh(server.config(), server.trusted(), 80, 24).unwrap();
    let h = terminal.sftp.clone().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("after-reconnect");
    let job = TransferJob::Download {
        remote: "/large".into(),
        local: destination.clone(),
    };
    let store = ResumeStore::default();
    let (proceeded, seen) = std::sync::mpsc::sync_channel(1);
    let (done, result) = std::sync::mpsc::channel();
    std::thread::spawn({
        let job = job.clone();
        let store = store.clone();
        move || {
            let result = h.transfer_resumable(job, store, token(), move |p| {
                if p.transferred > 0 {
                    let _ = proceeded.try_send(());
                }
            });
            let _ = done.send(result);
        }
    });
    seen.recv_timeout(Duration::from_secs(5)).unwrap();
    drop(terminal.guard);
    let reconnected = connect_ssh(server.config(), server.trusted(), 80, 24).unwrap();
    let h = reconnected.sftp.unwrap();
    h.transfer_resumable(job, store, token(), |_| {}).unwrap();
    assert!(result
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .is_err());
    assert_eq!(fs::read(destination).unwrap(), bytes());
}

#[test]
fn replaced_local_checkpoint_is_not_modified_published_or_deleted() {
    for hard_link in [false, true] {
        let server = Fixture::new(Options::default());
        let payload = bytes();
        fs::write(server.root.path().join("source"), &payload).unwrap();
        let terminal = connect_ssh(server.config(), server.trusted(), 80, 24).unwrap();
        let h = terminal.sftp.unwrap();
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("destination");
        let job = TransferJob::Download {
            remote: "/source".into(),
            local: destination.clone(),
        };
        let store = ResumeStore::default();
        let stop = token();
        let callback = stop.clone();
        assert_eq!(
            h.transfer_resumable(job.clone(), store.clone(), stop, move |p| {
                if p.transferred > 0 {
                    callback.store(true, Ordering::Release);
                }
            }),
            Err(SftpError::Cancelled)
        );
        let part = partial(temp.path(), ".syncssh-download-");
        let prefix = fs::read(&part).unwrap();
        fs::remove_file(&part).unwrap();
        let victim = temp.path().join("victim");
        fs::write(&victim, &prefix).unwrap();
        if hard_link {
            fs::hard_link(&victim, &part).unwrap();
        } else {
            fs::write(&part, &prefix).unwrap();
        }
        assert_eq!(
            h.transfer_resumable(job, store.clone(), token(), |_| {}),
            Err(SftpError::ResumeMismatch)
        );
        assert_eq!(fs::read(&victim).unwrap(), prefix);
        assert_eq!(fs::read(&part).unwrap(), prefix);
        assert!(!destination.exists());
        drop(store);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            fs::read(&part).unwrap(),
            prefix,
            "dropping rejected checkpoint must preserve replacement"
        );
        assert_eq!(fs::read(victim).unwrap(), prefix);
    }
}
#[test]
fn replaced_remote_checkpoint_hardlink_never_receives_writes() {
    let server = Fixture::new(Options::default());
    let terminal = connect_ssh(server.config(), server.trusted(), 80, 24).unwrap();
    let h = terminal.sftp.unwrap();
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let payload = bytes();
    fs::write(&source, &payload).unwrap();
    let store = ResumeStore::default();
    let job = TransferJob::Upload {
        local: source,
        remote: "/finished".into(),
    };
    let stop = token();
    let callback = stop.clone();
    assert_eq!(
        h.transfer_resumable(job.clone(), store.clone(), stop, move |p| {
            if p.transferred > 0 {
                callback.store(true, Ordering::Release);
            }
        }),
        Err(SftpError::Cancelled)
    );
    let part = partial(server.root.path(), ".syncssh-upload-");
    let prefix = fs::read(&part).unwrap();
    fs::remove_file(&part).unwrap();
    let victim = server.root.path().join("victim");
    fs::write(&victim, &prefix).unwrap();
    fs::hard_link(&victim, &part).unwrap();
    h.transfer_resumable(job, store, token(), |_| {}).unwrap();
    assert_eq!(fs::read(victim).unwrap(), prefix);
    assert_eq!(
        fs::read(server.root.path().join("finished")).unwrap(),
        payload
    );
}
