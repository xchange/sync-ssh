//! Real lrzsz peers run in an isolated local PTY; no server or user credentials
//! are used. Set SHELL=/bin/sh and SYNCSSH_LRZSZ_DIR to the lrzsz binary directory.
//! Without that variable, rz/sz (or lrz/lsz) are discovered on PATH. These tests
//! are explicitly ignored by default; invoke this test target with --ignored.
//! Missing tools fail an explicitly requested run instead of silently skipping.
#![cfg(unix)]

use std::{
    fs,
    io::{self, Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use syncssh_session::{
    local_shell, with_zmodem, SessionGuard, TerminalIo, ZmodemDirection, ZmodemEvent, ZmodemHandle,
    ZmodemSummary,
};

const DEADLINE: Duration = Duration::from_secs(40);

struct Tools {
    rz: PathBuf,
    sz: PathBuf,
}

fn tools() -> Tools {
    let configured = std::env::var_os("SYNCSSH_LRZSZ_DIR");
    let directories: Vec<PathBuf> = match &configured {
        Some(directory) => {
            let directory = PathBuf::from(directory);
            vec![
                directory.clone(),
                directory.join("bin"),
                directory.join("src"),
            ]
        }
        None => std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect(),
    };
    let find = |names: &[&str]| {
        directories.iter().find_map(|directory| {
            names.iter().map(|name| directory.join(name)).find(|path| {
                fs::metadata(path).is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            })
        })
    };
    match (
        find(&["rz", "lrz", "lrzsz-rz"]),
        find(&["sz", "lsz", "lrzsz-sz"]),
    ) {
        (Some(rz), Some(sz)) => Tools { rz, sz },
        _ if configured.is_some() => {
            panic!("SYNCSSH_LRZSZ_DIR does not contain executable rz/sz or lrz/lsz")
        }
        _ => {
            panic!("lrzsz interoperability requires rz/sz: install lrzsz or set SYNCSSH_LRZSZ_DIR")
        }
    }
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

/// Slow only the raw read side for cancellation tests so even a fast local
/// machine cannot finish the entire file before the test observes progress.
struct PacedReader(Box<dyn Read + Send>);

impl Read for PacedReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let length = bytes.len().min(1024);
        let count = self.0.read(&mut bytes[..length])?;
        if count != 0 {
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(count)
    }
}

struct Terminal {
    writer: Box<dyn Write + Send>,
    _resize: Arc<dyn Fn(u16, u16) + Send + Sync>,
    _guard: SessionGuard,
    control: ZmodemHandle,
    events: flume::Receiver<ZmodemEvent>,
    output: flume::Receiver<Vec<u8>>,
    text: Vec<u8>,
}

impl Terminal {
    fn new(paced: bool) -> Self {
        let mut io = local_shell(100, 30).unwrap();
        if paced {
            io.reader = Box::new(PacedReader(io.reader));
        }
        let (io, control, events) = with_zmodem(io).unwrap();
        let TerminalIo {
            mut reader,
            writer,
            resize,
            guard,
            ..
        } = io;
        let (output_tx, output) = flume::bounded(64);
        std::thread::spawn(move || {
            let mut buffer = [0; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if output_tx.send(buffer[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let mut terminal = Self {
            writer,
            _resize: resize,
            _guard: guard,
            control,
            events,
            output,
            text: Vec::new(),
        };
        // The complete marker does not occur in the echoed command. All later
        // markers therefore prove that the shell actually executed the command.
        terminal.command("stty -echo; PS1=''; export PS1; printf '\\nSYNCSSH_%s\\n' 'READY'");
        terminal.wait_text("SYNCSSH_READY");
        terminal.text.clear();
        terminal
    }

    fn command(&mut self, command: &str) {
        self.writer.write_all(command.as_bytes()).unwrap();
        self.writer.write_all(b"\n").unwrap();
        self.writer.flush().unwrap();
    }

    fn wait_text(&mut self, expected: &str) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self
                .text
                .windows(expected.len())
                .any(|part| part == expected.as_bytes())
            {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "missing shell output {expected:?}: {}",
                self.tail()
            );
            let chunk = self.output.recv_timeout(remaining).unwrap_or_else(|error| {
                panic!(
                    "missing shell output {expected:?} ({error}): {}",
                    self.tail()
                )
            });
            self.text.extend_from_slice(&chunk);
            assert!(
                self.text.len() < 128 * 1024,
                "protocol bytes leaked into terminal output"
            );
        }
    }

    fn tail(&self) -> String {
        String::from_utf8_lossy(&self.text[self.text.len().saturating_sub(4096)..]).into_owned()
    }

    fn requested(&self, expected: ZmodemDirection) -> u64 {
        match self
            .events
            .recv_timeout(DEADLINE)
            .expect("ZMODEM request was not detected")
        {
            ZmodemEvent::Requested { id, direction } => {
                assert_eq!(direction, expected);
                assert!(self.control.is_active());
                id
            }
            event => panic!("unexpected event before request: {event:?}"),
        }
    }

    fn finished(&self, id: u64) -> Result<ZmodemSummary, String> {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let event = self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("ZMODEM transfer did not finish");
            if std::env::var_os("SYNCSSH_ZMODEM_TEST_TRACE").is_some() {
                eprintln!("{event:?}");
            }
            match event {
                ZmodemEvent::Progress {
                    id: current,
                    transferred,
                    total,
                    ..
                } => {
                    assert_eq!(current, id);
                    if let Some(total) = total {
                        assert!(transferred <= total);
                    }
                }
                ZmodemEvent::Finished {
                    id: current,
                    result,
                } => {
                    assert_eq!(current, id);
                    return result;
                }
                event => panic!("unexpected event during transfer: {event:?}"),
            }
        }
    }

    fn assert_shell_usable(&mut self, marker: &str) {
        self.command(&format!("printf '\\nSYNCSSH_%s\\n' '{marker}'"));
        self.wait_text(&format!("SYNCSSH_{marker}"));
    }
}

fn files(directory: &Path) -> Vec<PathBuf> {
    let content = (0..2 * 1024 * 1024 + 197)
        .map(|index| ((index * 31 + index / 251) % 256) as u8)
        .collect::<Vec<_>>();
    let fixtures: [(&str, &[u8]); 3] = [
        ("empty.bin", b""),
        ("中文 文件.txt", "中文与 CRLF\r\n第二行\n".as_bytes()),
        ("binary.bin", &content),
    ];
    fixtures
        .iter()
        .map(|(name, data)| {
            let path = directory.join(name);
            fs::write(&path, data).unwrap();
            path
        })
        .collect()
}

fn assert_received(paths: &[PathBuf], directory: &Path) {
    for path in paths {
        let received = directory.join(path.file_name().unwrap());
        let expected = fs::read(path).unwrap();
        let actual = fs::read(&received).unwrap();
        assert_eq!(
            actual.len(),
            expected.len(),
            "size differs: {}",
            received.display()
        );
        assert!(
            actual == expected,
            "content differs: {}",
            received.display()
        );
    }
    assert_eq!(
        fs::read_dir(directory).unwrap().count(),
        paths.len(),
        "temporary files remain"
    );
}

#[test]
#[ignore = "requires lrzsz; run with SYNCSSH_LRZSZ_DIR or PATH"]
fn lrzsz_downloads_multifile_crc16_and_crc32_and_preserves_shell_tail() {
    let tools = tools();
    for options in ["", "-o"] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let mut paths = files(source.path());
        if options == "-o" {
            // Original lrzsz 0.12.20's CRC16 zsdata uses an unsigned
            // do/while(--length), so a zero-length payload underflows and
            // emits unrelated memory. Keep empty-file coverage in CRC32;
            // this peer cannot provide a valid empty-file CRC16 fixture.
            paths.remove(0);
        }
        let mut terminal = Terminal::new(false);
        let names = paths
            .iter()
            .map(|path| quote(path))
            .collect::<Vec<_>>()
            .join(" ");
        terminal.command(&format!(
            "{} -q {options} {names}; result=$?; printf '\\nSYNCSSH_%s:%s\\n' 'DOWNLOAD_TAIL' \"$result\"",
            quote(&tools.sz),
        ));
        let id = terminal.requested(ZmodemDirection::Download);
        terminal
            .control
            .receive_to(id, destination.path().into())
            .unwrap();
        let summary = terminal.finished(id).unwrap();
        assert_eq!(summary.files, paths.len());
        assert_eq!(summary.skipped, 0);
        assert_eq!(
            summary.bytes,
            paths
                .iter()
                .map(|path| path.metadata().unwrap().len())
                .sum::<u64>()
        );
        assert_received(&paths, destination.path());
        terminal.wait_text("SYNCSSH_DOWNLOAD_TAIL:0");
        terminal.assert_shell_usable("DOWNLOAD_SHELL_OK");
    }
}

#[test]
#[ignore = "requires lrzsz; run with SYNCSSH_LRZSZ_DIR or PATH"]
fn lrzsz_uploads_multifile_binary_empty_and_utf8_files_then_returns_to_shell() {
    let tools = tools();
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let paths = files(source.path());
    let mut terminal = Terminal::new(false);
    terminal.command(&format!(
        "cd {} && {} -q; result=$?; printf '\\nSYNCSSH_%s:%s\\n' 'UPLOAD_TAIL' \"$result\"",
        quote(destination.path()),
        quote(&tools.rz),
    ));
    let id = terminal.requested(ZmodemDirection::Upload);
    terminal.control.send_files(id, paths.clone()).unwrap();
    let summary = terminal.finished(id).unwrap();
    assert_eq!(summary.files, 3);
    assert_eq!(summary.skipped, 0);
    assert_eq!(
        summary.bytes,
        paths
            .iter()
            .map(|path| path.metadata().unwrap().len())
            .sum::<u64>()
    );
    terminal.wait_text("SYNCSSH_UPLOAD_TAIL:0");
    assert_received(&paths, destination.path());
    terminal.assert_shell_usable("UPLOAD_SHELL_OK");
}

#[test]
#[ignore = "requires lrzsz; run with SYNCSSH_LRZSZ_DIR or PATH"]
fn lrzsz_download_skips_existing_file_without_overwrite_and_continues_batch() {
    let tools = tools();
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let existing = source.path().join("existing.txt");
    let next = source.path().join("next.txt");
    fs::write(&existing, b"incoming file must not replace local content").unwrap();
    fs::write(&next, b"the next file must still transfer").unwrap();
    fs::write(
        destination.path().join("existing.txt"),
        b"keep this original",
    )
    .unwrap();
    let mut terminal = Terminal::new(false);
    terminal.command(&format!(
        "{} -q {} {}; printf '\\nSYNCSSH_%s\\n' 'SKIP_TAIL'",
        quote(&tools.sz),
        quote(&existing),
        quote(&next),
    ));
    let id = terminal.requested(ZmodemDirection::Download);
    terminal
        .control
        .receive_to(id, destination.path().into())
        .unwrap();
    let summary = terminal.finished(id).unwrap();
    assert_eq!(summary.files, 1);
    assert_eq!(summary.skipped, 1);
    assert_eq!(summary.bytes, fs::metadata(&next).unwrap().len());
    assert_eq!(
        fs::read(destination.path().join("existing.txt")).unwrap(),
        b"keep this original"
    );
    assert_eq!(
        fs::read(destination.path().join("next.txt")).unwrap(),
        fs::read(next).unwrap()
    );
    assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 2);
    terminal.wait_text("SYNCSSH_SKIP_TAIL");
    terminal.assert_shell_usable("SKIP_SHELL_OK");
}

#[test]
#[ignore = "requires lrzsz; run with SYNCSSH_LRZSZ_DIR or PATH"]
fn lrzsz_upload_reports_remote_skip_and_continues_with_next_file() {
    let tools = tools();
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let existing = source.path().join("existing.txt");
    let next = source.path().join("next.txt");
    // An empty source makes byte counts alone insufficient to distinguish a
    // skipped file from a successfully transferred empty file.
    fs::write(&existing, b"").unwrap();
    fs::write(&next, b"the next upload must succeed").unwrap();
    fs::write(
        destination.path().join("existing.txt"),
        b"keep original remote file",
    )
    .unwrap();
    let mut terminal = Terminal::new(false);
    // -p explicitly asks the peer to protect existing paths. A ZSKIP is not a
    // successful file completion, even though the library shares that event.
    terminal.command(&format!(
        "cd {} && {} -q -p; printf '\\nSYNCSSH_%s\\n' 'UPLOAD_SKIP_TAIL'",
        quote(destination.path()),
        quote(&tools.rz),
    ));
    let id = terminal.requested(ZmodemDirection::Upload);
    terminal
        .control
        .send_files(id, vec![existing, next.clone()])
        .unwrap();
    let summary = terminal.finished(id).unwrap();
    assert_eq!(summary.files, 1);
    assert_eq!(summary.skipped, 1);
    assert_eq!(summary.bytes, fs::metadata(&next).unwrap().len());
    terminal.wait_text("SYNCSSH_UPLOAD_SKIP_TAIL");
    assert_eq!(
        fs::read(destination.path().join("existing.txt")).unwrap(),
        b"keep original remote file"
    );
    assert_eq!(
        fs::read(destination.path().join("next.txt")).unwrap(),
        fs::read(next).unwrap()
    );
    assert_eq!(fs::read_dir(destination.path()).unwrap().count(), 2);
    terminal.assert_shell_usable("UPLOAD_SKIP_SHELL_OK");
}

#[test]
#[ignore = "requires lrzsz; run with SYNCSSH_LRZSZ_DIR or PATH"]
fn lrzsz_waits_for_file_choices_beyond_initial_handshake_timeout() {
    let tools = tools();
    let source = tempfile::tempdir().unwrap();
    let download_directory = tempfile::tempdir().unwrap();
    let upload_directory = tempfile::tempdir().unwrap();
    let path = source.path().join("等待选择.txt");
    fs::write(&path, "No file may be created before selection.\n").unwrap();
    let mut download = Terminal::new(false);
    let mut upload = Terminal::new(false);
    download.command(&format!(
        "{} -q {}; result=$?; printf '\\nSYNCSSH_%s:%s\\n' 'DELAYED_DOWNLOAD' \"$result\"",
        quote(&tools.sz),
        quote(&path),
    ));
    upload.command(&format!(
        "cd {} && {} -q; result=$?; printf '\\nSYNCSSH_%s:%s\\n' 'DELAYED_UPLOAD' \"$result\"",
        quote(upload_directory.path()),
        quote(&tools.rz),
    ));
    let download_id = download.requested(ZmodemDirection::Download);
    let upload_id = upload.requested(ZmodemDirection::Upload);
    // lrzsz's default initial retries expire within about 50 seconds. Merely
    // repeating ZRQINIT every five seconds also fails before this deadline:
    // rz limits those headers to ten. Exercise both choosers concurrently.
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        for (direction, terminal) in [("download", &download), ("upload", &upload)] {
            match terminal.events.try_recv() {
                Err(flume::TryRecvError::Empty) => {}
                other => panic!("{direction} advanced before file selection: {other:?}"),
            }
            assert!(
                terminal.control.is_active(),
                "{direction} request expired while chooser was open: {:?}",
                terminal.events.recv_timeout(Duration::from_millis(100)),
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(fs::read_dir(download_directory.path()).unwrap().count(), 0);
    assert_eq!(fs::read_dir(upload_directory.path()).unwrap().count(), 0);
    download
        .control
        .receive_to(download_id, download_directory.path().into())
        .unwrap();
    upload
        .control
        .send_files(upload_id, vec![path.clone()])
        .unwrap();
    assert_eq!(download.finished(download_id).unwrap().files, 1);
    assert_eq!(upload.finished(upload_id).unwrap().files, 1);
    download.wait_text("SYNCSSH_DELAYED_DOWNLOAD:0");
    upload.wait_text("SYNCSSH_DELAYED_UPLOAD:0");
    assert_received(&[path.clone()], download_directory.path());
    assert_received(&[path], upload_directory.path());
    download.assert_shell_usable("DELAYED_DOWNLOAD_SHELL_OK");
    upload.assert_shell_usable("DELAYED_UPLOAD_SHELL_OK");
}

#[test]
#[ignore = "requires lrzsz; run with SYNCSSH_LRZSZ_DIR or PATH"]
fn lrzsz_cancel_download_removes_partial_file_and_restores_shell() {
    let tools = tools();
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let paths = files(source.path());
    let mut terminal = Terminal::new(true);
    terminal.command(&format!(
        "{} -q {}; result=$?; printf '\\nSYNCSSH_%s:%s\\n' 'CANCEL_TAIL' \"$result\"",
        quote(&tools.sz),
        quote(&paths[2]),
    ));
    let id = terminal.requested(ZmodemDirection::Download);
    terminal
        .control
        .receive_to(id, destination.path().into())
        .unwrap();
    let deadline = Instant::now() + DEADLINE;
    loop {
        match terminal
            .events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            ZmodemEvent::Progress {
                id: current,
                transferred,
                ..
            } => {
                assert_eq!(current, id);
                if transferred > 0 {
                    terminal.control.cancel(id);
                    break;
                }
            }
            event => panic!("transfer ended before it could be cancelled: {event:?}"),
        }
    }
    assert!(
        terminal.finished(id).is_err(),
        "cancelled transfer reported success"
    );
    terminal.wait_text("SYNCSSH_CANCEL_TAIL:");
    assert!(
        !terminal.tail().contains("SYNCSSH_CANCEL_TAIL:0"),
        "sz reported success after cancellation"
    );
    assert_eq!(
        fs::read_dir(destination.path()).unwrap().count(),
        0,
        "partial download remains"
    );
    terminal.assert_shell_usable("CANCEL_SHELL_OK");
}
