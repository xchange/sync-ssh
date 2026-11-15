//! Reclaim the terminal after cancelling a ZMODEM peer. Streaming data may
//! already be in flight, but shell output after the peer's cancellation must
//! survive. Standard ZMODEM escapes data CAN bytes, making five consecutive
//! raw CAN bytes an unambiguous cancellation marker.

use flume::Receiver;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const CAN: u8 = 0x18;
const BACKSPACE: u8 = 0x08;
const MAX_FALLBACK: usize = 128 * 1024;

/// Call after sending the local cancellation sequence. `pending` contains only
/// unconsumed input, and `raw` is the same input queue used during the transfer.
/// Returned bytes belong back in the ordinary terminal-output path.
pub(super) fn recover(pending: &[u8], raw: &Receiver<Vec<u8>>, closed: &AtomicBool) -> Vec<u8> {
    let mut parser = Recovery::default();
    if let Some(text) = parser.feed(pending) {
        return text;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut last_input = Instant::now();
    while !closed.load(Ordering::Acquire) && Instant::now() < deadline {
        match raw.recv_timeout(Duration::from_millis(25)) {
            Ok(bytes) => {
                last_input = Instant::now();
                if let Some(text) = parser.feed(&bytes) {
                    return text;
                }
            }
            Err(flume::RecvTimeoutError::Disconnected) => break,
            Err(flume::RecvTimeoutError::Timeout) => {
                if parser.cancel_seen && last_input.elapsed() >= Duration::from_millis(100) {
                    break;
                }
            }
        }
    }
    // Some peers exit without acknowledging cancellation. Preserve their
    // bounded output tail instead of silently dropping a shell prompt or error.
    // In this uncommon fallback, residual protocol bytes may also be displayed.
    parser.fallback
}

#[derive(Default)]
struct Recovery {
    fallback: Vec<u8>,
    consecutive_can: usize,
    cancel_seen: bool,
    backspaces: usize,
}

impl Recovery {
    fn feed(&mut self, bytes: &[u8]) -> Option<Vec<u8>> {
        if !self.cancel_seen {
            self.fallback.extend_from_slice(bytes);
            if self.fallback.len() > MAX_FALLBACK {
                self.fallback.drain(..self.fallback.len() - MAX_FALLBACK);
            }
        }
        for (index, &byte) in bytes.iter().enumerate() {
            if !self.cancel_seen {
                self.consecutive_can = if byte == CAN {
                    self.consecutive_can + 1
                } else {
                    0
                };
                if self.consecutive_can >= 5 {
                    self.cancel_seen = true;
                    self.fallback.clear();
                }
                continue;
            }
            if self.backspaces == 0 && byte == CAN {
                continue;
            }
            if byte == BACKSPACE && self.backspaces < 10 {
                self.backspaces += 1;
                continue;
            }
            return Some(bytes[index..].to_vec());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_marker_handles_every_split_without_losing_shell_tail() {
        let mut input = b"queued binary\x00\xff\x18\x58".to_vec();
        input.extend_from_slice(&[CAN; 10]);
        input.extend_from_slice(&[BACKSPACE; 10]);
        let expected = b"\r\nSYNCSSH shell prompt\x1b[0m";
        input.extend_from_slice(expected);
        for split in 0..=input.len() {
            let mut parser = Recovery::default();
            let mut received = if let Some(text) = parser.feed(&input[..split]) {
                text
            } else {
                parser.feed(&input[split..]).unwrap_or_default()
            };
            // If the first chunk already supplied terminal text, the ordinary
            // router receives the rest unchanged after recovery returns.
            if split > input.len() - expected.len() {
                received.extend_from_slice(&input[split..]);
            }
            assert_eq!(received, expected, "split {split}");
        }
    }

    #[test]
    fn non_cancellation_can_bytes_do_not_discard_output() {
        let mut parser = Recovery::default();
        assert!(parser.feed(b"text\x18\x18\x18\x18x").is_none());
        assert!(parser.feed(b"\r\nready").is_none());
        assert!(!parser.cancel_seen);
        assert_eq!(parser.fallback, b"text\x18\x18\x18\x18x\r\nready");
    }

    #[test]
    fn unacknowledged_output_is_bounded_and_retains_its_tail() {
        let mut parser = Recovery::default();
        assert!(parser.feed(&vec![b'x'; MAX_FALLBACK * 2]).is_none());
        assert!(parser.feed(b"shell tail").is_none());
        assert_eq!(parser.fallback.len(), MAX_FALLBACK);
        assert!(parser.fallback.ends_with(b"shell tail"));
    }

    #[test]
    fn queued_shell_output_is_returned_and_later_packets_remain_queued() {
        let (tx, rx) = flume::unbounded();
        tx.send([vec![CAN; 10], vec![BACKSPACE; 10], b"prompt".to_vec()].concat())
            .unwrap();
        tx.send(b"later output".to_vec()).unwrap();
        assert_eq!(
            recover(b"old binary", &rx, &AtomicBool::new(false)),
            b"prompt"
        );
        assert_eq!(rx.try_recv().unwrap(), b"later output");
    }
}
