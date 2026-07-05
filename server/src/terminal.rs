/// Server-side PTY sessions for the terminal forwarder (docs/terminal.md).
///
/// One session per websocket connection. Two plain threads pump the PTY
/// master: a writer draining an mpsc of Data/Resize messages, a reader
/// forwarding output into the connection's direct channel as TermData.
/// Dropping the session closes the input channel, which makes the writer
/// SIGHUP the child; the reader reaps it and reports the exit code.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::protocol::{self, TermOpen};

/// Input to the PTY writer thread.
pub enum TermInput {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

pub struct TermSession {
    input_tx: std::sync::mpsc::Sender<TermInput>,
    exited: Arc<std::sync::atomic::AtomicBool>,
}

impl TermSession {
    pub fn send(&self, input: TermInput) {
        let _ = self.input_tx.send(input);
    }

    /// True once the child has been reaped (set before TermExit is sent, so a
    /// client that saw the exit always observes it).
    pub fn exited(&self) -> bool {
        self.exited.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Open a PTY, spawn `$SHELL` on it, and pump it against `out_tx` (encoded
/// protocol messages for one client's direct channel).
pub fn spawn(open: &TermOpen, out_tx: mpsc::Sender<Vec<u8>>) -> anyhow::Result<TermSession> {
    let cols = open.cols.clamp(2, 500);
    let rows = open.rows.clamp(2, 500);
    let (master, slave) = openpty(cols, rows)?;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let term = if open.term.is_empty() { "xterm-256color" } else { &open.term };

    let mut cmd = Command::new(&shell);
    cmd.env("TERM", term)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The slave is fd 0 after Stdio::from; make it the controlling tty.
            if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    let pid = child.id() as libc::pid_t;
    tracing::info!("[term] spawned {} (pid {}) at {}x{}", shell, pid, cols, rows);

    // The Child stays in the slot until the reader reaps it, so the writer's
    // SIGHUP can never hit a recycled pid.
    let child_slot = Arc::new(Mutex::new(Some(child)));
    let master = Arc::new(master);

    let (input_tx, input_rx) = std::sync::mpsc::channel::<TermInput>();
    let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_exited = exited.clone();

    let writer_master = master.clone();
    let writer_slot = child_slot.clone();
    std::thread::spawn(move || {
        let fd = writer_master.as_raw_fd();
        while let Ok(msg) = input_rx.recv() {
            match msg {
                TermInput::Data(bytes) => {
                    if write_all(fd, &bytes).is_err() {
                        break;
                    }
                }
                TermInput::Resize { cols, rows } => {
                    let ws = winsize(cols.clamp(2, 500), rows.clamp(2, 500));
                    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
                }
            }
        }
        // Channel closed: the connection is gone. Hang up on the child (a
        // process that ignores SIGHUP keeps running, same as under ssh).
        if let Some(child) = writer_slot.lock().as_ref() {
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGHUP) };
        }
    });

    std::thread::spawn(move || {
        let fd = master.as_raw_fd();
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break; // EOF/EIO once the child (and its slave fds) are gone
            }
            let msg = protocol::encode_term_data(&buf[..n as usize]);
            if out_tx.blocking_send(msg).is_err() {
                break; // client gone
            }
        }
        let Some(mut child) = child_slot.lock().take() else {
            return;
        };
        // If we stopped because the client vanished the child may still be
        // alive; hang up before reaping (harmless if it already exited).
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGHUP) };
        let code = match child.wait() {
            Ok(status) => status.code().unwrap_or(1),
            Err(_) => 1,
        };
        tracing::info!("[term] shell (pid {}) exited with {}", pid, code);
        reader_exited.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = out_tx.blocking_send(protocol::encode_term_exit((code & 0xff) as u8));
    });

    Ok(TermSession { input_tx, exited })
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }
}

fn write_all(fd: libc::c_int, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let n = unsafe { libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        bytes = &bytes[n as usize..];
    }
    Ok(())
}

/// Open a master/slave PTY pair with the given initial size.
fn openpty(cols: u16, rows: u16) -> anyhow::Result<(OwnedFd, OwnedFd)> {
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let master = OwnedFd::from_raw_fd(master);
        if libc::grantpt(master.as_raw_fd()) != 0 || libc::unlockpt(master.as_raw_fd()) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut name = [0u8; 128];
        if libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr() as *mut libc::c_char, name.len())
            != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let slave = libc::open(name.as_ptr() as *const libc::c_char, libc::O_RDWR | libc::O_NOCTTY);
        if slave < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let slave = OwnedFd::from_raw_fd(slave);
        let ws = winsize(cols, rows);
        libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
        Ok((master, slave))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{MSG_TERM_DATA, MSG_TERM_EXIT};
    use std::time::Duration;

    /// Full round trip: spawn a shell, run a command, read its output and
    /// exit code back off the session's output channel.
    #[tokio::test]
    async fn shell_roundtrip() {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let open = TermOpen { cols: 80, rows: 24, term: "dumb".to_string() };
        let session = spawn(&open, tx).expect("pty spawn");
        session.send(TermInput::Data(b"echo term-$((6*7)); exit 3\n".to_vec()));

        let mut out = Vec::new();
        let mut exit = None;
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("shell did not exit in time");
            let Some(msg) = msg else { break };
            match msg[0] {
                MSG_TERM_DATA => out.extend_from_slice(&msg[1..]),
                MSG_TERM_EXIT => {
                    exit = Some(msg[1]);
                    break;
                }
                other => panic!("unexpected message type 0x{other:02x}"),
            }
        }

        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("term-42"), "shell output: {text:?}");
        assert_eq!(exit, Some(3));
    }

    /// Dropping the session hangs up: the shell gets SIGHUP and is reaped.
    #[tokio::test]
    async fn drop_hangs_up() {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let open = TermOpen { cols: 80, rows: 24, term: "dumb".to_string() };
        let session = spawn(&open, tx).expect("pty spawn");
        drop(session);

        let exited = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(msg) = rx.recv().await {
                if msg[0] == MSG_TERM_EXIT {
                    return true;
                }
            }
            false
        })
        .await
        .expect("shell did not exit after hangup");
        assert!(exited);
    }
}
