#[path = "support/sftp_server.rs"]
mod support;
use std::{
    fs,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use support::{Fixture, Options};
use syncssh_session::{connect_ssh, BatchProgress, RemoteEntryKind, SftpError, TransferJob};
fn cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}
#[test]
fn management_refuses_overwrite_nonempty_delete_and_links() {
    let server = Fixture::new(Options::default());
    let terminal = connect_ssh(server.config(), server.trusted(), 80, 24).unwrap();
    let h = terminal.sftp.clone().unwrap();
    h.mkdir("/中文目录".into(), cancel()).unwrap();
    assert_eq!(
        h.mkdir("/中文目录".into(), cancel()),
        Err(SftpError::AlreadyExists)
    );
    assert_eq!(
        h.rename_no_replace("/欢迎.txt".into(), "/existing.bin".into(), cancel()),
        Err(SftpError::AlreadyExists)
    );
    h.rename_no_replace("/中文目录".into(), "/重命名目录".into(), cancel())
        .unwrap();
    h.rename_no_replace(
        "/欢迎.txt".into(),
        "/重命名目录/新名称.txt".into(),
        cancel(),
    )
    .unwrap();
    h.chmod("/重命名目录/新名称.txt".into(), 0o640, cancel())
        .unwrap();
    #[cfg(unix)]
    assert_eq!(
        h.metadata("/重命名目录/新名称.txt".into(), cancel())
            .unwrap()
            .permissions
            .unwrap()
            & 0o7777,
        0o640
    );
    assert_eq!(
        h.chmod("/重命名目录".into(), 0o10000, cancel()),
        Err(SftpError::InvalidPermissions)
    );
    assert_eq!(
        h.remove_empty_directory("/重命名目录".into(), cancel()),
        Err(SftpError::DirectoryNotEmpty)
    );
    assert_eq!(
        h.remove_file("/重命名目录".into(), cancel()),
        Err(SftpError::NotRegularFile)
    );
    assert_eq!(
        h.mkdir("/../escape".into(), cancel()),
        Err(SftpError::InvalidPath)
    );
    h.remove_file("/重命名目录/新名称.txt".into(), cancel())
        .unwrap();
    h.remove_empty_directory("/重命名目录".into(), cancel())
        .unwrap();
    assert_eq!(
        fs::read(server.root.path().join("existing.bin")).unwrap(),
        b"must remain unchanged"
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            server.root.path().join("existing.bin"),
            server.root.path().join("link"),
        )
        .unwrap();
        assert_eq!(
            h.metadata("/link".into(), cancel()).unwrap().kind,
            RemoteEntryKind::Symlink
        );
        assert_eq!(
            h.chmod("/link".into(), 0o600, cancel()),
            Err(SftpError::NotRegularFile)
        );
        h.remove_file("/link".into(), cancel()).unwrap();
        assert!(server.root.path().join("existing.bin").exists());
    }
}
#[test]
fn recursive_batch_continues_after_conflicts_and_retries_only_failures() {
    let server = Fixture::new(Options::default());
    let terminal = connect_ssh(server.config(), server.trusted(), 80, 24).unwrap();
    let h = terminal.sftp.clone().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let tree = temp.path().join("本地树");
    fs::create_dir_all(tree.join("目录/空目录")).unwrap();
    let data: Vec<_> = (0..2 * 1024 * 1024 + 19).map(|n| (n % 251) as u8).collect();
    fs::write(tree.join("目录/二进制.bin"), &data).unwrap();
    fs::write(tree.join("empty"), []).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(tree.join("目录"), tree.join("link")).unwrap();
    let report = h.transfer_batch(
        vec![
            TransferJob::Upload {
                local: tree.join("empty"),
                remote: "/existing.bin".into(),
            },
            TransferJob::Upload {
                local: tree.clone(),
                remote: "/远端树".into(),
            },
        ],
        cancel(),
        |_| {},
    );
    assert!(report
        .results
        .iter()
        .any(|r| r.result == Err(SftpError::AlreadyExists)));
    #[cfg(unix)]
    assert!(report
        .results
        .iter()
        .any(|r| r.result == Err(SftpError::NotRegularFile)));
    assert_eq!(
        fs::read(server.root.path().join("远端树/目录/二进制.bin")).unwrap(),
        data
    );
    assert!(server.root.path().join("远端树/目录/空目录").is_dir());
    assert!(!server.root.path().join("远端树/link").exists());
    let target = temp.path().join("下载树");
    let report = h.transfer_batch(
        vec![TransferJob::Download {
            remote: "/远端树".into(),
            local: target.clone(),
        }],
        cancel(),
        |_| {},
    );
    assert!(report.failed_jobs().is_empty(), "{report:?}");
    assert_eq!(fs::read(target.join("目录/二进制.bin")).unwrap(), data);
    fs::write(temp.path().join("conflict"), b"original").unwrap();
    let report = h.transfer_batch(
        vec![
            TransferJob::Download {
                remote: "/欢迎.txt".into(),
                local: temp.path().join("conflict"),
            },
            TransferJob::Download {
                remote: "/欢迎.txt".into(),
                local: temp.path().join("continued"),
            },
        ],
        cancel(),
        |_| {},
    );
    assert_eq!(report.failed_jobs().len(), 1);
    assert!(temp.path().join("continued").exists());
    fs::remove_file(temp.path().join("conflict")).unwrap();
    let retry = h.transfer_batch(report.failed_jobs(), cancel(), |_| {});
    assert!(retry.failed_jobs().is_empty());
}
#[test]
fn cancelling_recursive_queue_cleans_partial_and_preserves_retry_jobs() {
    let server = Fixture::new(Options {
        chunk_delay: Duration::from_millis(3),
        ..Default::default()
    });
    let terminal = connect_ssh(server.config(), server.trusted(), 80, 24).unwrap();
    let h = terminal.sftp.clone().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("large");
    fs::write(&file, vec![42; 2 * 1024 * 1024]).unwrap();
    let token = cancel();
    let stop = token.clone();
    let report = h.transfer_batch(
        vec![
            TransferJob::Upload {
                local: file.clone(),
                remote: "/cancelled".into(),
            },
            TransferJob::Upload {
                local: file,
                remote: "/unstarted".into(),
            },
        ],
        token,
        move |p| {
            if matches!(p,BatchProgress::Progress{progress,..}if progress.transferred>0) {
                stop.store(true, Ordering::Release);
            }
        },
    );
    assert!(report.cancelled);
    assert_eq!(report.failed_jobs().len(), 2);
    assert!(!server.root.path().join("cancelled").exists());
    assert!(!server.root.path().join("unstarted").exists());
    assert!(!fs::read_dir(server.root.path()).unwrap().any(|p| p
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".syncssh-upload")));
}

#[test]
fn top_level_batch_download_rejects_portable_reserved_names_before_io() {
    let server = Fixture::new(Options::default());
    let terminal = connect_ssh(server.config(), server.trusted(), 80, 24).unwrap();
    let h = terminal.sftp.unwrap();
    let temp = tempfile::tempdir().unwrap();
    for name in [
        "NUL",
        "CON.txt",
        "file:stream",
        "LPT1",
        "trailing.",
        "trailing ",
    ] {
        let report = h.transfer_batch(
            vec![TransferJob::Download {
                remote: "/欢迎.txt".into(),
                local: temp.path().join(name),
            }],
            cancel(),
            |_| {},
        );
        assert_eq!(
            report.results[0].result,
            Err(SftpError::InvalidPath),
            "{name}"
        );
    }
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
}
