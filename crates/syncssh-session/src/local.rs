use crate::{dimensions, SessionError, SessionGuard, TerminalIo};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::sync::{Arc, Mutex};

pub fn local_shell(cols: u16, rows: u16) -> Result<TerminalIo, SessionError> {
    #[cfg(windows)]
    let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
    #[cfg(not(windows))]
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut command = CommandBuilder::new(shell);
    #[cfg(not(windows))]
    command.arg("-l");
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "SyncSSH");
    command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        command.cwd(home);
    }
    spawn(command, cols, rows)
}

fn spawn(command: CommandBuilder, cols: u16, rows: u16) -> Result<TerminalIo, SessionError> {
    let (cols, rows) = dimensions(cols, rows);
    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| SessionError::Transport(error.to_string()))?;
    // Acquire handles before creating a child, so setup failures cannot leak it.
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| SessionError::Transport(error.to_string()))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| SessionError::Transport(error.to_string()))?;
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| SessionError::Transport(error.to_string()))?;
    drop(pair.slave);
    let child = Arc::new(Mutex::new(child));
    let worker_child = child.clone();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    // A single thread owns all wait/kill operations. Never retain a bare PID
    // signaller after reaping: an exited child's PID could be reused.
    if let Err(error) = std::thread::Builder::new()
        .name("syncssh-pty-wait".into())
        .spawn(move || {
            let Ok(mut child) = worker_child.lock() else {
                return;
            };
            loop {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) => {}
                }
                match stop_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        // try_wait above has not reaped a running process, so the
                        // kernel cannot recycle this PID while we terminate it.
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        })
    {
        if let Ok(mut child) = child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        return Err(error.into());
    }
    let master = Arc::new(Mutex::new(pair.master));
    let resize = Arc::new(move |cols, rows| {
        let (cols, rows) = dimensions(cols, rows);
        if let Ok(master) = master.lock() {
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    });
    Ok(TerminalIo {
        sftp: None,
        tunnels: None,
        status: None,
        reader,
        writer,
        resize,
        guard: SessionGuard::new(move || {
            let _ = stop_tx.send(());
        }),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::Duration;

    #[test]
    fn closing_guard_terminates_and_reaps_running_shell() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.arg("-c");
        command.arg("exec sleep 60");
        let TerminalIo {
            mut reader,
            writer,
            resize,
            guard,
            ..
        } = spawn(command, 80, 24).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = reader.read_to_end(&mut bytes);
            let _ = tx.send(());
        });
        drop(guard);
        // Keep the master, writer, and resize handles alive to prove closure
        // comes from terminating the child, rather than merely closing the PTY.
        rx.recv_timeout(Duration::from_secs(3))
            .expect("shell was not terminated");
        drop((writer, resize));
    }

    #[test]
    fn real_pty_runs_shell_and_reports_resized_dimensions() {
        let mut command = CommandBuilder::new("/bin/sh");
        command.arg("-c");
        command.arg("read answer; stty size; printf 'SYNCSSH_PTY_%s\\n' \"$answer\"");
        let TerminalIo {
            mut reader,
            mut writer,
            resize,
            guard,
            ..
        } = spawn(command, 80, 24).unwrap();
        resize(97, 31);
        writer.write_all(b"SMOKE\n").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            // Linux PTYs can report EIO at EOF; accumulated bytes still count.
            let _ = reader.read_to_end(&mut bytes);
            let _ = tx.send(String::from_utf8_lossy(&bytes).into_owned());
        });
        let output = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("PTY smoke timed out");
        assert!(output.contains("SYNCSSH_PTY_SMOKE"), "{output}");
        assert!(output.contains("31 97"), "{output}");
        drop(guard);
    }
}
