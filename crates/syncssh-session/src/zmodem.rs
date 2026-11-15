//! ZMODEM multiplexing over an existing terminal stream. File decisions are
//! explicitly made by the UI; wire bytes and local file I/O stay on workers.
mod protocol;
mod recovery;
mod writer;
use writer::{AbortRequest, GateWriter, WriteRequest};

use crate::{SessionError, SessionGuard, TerminalIo};
use flume::{Receiver, Sender};
use std::{
    io::{self, Read},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const TICK: Duration = Duration::from_millis(25);
const CHUNK: usize = 4096;
const QUEUE: usize = 32;
const CHOICE_TIMEOUT: Duration = Duration::from_secs(120);
const ABORT: &[u8] =
    b"\x18\x18\x18\x18\x18\x18\x18\x18\x18\x18\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZmodemDirection {
    Upload,
    Download,
}

#[derive(Debug, Default)]
pub struct ZmodemSummary {
    pub files: usize,
    pub bytes: u64,
    pub skipped: usize,
}

#[derive(Debug)]
pub enum ZmodemEvent {
    Requested {
        id: u64,
        direction: ZmodemDirection,
    },
    Progress {
        id: u64,
        name: String,
        transferred: u64,
        total: Option<u64>,
    },
    Finished {
        id: u64,
        result: Result<ZmodemSummary, String>,
    },
}

enum Choice {
    Upload(Vec<PathBuf>),
    Download(PathBuf),
}

struct Shared {
    writes: Sender<WriteRequest>,
    aborts: Sender<AbortRequest>,
    active: AtomicU64,
    epoch: AtomicU64,
    cancelled: AtomicU64,
    closed: AtomicBool,
    write_failed: AtomicBool,
}

#[derive(Clone)]
pub struct ZmodemHandle {
    shared: Arc<Shared>,
    commands: Sender<(u64, Choice)>,
}

impl ZmodemHandle {
    pub fn is_active(&self) -> bool {
        self.shared.active.load(Ordering::Acquire) != 0
    }

    pub fn send_files(&self, id: u64, paths: Vec<PathBuf>) -> Result<(), String> {
        if paths.is_empty() || paths.len() > 256 {
            return Err("请选择 1 至 256 个普通文件。".into());
        }
        self.choose(id, Choice::Upload(paths))
    }

    pub fn receive_to(&self, id: u64, directory: PathBuf) -> Result<(), String> {
        self.choose(id, Choice::Download(directory))
    }

    fn choose(&self, id: u64, choice: Choice) -> Result<(), String> {
        if id == 0 || self.shared.active.load(Ordering::Acquire) != id {
            return Err("该 ZMODEM 请求已结束。".into());
        }
        self.commands
            .try_send((id, choice))
            .map_err(|_| "ZMODEM 正忙或连接已关闭。".to_owned())
    }

    pub fn cancel(&self, id: u64) {
        if self.shared.active.load(Ordering::Acquire) == id {
            self.shared.cancelled.store(id, Ordering::Release);
        }
    }
}

/// Wraps a terminal without opening another SSH connection. Closing the returned
/// guard stops transfers and closes the original transport. The caller must
/// continuously drain terminal output and handle the event receiver.
pub fn with_zmodem(
    io: TerminalIo,
) -> Result<(TerminalIo, ZmodemHandle, Receiver<ZmodemEvent>), SessionError> {
    let TerminalIo {
        mut reader,
        writer,
        resize,
        guard,
        sftp,
        tunnels,
        status,
    } = io;
    let (write_tx, write_rx) = flume::bounded(QUEUE);
    let (abort_tx, abort_rx) = flume::bounded(1);
    let shared = Arc::new(Shared {
        writes: write_tx,
        aborts: abort_tx,
        active: AtomicU64::new(0),
        epoch: AtomicU64::new(0),
        cancelled: AtomicU64::new(0),
        closed: AtomicBool::new(false),
        write_failed: AtomicBool::new(false),
    });
    let (raw_tx, raw_rx) = flume::bounded(QUEUE);
    let (output_tx, output_rx) = flume::bounded(QUEUE);
    let (event_tx, event_rx) = flume::bounded(64);
    let (command_tx, command_rx) = flume::bounded(4);
    let write_shared = shared.clone();
    std::thread::Builder::new()
        .name("syncssh-zmodem-write".into())
        .spawn(move || writer::run(writer, write_shared, write_rx, abort_rx))?;
    let read_shared = shared.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("syncssh-zmodem-read".into())
        .spawn(move || {
            let mut buffer = [0; CHUNK];
            while !read_shared.closed.load(Ordering::Acquire) {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if !send_bounded(&raw_tx, buffer[..n].to_vec(), &read_shared) {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
    {
        shared.closed.store(true, Ordering::Release);
        return Err(error.into());
    }
    let worker_shared = shared.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("syncssh-zmodem".into())
        .spawn(move || {
            route(worker_shared, raw_rx, output_tx, event_tx, command_rx);
        })
    {
        shared.closed.store(true, Ordering::Release);
        return Err(error.into());
    }
    let handle = ZmodemHandle {
        shared: shared.clone(),
        commands: command_tx,
    };
    let gate = GateWriter(shared.clone());
    let wrapped_guard = SessionGuard::new(move || {
        shared.closed.store(true, Ordering::Release);
        drop(guard);
    });
    Ok((
        TerminalIo {
            reader: Box::new(OutputReader {
                rx: output_rx,
                pending: io::Cursor::new(Vec::new()),
            }),
            writer: Box::new(gate),
            resize,
            guard: wrapped_guard,
            sftp,
            tunnels,
            status,
        },
        handle,
        event_rx,
    ))
}

struct OutputReader {
    rx: Receiver<Vec<u8>>,
    pending: io::Cursor<Vec<u8>>,
}

impl Read for OutputReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let n = self.pending.read(buffer)?;
            if n != 0 {
                return Ok(n);
            }
            match self.rx.recv() {
                Ok(bytes) => self.pending = io::Cursor::new(bytes),
                Err(_) => return Ok(0),
            }
        }
    }
}

fn send_bounded<T>(tx: &Sender<T>, mut value: T, shared: &Shared) -> bool {
    while !shared.closed.load(Ordering::Acquire) {
        match tx.send_timeout(value, TICK) {
            Ok(()) => return true,
            Err(flume::SendTimeoutError::Timeout(returned)) => value = returned,
            Err(flume::SendTimeoutError::Disconnected(_)) => return false,
        }
    }
    false
}

fn route(
    shared: Arc<Shared>,
    raw: Receiver<Vec<u8>>,
    output: Sender<Vec<u8>>,
    events: Sender<ZmodemEvent>,
    commands: Receiver<(u64, Choice)>,
) {
    let mut detector = Detector::default();
    let mut id: u64 = 0;
    let mut next = None;
    while !shared.closed.load(Ordering::Acquire) {
        let bytes = if let Some(bytes) = next.take() {
            bytes
        } else {
            match raw.recv_timeout(TICK) {
                Ok(bytes) => bytes,
                Err(flume::RecvTimeoutError::Timeout) => {
                    let delay = if detector.pending.starts_with(b"**\x18B") {
                        Duration::from_secs(5)
                    } else {
                        Duration::from_millis(150)
                    };
                    if detector.since.is_some_and(|t| t.elapsed() > delay) {
                        let bytes = detector.flush();
                        if !bytes.is_empty() && !send_bounded(&output, bytes, &shared) {
                            break;
                        }
                    }
                    continue;
                }
                Err(flume::RecvTimeoutError::Disconnected) => {
                    let bytes = detector.flush();
                    if !bytes.is_empty() {
                        send_bounded(&output, bytes, &shared);
                    }
                    break;
                }
            }
        };
        let (plain, found) = detector.feed(&bytes);
        if !plain.is_empty() && !send_bounded(&output, plain, &shared) {
            break;
        }
        let Some((direction, initial)) = found else {
            continue;
        };
        id = id.wrapping_add(1).max(1);
        shared.active.store(id, Ordering::Release);
        shared.epoch.store(id, Ordering::Release);
        while commands.try_recv().is_ok() {}
        if !send_bounded(&events, ZmodemEvent::Requested { id, direction }, &shared) {
            abort_wire(&shared);
            break;
        }
        let mut wire = Wire {
            shared: &shared,
            raw: &raw,
            events: &events,
            id,
            pending: initial,
            offset: 0,
            can_streak: 0,
            peer_cancelled: false,
        };
        wire.peer_cancelled = remote_cancelled(&wire.pending, &mut wire.can_streak);
        let result =
            await_choice(&mut wire, direction, &commands).and_then(|choice| match choice {
                Choice::Upload(paths) => protocol::upload(&mut wire, paths),
                Choice::Download(directory) => protocol::download(&mut wire, directory),
            });
        let failed = result.is_err();
        let tail = if failed {
            shared.cancelled.store(id, Ordering::Release);
            if !wire.peer_cancelled {
                abort_wire(&shared);
            }
            recovery::recover(wire.remaining(), &raw, &shared.closed)
        } else {
            wire.remaining().to_vec()
        };
        shared.active.store(0, Ordering::Release);
        if !send_bounded(&events, ZmodemEvent::Finished { id, result }, &shared) {
            break;
        }
        if failed {
            // Do not reinterpret an unacknowledged peer's buffered handshake as
            // a new transfer immediately after the user cancelled that request.
            if !tail.is_empty() && !send_bounded(&output, tail, &shared) {
                break;
            }
        } else {
            next = Some(tail);
        }
    }
    shared.active.store(0, Ordering::Release);
    shared.closed.store(true, Ordering::Release);
}

fn await_choice(
    wire: &mut Wire<'_>,
    direction: ZmodemDirection,
    commands: &Receiver<(u64, Choice)>,
) -> Result<Choice, String> {
    let deadline = Instant::now() + CHOICE_TIMEOUT;
    protocol::selection_keepalive(wire, direction, true)?;
    let mut last_keepalive = Instant::now();
    loop {
        wire.check()?;
        if Instant::now() >= deadline {
            return Err("等待选择文件超时，已取消 ZMODEM。".into());
        }
        while let Ok((id, choice)) = commands.try_recv() {
            if id == wire.id {
                let choice = match (direction, choice) {
                    (ZmodemDirection::Upload, choice @ Choice::Upload(_))
                    | (ZmodemDirection::Download, choice @ Choice::Download(_)) => choice,
                    _ => return Err("ZMODEM 传输方向不匹配。".into()),
                };
                // Keepalive responses and repeated metadata describe an offer,
                // not permission to create a file. Restart negotiation after
                // the explicit local decision and fetch fresh file metadata.
                wire.pending.clear();
                wire.offset = 0;
                for _ in 0..QUEUE {
                    let Ok(bytes) = wire.raw.try_recv() else {
                        break;
                    };
                    wire.peer_cancelled |= remote_cancelled(&bytes, &mut wire.can_streak);
                }
                wire.check()?;
                return Ok(choice);
            }
        }
        if last_keepalive.elapsed() >= Duration::from_secs(5) {
            protocol::selection_keepalive(wire, direction, false)?;
            last_keepalive = Instant::now();
        }
        match wire.raw.recv_timeout(TICK) {
            Ok(bytes) => {
                wire.peer_cancelled |= remote_cancelled(&bytes, &mut wire.can_streak);
                if wire.pending.len() + bytes.len() > CHUNK * QUEUE {
                    return Err("选择文件前收到过多 ZMODEM 数据，已取消。".into());
                }
                wire.pending.extend_from_slice(&bytes);
            }
            Err(flume::RecvTimeoutError::Timeout) => {}
            Err(flume::RecvTimeoutError::Disconnected) => return Err("连接已关闭。".into()),
        }
    }
}

fn abort_wire(shared: &Shared) {
    let (done, ack) = flume::bounded(1);
    // The independent priority slot remains queued if the OS write is blocked.
    // Once it returns, the writer drops old-generation data and sends CAN first.
    if shared.aborts.try_send(AbortRequest { done }).is_ok() {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !shared.closed.load(Ordering::Acquire) {
            match ack.recv_timeout(TICK) {
                Err(flume::RecvTimeoutError::Timeout) => continue,
                _ => break,
            }
        }
    }
}

fn remote_cancelled(bytes: &[u8], consecutive: &mut u8) -> bool {
    let mut cancelled = false;
    for &byte in bytes {
        if byte == 0x18 {
            *consecutive = consecutive.saturating_add(1);
            cancelled |= *consecutive >= 5;
        } else {
            *consecutive = 0;
        }
    }
    cancelled
}

struct Wire<'a> {
    shared: &'a Shared,
    raw: &'a Receiver<Vec<u8>>,
    events: &'a Sender<ZmodemEvent>,
    id: u64,
    pending: Vec<u8>,
    offset: usize,
    can_streak: u8,
    peer_cancelled: bool,
}

impl Wire<'_> {
    fn check(&self) -> Result<(), String> {
        if self.shared.closed.load(Ordering::Acquire) {
            Err("连接已关闭。".into())
        } else if self.shared.cancelled.load(Ordering::Acquire) == self.id {
            Err("已取消 ZMODEM 传输。".into())
        } else if self.peer_cancelled {
            Err("对端取消了 ZMODEM 传输。".into())
        } else {
            Ok(())
        }
    }

    fn remaining(&self) -> &[u8] {
        &self.pending[self.offset..]
    }

    fn consume(&mut self, n: usize) {
        self.offset += n;
    }

    fn refill(&mut self) -> Result<bool, String> {
        self.check()?;
        if !self.remaining().is_empty() {
            return Ok(true);
        }
        match self.raw.recv_timeout(TICK) {
            Ok(bytes) => {
                self.peer_cancelled |= remote_cancelled(&bytes, &mut self.can_streak);
                self.pending = bytes;
                self.offset = 0;
                self.check()?;
                Ok(true)
            }
            Err(flume::RecvTimeoutError::Timeout) => Ok(false),
            Err(flume::RecvTimeoutError::Disconnected) => Err("连接已关闭。".into()),
        }
    }

    fn write(&self, bytes: &[u8]) -> Result<(), String> {
        for chunk in bytes.chunks(CHUNK) {
            let deadline = Instant::now() + Duration::from_secs(20);
            let (done, ack) = flume::bounded(1);
            let mut request = WriteRequest {
                epoch: self.id,
                protocol: true,
                bytes: chunk.to_vec(),
                done: Some(done),
            };
            loop {
                self.check()?;
                if Instant::now() >= deadline {
                    return Err("ZMODEM 发送超时。".into());
                }
                match self.shared.writes.send_timeout(request, TICK) {
                    Ok(()) => break,
                    Err(flume::SendTimeoutError::Timeout(returned)) => request = returned,
                    Err(flume::SendTimeoutError::Disconnected(_)) => {
                        return Err("连接写入已关闭。".into())
                    }
                }
            }
            loop {
                self.check()?;
                if Instant::now() >= deadline {
                    return Err("ZMODEM 发送超时。".into());
                }
                match ack.recv_timeout(TICK) {
                    Ok(result) => {
                        result.map_err(|error| error.to_string())?;
                        break;
                    }
                    Err(flume::RecvTimeoutError::Timeout) => continue,
                    Err(flume::RecvTimeoutError::Disconnected) => {
                        return Err("连接写入已关闭。".into())
                    }
                }
            }
        }
        Ok(())
    }

    fn progress(&self, name: &str, transferred: u64, total: Option<u64>) {
        // Progress is replaceable; a slow UI cannot grow the queue or stall I/O.
        let _ = self.events.try_send(ZmodemEvent::Progress {
            id: self.id,
            name: name.to_owned(),
            transferred,
            total,
        });
    }
}

#[derive(Default)]
struct Detector {
    pending: Vec<u8>,
    since: Option<Instant>,
}

impl Detector {
    fn flush(&mut self) -> Vec<u8> {
        self.since = None;
        std::mem::take(&mut self.pending)
    }

    fn feed(&mut self, bytes: &[u8]) -> (Vec<u8>, Option<(ZmodemDirection, Vec<u8>)>) {
        const PREFIX: &[u8] = b"**\x18B";
        self.pending.extend_from_slice(bytes);
        let mut plain = Vec::new();
        loop {
            if let Some(start) = self.pending.windows(PREFIX.len()).position(|s| s == PREFIX) {
                plain.extend(self.pending.drain(..start));
                if self.pending.len() < 18 {
                    break;
                }
                if let Some(direction) = initial_header(&self.pending[..18]) {
                    return (plain, Some((direction, self.flush())));
                }
                plain.push(self.pending.remove(0));
                continue;
            }
            let keep = (1..PREFIX.len())
                .rev()
                .find(|&n| self.pending.ends_with(&PREFIX[..n]))
                .unwrap_or(0);
            plain.extend(self.pending.drain(..self.pending.len() - keep));
            break;
        }
        self.since = if self.pending.is_empty() {
            None
        } else {
            Some(Instant::now())
        };
        (plain, None)
    }
}

fn initial_header(bytes: &[u8]) -> Option<ZmodemDirection> {
    let mut decoded = [0; 7];
    for (index, pair) in bytes[4..18].chunks_exact(2).enumerate() {
        decoded[index] =
            (pair[0] as char).to_digit(16)? as u8 * 16 + (pair[1] as char).to_digit(16)? as u8;
    }
    let mut crc: u16 = 0;
    for &byte in &decoded[..5] {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    if crc != u16::from_be_bytes([decoded[5], decoded[6]]) {
        return None;
    }
    match decoded[0] {
        0 => Some(ZmodemDirection::Download),
        1 => Some(ZmodemDirection::Upload),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;

    #[test]
    fn detector_preserves_text_and_handles_every_header_split() {
        let frame = b"**\x18B00000000000000\r\n\x11";
        for split in 0..=frame.len() {
            let mut detector = Detector::default();
            let (plain, found) = detector.feed(b"hello\x1b[31m");
            assert_eq!(plain, b"hello\x1b[31m");
            assert!(found.is_none());
            let (_, first) = detector.feed(&frame[..split]);
            let found = if let Some(found) = first {
                found
            } else {
                detector.feed(&frame[split..]).1.unwrap()
            };
            assert_eq!(found.0, ZmodemDirection::Download);
            assert!(found.1.starts_with(&frame[..18]));
        }
    }

    #[test]
    fn detector_rejects_bad_crc_without_swallowing_text() {
        let input = b"a***\x18B00000000000001\r\nhello**";
        let mut detector = Detector::default();
        let (mut plain, found) = detector.feed(input);
        assert!(found.is_none());
        plain.extend(detector.flush());
        assert_eq!(plain, input);
    }

    #[test]
    fn empty_reads_do_not_wait_for_terminal_output() {
        let (_tx, rx) = flume::bounded(1);
        let mut reader = OutputReader {
            rx,
            pending: io::Cursor::new(Vec::new()),
        };
        assert_eq!(reader.read(&mut []).unwrap(), 0);
    }

    #[test]
    fn remote_cancel_is_detected_across_chunks_but_escaped_data_is_not() {
        let mut streak = 0;
        assert!(!remote_cancelled(b"data\x18\x58\x18\x18", &mut streak));
        assert!(!remote_cancelled(b"\x18\x18", &mut streak));
        assert!(remote_cancelled(b"\x18\x08", &mut streak));
    }

    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn pending_transfer_gates_input_and_cancel_restores_terminal_tail() {
        let (input_tx, input_rx) = flume::unbounded();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let guard_closed = closed.clone();
        let io = TerminalIo {
            reader: Box::new(OutputReader {
                rx: input_rx,
                pending: io::Cursor::new(Vec::new()),
            }),
            writer: Box::new(Capture(captured.clone())),
            resize: Arc::new(|_, _| {}),
            guard: SessionGuard::new(move || {
                guard_closed.store(true, Ordering::Release);
            }),
            sftp: None,
            tunnels: None,
            status: None,
        };
        let (mut io, handle, events) = with_zmodem(io).unwrap();
        input_tx
            .send(b"**\x18B00000000000000\r\n\x11".to_vec())
            .unwrap();
        let id = match events.recv_timeout(Duration::from_secs(2)).unwrap() {
            ZmodemEvent::Requested {
                id,
                direction: ZmodemDirection::Download,
            } => id,
            event => panic!("unexpected {event:?}"),
        };
        assert!(handle.is_active());
        io.writer.write_all(b"do not send").unwrap();
        assert!(!captured
            .lock()
            .unwrap()
            .windows(11)
            .any(|bytes| bytes == b"do not send"));
        handle.cancel(id);
        input_tx.send([ABORT, b"shell tail"].concat()).unwrap();
        match events.recv_timeout(Duration::from_secs(3)).unwrap() {
            ZmodemEvent::Finished {
                id: finished,
                result: Err(_),
            } => assert_eq!(finished, id),
            event => panic!("unexpected {event:?}"),
        }
        assert!(!handle.is_active());
        assert!(handle.receive_to(id, PathBuf::from("/tmp")).is_err());
        let mut tail = [0; 10];
        io.reader.read_exact(&mut tail).unwrap();
        assert_eq!(&tail, b"shell tail");
        io.writer.write_all(b"ordinary input").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !captured.lock().unwrap().ends_with(b"ordinary input") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(captured.lock().unwrap().ends_with(b"ordinary input"));
        drop(io.guard);
        assert!(closed.load(Ordering::Acquire));
    }
}
