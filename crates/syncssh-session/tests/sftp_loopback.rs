//! Protocol integration tests use an actual SSH connection and isolated SFTP server.
#[path = "support/sftp_server.rs"]
mod support;
use std::{
    fs,
    io::{Read, Write},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use support::{Fixture, Options};
use syncssh_session::{connect_ssh, RemoteEntryKind, SftpError};

fn bounded<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = work();
        let _ = tx.send(result);
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("operation did not finish within 10 seconds")
}
fn data() -> Vec<u8> {
    (0..2 * 1024 * 1024 + 197)
        .map(|n| (n % 251) as u8)
        .collect()
}
fn never_cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[test]
fn cancelling_while_upload_open_acknowledgement_is_delayed_removes_its_created_file() {
    let server = Fixture::new(Options {
        open_response_delay: Duration::from_millis(400),
        ..Default::default()
    });
    let terminal = connect_ssh(server.config(), server.trusted(), 80, 24).unwrap();
    let sftp = terminal.sftp.clone().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let local = temp.path().join("source.bin");
    fs::write(
        &local,
        b"cancel before the server returns the upload handle",
    )
    .unwrap();
    let cancellation = never_cancel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn({
        let cancellation = cancellation.clone();
        move || {
            let result = sftp.upload(local, "/delayed-open.bin".into(), cancellation, |_| {});
            let _ = done_tx.send(result);
        }
    });
    let temporary = server
        .upload_opened
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    assert!(server
        .root
        .path()
        .join(temporary.trim_start_matches('/'))
        .exists());
    cancellation.store(true, Ordering::Release);
    assert_eq!(
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        Err(SftpError::Cancelled)
    );
    assert!(!server.root.path().join("delayed-open.bin").exists());
    assert!(
        !server
            .root
            .path()
            .join(temporary.trim_start_matches('/'))
            .exists(),
        "cancellation during OPEN must clean the file the server already created"
    );
    drop(terminal.guard);
}

#[test]
fn sftp_reuses_authenticated_ssh_while_terminal_remains_responsive_and_roundtrips_files() {
    let server = Fixture::new(Options {
        chunk_delay: Duration::from_millis(10),
        ..Default::default()
    });
    let mut terminal = connect_ssh(server.config(), server.trusted(), 80, 24).unwrap();
    let sftp = terminal
        .sftp
        .clone()
        .expect("SSH terminal exposes SFTP handle");
    let directory = bounded({
        let sftp = sftp.clone();
        move || sftp.list(".".into())
    })
    .unwrap();
    assert_eq!(directory.path, "/");
    assert!(directory
        .entries
        .iter()
        .any(|e| e.name == "欢迎.txt" && matches!(e.kind, RemoteEntryKind::File)));
    assert!(directory
        .entries
        .iter()
        .any(|e| e.name == "documents" && matches!(e.kind, RemoteEntryKind::Directory)));
    let temp = tempfile::tempdir().unwrap();
    let local = temp.path().join("local.bin");
    let expected = data();
    fs::write(&local, &expected).unwrap();
    let (progress_tx, progress_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let sent = Arc::new(AtomicU64::new(0));
    std::thread::spawn({
        let sftp = sftp.clone();
        let sent = sent.clone();
        move || {
            let result = sftp.upload(local, "/上传.bin".into(), never_cancel(), move |p| {
                sent.store(p.transferred, Ordering::SeqCst);
                if p.transferred > 0 {
                    let _ = progress_tx.send(p.transferred);
                }
            });
            let _ = done_tx.send(result);
        }
    });
    progress_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // Concurrent terminal traffic must not wait for the file channel to finish.
    terminal
        .writer
        .write_all(b"terminal-during-sftp\r\n")
        .unwrap();
    (terminal.resize)(101, 39);
    let mut reader = terminal.reader;
    let shell = bounded(move || {
        let expected = b"SFTP FIXTURE READY\r\nterminal-during-sftp\r\n";
        let mut bytes = vec![0; expected.len()];
        reader.read_exact(&mut bytes).unwrap();
        bytes
    });
    assert_eq!(shell, b"SFTP FIXTURE READY\r\nterminal-during-sftp\r\n");
    assert_eq!(
        server.resize.recv_timeout(Duration::from_secs(3)).unwrap(),
        (101, 39)
    );
    done_rx
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(sent.load(Ordering::SeqCst), expected.len() as u64);
    assert_eq!(
        fs::read(server.root.path().join("上传.bin")).unwrap(),
        expected
    );
    let downloaded = temp.path().join("downloaded.bin");
    bounded({
        let sftp = sftp.clone();
        let downloaded = downloaded.clone();
        move || sftp.download(downloaded, "/上传.bin".into(), never_cancel(), |_| {})
    })
    .unwrap();
    assert_eq!(fs::read(downloaded).unwrap(), expected);
    // Neither an existing remote target nor an existing local file may be clobbered.
    let local = temp.path().join("existing-local.bin");
    fs::write(&local, b"local-preserved").unwrap();
    assert!(bounded({
        let sftp = sftp.clone();
        let local = local.clone();
        move || sftp.upload(local, "/existing.bin".into(), never_cancel(), |_| {})
    })
    .is_err());
    assert_eq!(
        fs::read(server.root.path().join("existing.bin")).unwrap(),
        b"must remain unchanged"
    );
    assert!(bounded({
        let sftp = sftp.clone();
        let local = local.clone();
        move || sftp.download(local, "/上传.bin".into(), never_cancel(), |_| {})
    })
    .is_err());
    assert_eq!(fs::read(local).unwrap(), b"local-preserved");
    assert_eq!(
        server.authentications.load(Ordering::SeqCst),
        1,
        "SFTP must not re-authenticate"
    );
    assert_eq!(
        server.connections.load(Ordering::SeqCst),
        1,
        "SFTP must reuse the existing SSH connection"
    );
    drop(terminal.guard);
}

#[test]
fn cancellation_and_guard_close_do_not_publish_partial_downloads_or_uploads() {
    let server = Fixture::new(Options {
        chunk_delay: Duration::from_millis(15),
        ..Default::default()
    });
    let terminal = connect_ssh(server.config(), server.trusted(), 80, 24).unwrap();
    let sftp = terminal.sftp.clone().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source.bin");
    fs::write(&source, data()).unwrap();
    let cancelled = never_cancel();
    let result = bounded({
        let sftp = sftp.clone();
        let cancelled = cancelled.clone();
        move || {
            let flag = cancelled.clone();
            sftp.upload(source, "/cancelled.bin".into(), cancelled, move |p| {
                if p.transferred >= 65536 {
                    flag.store(true, Ordering::SeqCst);
                }
            })
        }
    });
    assert!(result.is_err());
    assert!(!server.root.path().join("cancelled.bin").exists());
    assert!(
        fs::read_dir(server.root.path()).unwrap().all(|e| {
            let n = e.unwrap().file_name().to_string_lossy().to_string();
            !n.starts_with('.')
        }),
        "cancelled upload must clean temporary remote files"
    );
    fs::write(server.root.path().join("large.bin"), data()).unwrap();
    let destination = temp.path().join("cancelled-download.bin");
    let result = bounded({
        let sftp = sftp.clone();
        let destination = destination.clone();
        move || {
            let cancelled = never_cancel();
            let flag = cancelled.clone();
            sftp.download(destination, "/large.bin".into(), cancelled, move |p| {
                if p.transferred >= 65536 {
                    flag.store(true, Ordering::SeqCst);
                }
            })
        }
    });
    assert!(result.is_err());
    assert!(!destination.exists());
    // Close the terminal guard during an active transfer; detached handles cannot keep it alive.
    let (progress_tx, progress_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let destination = temp.path().join("disconnected-download.bin");
    std::thread::spawn({
        let sftp = sftp.clone();
        let destination = destination.clone();
        move || {
            let result =
                sftp.download(destination, "/large.bin".into(), never_cancel(), move |p| {
                    if p.transferred > 0 {
                        let _ = progress_tx.send(());
                    }
                });
            let _ = done_tx.send(result);
        }
    });
    progress_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let start = Instant::now();
    drop(terminal.guard);
    assert!(done_rx
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .is_err());
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(!destination.exists());
    assert!(sftp.is_closed());
    assert!(bounded(move || sftp.list("/".into())).is_err());
}

#[test]
fn unsupported_sftp_does_not_close_or_reauthenticate_the_terminal() {
    let server = Fixture::new(Options {
        enabled: false,
        ..Default::default()
    });
    let mut terminal = connect_ssh(server.config(), server.trusted(), 80, 24).unwrap();
    let sftp = terminal.sftp.clone().unwrap();
    for expected in 1..=3 {
        let attempt = sftp.clone();
        assert_eq!(
            bounded(move || attempt.list(".".into())).unwrap_err(),
            SftpError::Unsupported
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while server.closed_subsystems.load(Ordering::SeqCst) < expected
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            server.closed_subsystems.load(Ordering::SeqCst),
            expected,
            "an unsupported SFTP handshake must close its session channel before retrying"
        );
    }
    terminal.writer.write_all(b"still-connected\r\n").unwrap();
    let mut reader = terminal.reader;
    let bytes = bounded(move || {
        let mut bytes = vec![0; b"SFTP FIXTURE READY\r\nstill-connected\r\n".len()];
        reader.read_exact(&mut bytes).unwrap();
        bytes
    });
    assert_eq!(bytes, b"SFTP FIXTURE READY\r\nstill-connected\r\n");
    assert_eq!(server.authentications.load(Ordering::SeqCst), 1);
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
    drop(terminal.guard);
}
