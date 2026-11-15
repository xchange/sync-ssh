use super::{Wire, ZmodemDirection, ZmodemSummary, TICK};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tempfile::NamedTempFile;
use zmodem2::{Action, Event, FileInfo, Position, Receiver, Sender};

const STALL_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
const MAX_FILES: usize = 256;

fn protocol_error(error: zmodem2::Error) -> String {
    format!("ZMODEM 协议错误：{error:?}")
}

/// Keep a peer alive while a native file chooser is open, without accepting a
/// file or requesting file data. The caller sends this immediately and every
/// five seconds, then clears cached handshake replies before starting I/O.
pub(super) fn selection_keepalive(
    wire: &Wire<'_>,
    direction: ZmodemDirection,
    first: bool,
) -> Result<(), String> {
    wire.write(selection_packet(direction, first))
}

fn selection_packet(direction: ZmodemDirection, first: bool) -> &'static [u8] {
    match direction {
        // CANFDX | CANOVIO | CANFC32, with no file accepted (no ZRPOS).
        ZmodemDirection::Download if first => b"**\x18B0100000023be50\r\n\x11",
        // Newer lrzsz counts repeated ZRINITs as rejected offers once its
        // five-second wait expires. ZCRC is the standard pre-file checksum
        // query: asking for just the first byte keeps the sender's offer open
        // without accepting it, streaming its contents, or exhausting retries.
        // A zero count would ask it to hash the whole file, so use one.
        ZmodemDirection::Download => b"**\x18B0d0100000057ce\r\n\x11",
        ZmodemDirection::Upload if first => b"**\x18B00000000000000\r\n\x11",
        // lrzsz stops after ten ZRQINITs. Instead repeat standard ZSINIT with
        // an empty attention string: rz acknowledges it and stays in the same
        // pre-file state, without consuming its handshake retry budget. The
        // payload is NUL + ZCRCW with CRC16 ddcd; NUL is ZDLE quoted.
        ZmodemDirection::Upload => b"**\x18B02000000004483\r\n\x11\x18@\x18k\xdd\xcd\x11",
    }
}

/// Apply one platform-independent filename policy, even on Unix. This avoids
/// restoring a portable download folder into Windows device/stream paths later.
fn safe_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_control() || "/\\:<>\"|?*".contains(c))
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
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) {
        return false;
    }
    for prefix in ["COM", "LPT"] {
        if let Some(suffix) = stem.strip_prefix(prefix) {
            if matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            ) {
                return false;
            }
        }
    }
    true
}

struct Upload {
    file: File,
    name: String,
    size: u64,
    transferred: u64,
    eof_sent: bool,
}

fn validate_upload_size(size: u64) -> Result<(), String> {
    if size > u32::MAX as u64 {
        return Err("ZMODEM 仅支持小于 4 GiB 的普通文件。".into());
    }
    Ok(())
}

impl Upload {
    fn open(path: &Path) -> Result<Self, String> {
        if !path.is_absolute() {
            return Err("上传文件必须使用绝对路径。".into());
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if !safe_name(name) {
            return Err("上传文件名须为 UTF-8，且不能包含路径、控制字符或保留名称。".into());
        }
        let meta = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
        if !meta.is_file() {
            return Err("ZMODEM 仅支持普通文件，不跟随符号链接。".into());
        }
        let file = File::open(path).map_err(|error| error.to_string())?;
        let meta = file.metadata().map_err(|error| error.to_string())?;
        if !meta.is_file() {
            return Err("ZMODEM 仅支持普通文件。".into());
        }
        validate_upload_size(meta.len())?;
        Ok(Self {
            file,
            name: name.to_owned(),
            size: meta.len(),
            transferred: 0,
            eof_sent: false,
        })
    }

    fn offer(&self, sender: &mut Sender) -> Result<(), String> {
        sender
            .start_file(
                FileInfo::new(self.name.as_bytes(), Some(Position::new(self.size as u32)))
                    .with_mode(0o100600),
            )
            .map_err(protocol_error)
    }
}

pub(super) fn upload(wire: &mut Wire<'_>, paths: Vec<PathBuf>) -> Result<ZmodemSummary, String> {
    // Validate every selection before sending the first byte of a file. Keep
    // descriptors open so later path changes cannot swap the selected content.
    let mut files = paths
        .iter()
        .map(|path| Upload::open(path))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter();
    let mut current = files.next().ok_or("没有选择文件。")?;
    let mut sender = Sender::new().map_err(protocol_error)?;
    sender.set_streaming_window(16);
    current.offer(&mut sender)?;
    wire.progress(&current.name, 0, Some(current.size));
    let mut summary = ZmodemSummary::default();
    let mut completed = false;
    let mut last_retry = Instant::now();
    let mut last_progress = Instant::now();
    let mut progress_at = Instant::now();
    let mut file_buf = [0; 8192];
    loop {
        wire.check()?;
        match sender.poll() {
            Action::WriteWire(bytes) => {
                // Observe the library's ZEOF output: unlike FileCompleted this
                // is not emitted when the peer responds with ZSKIP. It also
                // distinguishes a legitimately empty file from an empty skip.
                if bytes.starts_with(b"**\x18B0b")
                    || bytes.starts_with(b"*\x18C\x0b")
                    || bytes.starts_with(b"*\x18C\x18K")
                {
                    current.eof_sent = true;
                }
                let n = bytes.len();
                wire.write(&escape_controls(bytes))?;
                sender.wire_written(n);
            }
            Action::ReadFile { offset, max_len } => {
                let start = offset.get() as u64;
                let remaining = current
                    .size
                    .checked_sub(start)
                    .ok_or("ZMODEM 请求了超出文件大小的位置。")?;
                if remaining == 0 {
                    return Err("ZMODEM 请求了文件末尾之外的数据。".into());
                }
                current
                    .file
                    .seek(SeekFrom::Start(start))
                    .map_err(|error| error.to_string())?;
                let n = max_len.min(file_buf.len()).min(remaining as usize);
                current
                    .file
                    .read_exact(&mut file_buf[..n])
                    .map_err(|error| format!("读取上传文件失败（文件可能已修改）：{error}"))?;
                sender.submit_file(&file_buf[..n]).map_err(protocol_error)?;
                current.transferred = current.transferred.max(start + n as u64);
                last_progress = Instant::now();
                if progress_at.elapsed() >= Duration::from_millis(75) {
                    wire.progress(&current.name, current.transferred, Some(current.size));
                    progress_at = Instant::now();
                }
            }
            Action::Event(Event::FileCompleted) => {
                if current.eof_sent {
                    summary.files += 1;
                    summary.bytes += current.size;
                    wire.progress(&current.name, current.size, Some(current.size));
                } else {
                    summary.skipped += 1;
                }
                last_progress = Instant::now();
                if let Some(next) = files.next() {
                    current = next;
                    current.offer(&mut sender)?;
                    wire.progress(&current.name, 0, Some(current.size));
                } else {
                    sender.finish().map_err(protocol_error)?;
                }
            }
            Action::Event(Event::SessionCompleted) => completed = true,
            Action::Event(Event::Aborted) => return Err("对端取消了 ZMODEM 传输。".into()),
            Action::Idle if completed => {
                strip_hex_trailer(wire);
                return Ok(summary);
            }
            Action::Idle => {
                if wire.refill()? {
                    let consumed = sender
                        .submit_wire(wire.remaining())
                        .map_err(protocol_error)?;
                    wire.consume(consumed);
                    if consumed == 0 {
                        std::thread::sleep(TICK);
                    }
                }
                if last_progress.elapsed() >= STALL_TIMEOUT {
                    return Err("ZMODEM 上传等待对端响应超时。".into());
                }
                if last_retry.elapsed() >= RETRY_INTERVAL {
                    sender.timeout().map_err(protocol_error)?;
                    last_retry = Instant::now();
                }
            }
            _ => return Err("不支持的 ZMODEM 上传状态。".into()),
        }
    }
}

/// zmodem2 performs the minimum ZDLE escaping. Quote the remaining C0/C1
/// controls as well: older rz programs leave terminal IEXTEN enabled, and peers
/// can advertise ESCCTL. Existing ZDLE pairs (headers, terminators and already
/// escaped bytes) are preserved. Hex headers retain their CR/LF/XON trailer.
fn escape_controls(bytes: &[u8]) -> Vec<u8> {
    if bytes.starts_with(b"**\x18B") || bytes == b"OO" {
        return bytes.to_vec();
    }
    let mut escaped = Vec::with_capacity(bytes.len() * 2);
    let mut iter = bytes.iter().copied();
    while let Some(byte) = iter.next() {
        if byte == 0x18 {
            escaped.push(byte);
            if let Some(next) = iter.next() {
                escaped.push(next);
            }
        } else if byte & 0x60 == 0 {
            escaped.extend_from_slice(&[0x18, byte ^ 0x40]);
        } else {
            escaped.push(byte);
        }
    }
    escaped
}

struct Download {
    temp: NamedTempFile,
    destination: PathBuf,
    name: String,
    size: Option<u64>,
    transferred: u64,
}

pub(super) fn download(wire: &mut Wire<'_>, directory: PathBuf) -> Result<ZmodemSummary, String> {
    if !directory.is_absolute() || !directory.is_dir() {
        return Err("请选择存在的本地下载目录。".into());
    }
    let directory = directory
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let mut receiver = Receiver::with_flow_control(0, true).map_err(protocol_error)?;
    receiver.set_manual_file_accept(true);
    let mut current: Option<Download> = None;
    let mut summary = ZmodemSummary::default();
    let mut completed = false;
    let mut last_progress = Instant::now();
    let mut last_retry = Instant::now();
    let mut progress_at = Instant::now();
    loop {
        wire.check()?;
        match receiver.poll() {
            Action::WriteWire(bytes) => {
                let n = bytes.len();
                wire.write(bytes)?;
                receiver.wire_written(n);
            }
            Action::WriteFile(bytes) => {
                let file = current.as_mut().ok_or("未选择下载文件。")?;
                let transferred = file.transferred + bytes.len() as u64;
                if transferred > u32::MAX as u64 || file.size.is_some_and(|size| transferred > size)
                {
                    return Err("ZMODEM 文件数据超过声明的大小。".into());
                }
                file.temp
                    .write_all(bytes)
                    .map_err(|error| error.to_string())?;
                file.transferred = transferred;
                let n = bytes.len();
                receiver.file_written(n).map_err(protocol_error)?;
                last_progress = Instant::now();
                if progress_at.elapsed() >= Duration::from_millis(75) {
                    wire.progress(&file.name, file.transferred, file.size);
                    progress_at = Instant::now();
                }
            }
            Action::Event(Event::FileStarted(info)) => {
                if summary.files + summary.skipped >= MAX_FILES {
                    return Err("一个 ZMODEM 批次最多接收 256 个文件。".into());
                }
                let name = std::str::from_utf8(info.name)
                    .map_err(|_| "远端文件名不是 UTF-8。")?
                    .to_owned();
                if !safe_name(&name) {
                    return Err(
                        "拒绝不安全的远端文件名：名称不能包含路径、控制字符或保留名称。".into(),
                    );
                }
                if info
                    .mode
                    .is_some_and(|mode| mode & 0o170000 != 0 && mode & 0o170000 != 0o100000)
                {
                    return Err("ZMODEM 只接收普通文件。".into());
                }
                let size = info.size.map(|position| position.get() as u64);
                if let Some(file) = current.as_ref() {
                    // A ZRINIT sent while the chooser was open can have one
                    // final ZFILE retransmission still in flight at selection.
                    // Reaffirm the same offer only before any data is written;
                    // never replace an in-progress file or create another temp.
                    if file.transferred == 0 && file.name == name && file.size == size {
                        receiver.accept_file_at(0).map_err(protocol_error)?;
                        last_progress = Instant::now();
                        continue;
                    }
                    return Err("ZMODEM 在文件完成前收到了新文件。".into());
                }
                let destination = directory.join(&name);
                match fs::symlink_metadata(&destination) {
                    Ok(_) => {
                        summary.skipped += 1;
                        receiver.skip_file().map_err(protocol_error)?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        let temp = tempfile::Builder::new()
                            .prefix(".syncssh-zmodem-")
                            .tempfile_in(&directory)
                            .map_err(|error| error.to_string())?;
                        current = Some(Download {
                            temp,
                            destination,
                            name: name.clone(),
                            size,
                            transferred: 0,
                        });
                        receiver.accept_file_at(0).map_err(protocol_error)?;
                        wire.progress(&name, 0, size);
                    }
                    Err(error) => return Err(error.to_string()),
                }
                last_progress = Instant::now();
            }
            Action::Event(Event::FileCompleted) => {
                let file = current.take().ok_or("ZMODEM 文件完成状态不一致。")?;
                if file.size.is_some_and(|size| size != file.transferred) {
                    return Err("ZMODEM 文件大小校验失败。".into());
                }
                file.temp
                    .as_file()
                    .sync_all()
                    .map_err(|error| error.to_string())?;
                file.temp
                    .persist_noclobber(&file.destination)
                    .map_err(|error| error.to_string())?;
                summary.files += 1;
                summary.bytes += file.transferred;
                wire.progress(&file.name, file.transferred, file.size);
                last_progress = Instant::now();
            }
            Action::Event(Event::SessionCompleted) => {
                if current.is_some() {
                    return Err("ZMODEM 在文件完成前结束了会话。".into());
                }
                completed = true;
            }
            Action::Event(Event::Aborted) => return Err("对端取消了 ZMODEM 传输。".into()),
            Action::Idle if completed => {
                // The library exposes SessionCompleted before the queued ZFIN
                // is written. Only now may we consume the peer's final OO and
                // return the untouched shell suffix to the terminal.
                consume_finish(wire)?;
                return Ok(summary);
            }
            Action::Idle => {
                if wire.refill()? {
                    let consumed = receiver
                        .submit_wire(wire.remaining())
                        .map_err(protocol_error)?;
                    wire.consume(consumed);
                    if consumed == 0 {
                        std::thread::sleep(TICK);
                    }
                }
                if last_progress.elapsed() >= STALL_TIMEOUT {
                    return Err("ZMODEM 下载等待对端响应超时。".into());
                }
                if last_retry.elapsed() >= RETRY_INTERVAL {
                    receiver.timeout().map_err(protocol_error)?;
                    last_retry = Instant::now();
                }
            }
            _ => return Err("不支持的 ZMODEM 下载状态。".into()),
        }
    }
}

fn framing(byte: u8) -> bool {
    matches!(byte, 0x0d | 0x0a | 0x8d | 0x8a | 0x11 | 0x13 | 0x91 | 0x93)
}

fn strip_hex_trailer(wire: &mut Wire<'_>) {
    wire.consume(hex_trailer_len(wire.remaining()));
}

fn hex_trailer_len(bytes: &[u8]) -> usize {
    // A hex header owns one CR/LF pair and an optional XON. Any further
    // newlines already belong to the shell, even when delivered in this read.
    let mut length = 0;
    if matches!(bytes.get(length), Some(0x0d | 0x8d)) {
        length += 1;
    }
    if matches!(bytes.get(length), Some(0x0a | 0x8a)) {
        length += 1;
    }
    if matches!(bytes.get(length), Some(0x11 | 0x91)) {
        length += 1;
    }
    length
}

fn consume_finish(wire: &mut Wire<'_>) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut first_o = false;
    loop {
        wire.check()?;
        if !wire.remaining().is_empty() {
            let byte = wire.remaining()[0];
            if !first_o && framing(byte) {
                wire.consume(1);
            } else if byte == b'O' {
                wire.consume(1);
                if first_o {
                    return Ok(());
                }
                first_o = true;
            } else {
                if first_o {
                    let mut rest = vec![b'O'];
                    rest.extend_from_slice(wire.remaining());
                    wire.pending = rest;
                    wire.offset = 0;
                }
                return Ok(());
            }
        } else if Instant::now() >= deadline || wire.raw.is_disconnected() {
            if first_o {
                wire.pending = vec![b'O'];
                wire.offset = 0;
            }
            return Ok(());
        } else {
            // A peer can close immediately after the final file. The received
            // files remain valid even if the optional final OO never arrives.
            if wire.refill().is_err() {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_keepalive_is_valid_and_does_not_offer_a_file() {
        let mut sender = Sender::new().unwrap();
        if let Action::WriteWire(bytes) = sender.poll() {
            assert_eq!(bytes, selection_packet(ZmodemDirection::Upload, true));
        } else {
            panic!("sender has no initial handshake");
        }
        let mut receiver = Receiver::with_flow_control(0, true).unwrap();
        if let Action::WriteWire(bytes) = receiver.poll() {
            assert_eq!(bytes, selection_packet(ZmodemDirection::Download, true));
            let length = bytes.len();
            receiver.wire_written(length);
        } else {
            panic!("receiver has no initial handshake");
        }
        // The library peer must accept a complete, CRC-valid ZSINIT and reply
        // with ZACK. No FileStarted event, file path, or storage is involved.
        let packet = selection_packet(ZmodemDirection::Upload, false);
        assert!(receiver.submit_wire(packet).unwrap() > 0);
        match receiver.poll() {
            Action::WriteWire(bytes) => assert!(bytes.starts_with(b"**\x18B03")),
            other => panic!("expected ZSINIT acknowledgement, got {other:?}"),
        }
    }

    #[test]
    fn hex_trailer_preserves_extra_shell_newlines_and_controls() {
        for (input, expected) in [
            (&b"\r\n\r\n\n$ "[..], &b"\r\n\n$ "[..]),
            (&b"\r\n\x11\n$ "[..], &b"\n$ "[..]),
            (&b"\x8d\x8a\x91\r\n$ "[..], &b"\r\n$ "[..]),
            (&b"\n\n$ "[..], &b"\n$ "[..]),
            (&b"\r\n\x13$ "[..], &b"\x13$ "[..]),
            (&b"$ "[..], &b"$ "[..]),
            (&b""[..], &b""[..]),
        ] {
            assert_eq!(&input[hex_trailer_len(input)..], expected);
        }
    }

    #[test]
    fn download_names_cannot_escape_or_use_windows_special_paths() {
        for name in [
            "",
            ".",
            "..",
            "../escape",
            "/tmp/file",
            "C:\\file",
            "a/b",
            "a\\b",
            "name:stream",
            "CON",
            "con.txt",
            "LPT9.log",
            "COM¹.txt",
            "a\n",
            "a.",
            "a ",
        ] {
            assert!(!safe_name(name), "{name:?}");
        }
        for name in [
            "中文.txt",
            "normal.dat",
            "a b.txt",
            ".hidden",
            "compose.txt",
        ] {
            assert!(safe_name(name), "{name:?}");
        }
    }

    #[test]
    fn outgoing_control_quoting_preserves_zdle_framing_and_hex_trailers() {
        let header = b"**\x18B00000000000000\r\n\x11";
        assert_eq!(escape_controls(header), header);
        assert_eq!(
            escape_controls(b"*\x18C\x04\x00\x16\x18X\x18k"),
            b"*\x18C\x18D\x18@\x18V\x18X\x18k"
        );
        let all_controls: Vec<_> = (0..32).filter(|byte| *byte != 0x18).collect();
        let quoted = escape_controls(&all_controls);
        assert_eq!(quoted.len(), all_controls.len() * 2);
        assert!(quoted
            .chunks_exact(2)
            .zip(&all_controls)
            .all(|(pair, byte)| pair == [0x18, byte ^ 0x40]));
    }

    #[test]
    fn upload_validates_regular_files_and_size_before_transfer() {
        assert!(validate_upload_size(0).is_ok());
        assert!(validate_upload_size(u32::MAX as u64).is_ok());
        assert!(validate_upload_size(u32::MAX as u64 + 1).is_err());
        assert!(validate_upload_size(u64::MAX).is_err());
        let directory = tempfile::tempdir().unwrap();
        assert!(Upload::open(directory.path()).is_err());
        let path = directory.path().join("file.bin");
        File::create(&path).unwrap();
        assert!(Upload::open(&path).is_ok());
        #[cfg(unix)]
        {
            // Unix filesystems support sparse extension here. An ordinary
            // Windows file may reserve the full 4 GiB, so its size boundary is
            // tested above without creating a multi-gigabyte disk allocation.
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(u32::MAX as u64 + 1)
                .unwrap();
            assert!(Upload::open(&path).is_err());
            let link = directory.path().join("link.bin");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(Upload::open(&link).is_err());
        }
    }
}
