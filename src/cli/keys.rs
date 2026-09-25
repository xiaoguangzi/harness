//! Keys read while a turn runs, when the line editor isn't reading: Esc
//! interrupts the turn, like Ctrl-C.

use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use tokio::sync::oneshot;

const ESC: u8 = 0x1b;
/// How often the reader checks whether to stop.
const POLL_MS: libc::c_int = 50;
/// How long after an Esc byte more bytes mean an escape sequence (an arrow
/// key, say) rather than the Esc key.
const SEQUENCE_MS: libc::c_int = 30;

/// Watches the terminal for Esc until dropped. Meanwhile the terminal
/// doesn't echo what is typed, and what is typed is discarded; Ctrl-C still
/// sends SIGINT.
pub struct EscWatch {
    pressed: Option<oneshot::Receiver<()>>,
    reader: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    saved: Option<libc::termios>,
}

impl EscWatch {
    /// Starts watching, if stdin is a terminal; otherwise Esc never comes.
    pub fn start() -> Self {
        let mut watch = Self { pressed: None, reader: None, saved: None };
        if !io::stdin().is_terminal() {
            return watch;
        }
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut saved) } != 0 {
            return watch;
        }
        // Byte at a time and silent, but keep ISIG so Ctrl-C still works.
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return watch;
        }
        watch.saved = Some(saved);

        let (tx, rx) = oneshot::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let stop = stop.clone();
            std::thread::spawn(move || read_keys(&stop, tx))
        };
        watch.pressed = Some(rx);
        watch.reader = Some((stop, reader));
        watch
    }

    /// Resolves when Esc is pressed.
    pub async fn pressed(&mut self) {
        if let Some(rx) = self.pressed.as_mut()
            && rx.await.is_ok()
        {
            return;
        }
        std::future::pending().await
    }
}

impl Drop for EscWatch {
    fn drop(&mut self) {
        // Stop reading before the line editor does.
        if let Some((stop, reader)) = self.reader.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = reader.join();
        }
        if let Some(saved) = &self.saved {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved) };
        }
    }
}

fn read_keys(stop: &AtomicBool, tx: oneshot::Sender<()>) {
    let mut buf = [0u8; 64];
    while !stop.load(Ordering::Relaxed) {
        if !readable(POLL_MS) {
            continue;
        }
        let n = match read(&mut buf) {
            Some(0) => continue,
            Some(n) => n,
            None => return,
        };
        // A lone Esc with nothing right behind it is the key; an escape
        // sequence arrives together, so the rest is readable at once.
        if buf[..n].iter().all(|&b| b == ESC) && !readable(SEQUENCE_MS) {
            let _ = tx.send(());
            return;
        }
    }
}

/// Whether stdin has input (or has hung up) within `timeout_ms`.
fn readable(timeout_ms: libc::c_int) -> bool {
    let mut fd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut fd, 1, timeout_ms) > 0 && fd.revents != 0 }
}

/// Reads what stdin has: `Some(0)` if a signal cut the read short, `None`
/// at its end or on an error.
fn read(buf: &mut [u8]) -> Option<usize> {
    let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
    match n {
        n if n > 0 => Some(n as usize),
        -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => Some(0),
        _ => None,
    }
}
