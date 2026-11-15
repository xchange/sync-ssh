//! A dedicated transport writer keeps OS-level PTY writes off both GPUI and the
//! protocol worker. Requests are bounded and carry an epoch so cancellation can
//! discard stale data before it reaches the shell.
use super::{Shared, ABORT, CHUNK, TICK};
use flume::{Receiver, Sender};
use std::{
    io::{self, Write},
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

pub(super) struct WriteRequest {
    pub epoch: u64,
    pub protocol: bool,
    pub bytes: Vec<u8>,
    pub done: Option<Sender<io::Result<()>>>,
}

pub(super) struct AbortRequest {
    pub done: Sender<io::Result<()>>,
}

pub(super) struct GateWriter(pub Arc<Shared>);

impl Write for GateWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Snapshot before checking ownership. If a transfer starts between the
        // two loads, the old epoch prevents this input from being replayed later.
        let epoch = self.0.epoch.load(Ordering::Acquire);
        if self.0.closed.load(Ordering::Acquire) || self.0.write_failed.load(Ordering::Acquire) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if self.0.active.load(Ordering::Acquire) != 0 {
            // Never queue keystrokes, mouse reports or VT replies to replay into
            // the shell after the transfer. The GUI separately disables input.
            return Ok(bytes.len());
        }
        let n = bytes.len().min(CHUNK);
        if n == 0 {
            return Ok(0);
        }
        self.0
            .writes
            .try_send(WriteRequest {
                epoch,
                protocol: false,
                bytes: bytes[..n].to_vec(),
                done: None,
            })
            .map_err(|error| match error {
                flume::TrySendError::Full(_) => io::Error::from(io::ErrorKind::WouldBlock),
                flume::TrySendError::Disconnected(_) => io::Error::from(io::ErrorKind::BrokenPipe),
            })?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Like SSH's ChannelWriter, foreground success means accepted by a
        // bounded queue. The worker actually flushes every packet. Protocol
        // writes additionally await an acknowledgment of write AND flush.
        if self.0.closed.load(Ordering::Acquire) || self.0.write_failed.load(Ordering::Acquire) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        Ok(())
    }
}

fn request_valid(shared: &Shared, request: &WriteRequest) -> bool {
    if shared.closed.load(Ordering::Acquire) {
        return false;
    }
    if request.protocol {
        shared.active.load(Ordering::Acquire) == request.epoch
            && shared.cancelled.load(Ordering::Acquire) != request.epoch
    } else {
        shared.active.load(Ordering::Acquire) == 0
            && shared.epoch.load(Ordering::Acquire) == request.epoch
    }
}

pub(super) fn run(
    mut writer: Box<dyn Write + Send>,
    shared: Arc<Shared>,
    writes: Receiver<WriteRequest>,
    aborts: Receiver<AbortRequest>,
) {
    while !shared.closed.load(Ordering::Acquire) {
        if let Ok(abort) = aborts.try_recv() {
            if !send_abort(&mut *writer, &shared, abort) {
                break;
            }
            continue;
        }
        let request = match writes.recv_timeout(TICK) {
            Ok(request) => request,
            Err(flume::RecvTimeoutError::Timeout) => continue,
            Err(flume::RecvTimeoutError::Disconnected) => break,
        };
        // An abort arriving while recv was waiting still precedes queued data.
        if let Ok(abort) = aborts.try_recv() {
            if !send_abort(&mut *writer, &shared, abort) {
                break;
            }
        }
        let result = write_packet(&mut *writer, &request.bytes, || {
            request_valid(&shared, &request)
        });
        let fatal = fatal_write(&result);
        if let Some(done) = request.done {
            let _ = done.send(result);
        }
        if fatal {
            shared.write_failed.store(true, Ordering::Release);
            break;
        }
    }
}

fn send_abort(writer: &mut dyn Write, shared: &Shared, abort: AbortRequest) -> bool {
    let result = write_packet(writer, ABORT, || !shared.closed.load(Ordering::Acquire));
    let fatal = fatal_write(&result);
    let _ = abort.done.send(result);
    if fatal {
        shared.write_failed.store(true, Ordering::Release);
    }
    !fatal
}

fn fatal_write(result: &io::Result<()>) -> bool {
    result.as_ref().is_err_and(|error| {
        !matches!(
            error.kind(),
            io::ErrorKind::Interrupted | io::ErrorKind::TimedOut
        )
    })
}

fn write_packet(writer: &mut dyn Write, bytes: &[u8], valid: impl Fn() -> bool) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut offset = 0;
    loop {
        if !valid() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let result = if offset < bytes.len() {
            writer.write(&bytes[offset..]).and_then(|n| {
                if n == 0 {
                    return Err(io::ErrorKind::WriteZero.into());
                }
                offset += n;
                Ok(false)
            })
        } else {
            writer.flush().map(|()| true)
        };
        match result {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zmodem::Wire;
    use std::sync::{
        atomic::{AtomicBool, AtomicU64},
        Mutex,
    };

    struct BlockedWriter {
        entered: Sender<()>,
        release: Receiver<()>,
        captured: Arc<Mutex<Vec<u8>>>,
        first: bool,
    }

    impl Write for BlockedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.first {
                self.first = false;
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
                // Simulate an OS write that made only partial progress when it
                // eventually returned. Its remainder must not be replayed.
                self.captured.lock().unwrap().push(bytes[0]);
                return Ok(1);
            }
            self.captured.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn blocked_os_write_does_not_block_cancel_and_abort_precedes_fresh_input() {
        let (writes, write_rx) = flume::bounded(4);
        let (aborts, abort_rx) = flume::bounded(1);
        let shared = Arc::new(Shared {
            writes,
            aborts,
            active: AtomicU64::new(1),
            epoch: AtomicU64::new(1),
            cancelled: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            write_failed: AtomicBool::new(false),
        });
        let (entered, started) = flume::bounded(1);
        let (release, unblock) = flume::bounded(1);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let writer = BlockedWriter {
            entered,
            release: unblock,
            captured: captured.clone(),
            first: true,
        };
        let actor_shared = shared.clone();
        let actor =
            std::thread::spawn(move || run(Box::new(writer), actor_shared, write_rx, abort_rx));
        let protocol_shared = shared.clone();
        let (done, completed) = flume::bounded(1);
        std::thread::spawn(move || {
            let (_raw_tx, raw) = flume::bounded(1);
            let (events, _event_rx) = flume::bounded(1);
            let wire = Wire {
                shared: &protocol_shared,
                raw: &raw,
                events: &events,
                id: 1,
                pending: Vec::new(),
                offset: 0,
                can_streak: 0,
                peer_cancelled: false,
            };
            done.send(wire.write(b"first packet remainder")).unwrap();
        });
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        shared
            .writes
            .send(WriteRequest {
                epoch: 1,
                protocol: true,
                bytes: b"obsolete queued data".to_vec(),
                done: None,
            })
            .unwrap();
        shared.cancelled.store(1, Ordering::Release);
        assert!(completed
            .recv_timeout(Duration::from_millis(500))
            .unwrap()
            .is_err());
        let (abort_done, abort_ack) = flume::bounded(1);
        shared
            .aborts
            .send(AbortRequest { done: abort_done })
            .unwrap();
        shared.active.store(0, Ordering::Release);
        let before = Instant::now();
        GateWriter(shared.clone())
            .write_all(b"fresh input")
            .unwrap();
        assert!(before.elapsed() < Duration::from_millis(100));
        release.send(()).unwrap();
        abort_ack
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !captured.lock().unwrap().ends_with(b"fresh input") && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            *captured.lock().unwrap(),
            [b"f".as_slice(), ABORT, b"fresh input"].concat()
        );
        shared.closed.store(true, Ordering::Release);
        actor.join().unwrap();
    }
}
