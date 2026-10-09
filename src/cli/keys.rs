//! Keys read while a turn runs, when the line editor isn't reading: Esc
//! interrupts the turn, like Ctrl-C, and the other keys can edit the input
//! box drawn under the status.

use std::io::{self, IsTerminal, Write};
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
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
/// Longest escape sequence taken apart; anything longer is dropped.
const MAX_SEQUENCE: usize = 32;

/// A key that edits the input box.
#[derive(Debug, Clone, PartialEq)]
pub enum Key {
    /// Text typed, or pasted.
    Text(String),
    Enter,
    Tab,
    Esc,
    Eof,
    /// Alt+Enter: a line break in the text.
    Newline,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    WordLeft,
    WordRight,
    /// Ctrl+U: deletes back to the start of the line.
    KillToStart,
    /// Ctrl+K: deletes to the end of the line.
    KillToEnd,
    /// Ctrl+W or Alt+Backspace: deletes the word before the cursor.
    KillWord,
    /// Ctrl+V: pastes an image from the clipboard.
    PasteImage,
    /// Ctrl+O: inspect the full output of tool calls.
    ToolOutput,
    /// A terminal position report, ignored by the owned viewport.
    Position(usize),
}

type OnKey = Box<dyn FnMut(Key) + Send>;

/// Watches the terminal for Esc until dropped. Meanwhile the terminal
/// doesn't echo what is typed; other keys go to a handler, or are
/// discarded. Ctrl-C still sends SIGINT.
pub struct KeyWatch {
    pressed: Option<oneshot::Receiver<()>>,
    reader: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
    saved: Option<libc::termios>,
    /// Whether bracketed paste was turned on, to turn off again.
    paste: bool,
}

impl KeyWatch {
    /// Starts watching for Esc, if stdin is a terminal; otherwise Esc never
    /// comes. Other keys are discarded.
    pub fn start() -> Self {
        Self::watch(None, false)
    }

    /// Hands every key, including Esc, to the frontend on the reading
    /// thread. Pasted text arrives as one `Key::Text`. The frontend decides
    /// whether Esc closes a panel or interrupts a turn.
    pub fn for_prompt(on_key: impl FnMut(Key) + Send + 'static) -> Self {
        Self::watch(Some(Box::new(on_key)), true)
    }

    fn watch(on_key: Option<OnKey>, prompt: bool) -> Self {
        let mut watch = Self { pressed: None, reader: None, saved: None, paste: false };
        if !io::stdin().is_terminal() {
            return watch;
        }
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut saved) } != 0 {
            return watch;
        }
        // Byte at a time and silent, but keep ISIG so Ctrl-C still works.
        let mut raw = saved;
        // IEXTEN would consume Ctrl+O as VDISCARD on macOS instead of
        // delivering it to the output inspector. Keep ISIG for Ctrl+C.
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::IEXTEN);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return watch;
        }
        watch.saved = Some(saved);
        if on_key.is_some() {
            // A pasted line break then doesn't send the text.
            watch.paste = true;
            let _ = io::stdout().write_all(b"\x1b[?2004h").and_then(|()| io::stdout().flush());
        }

        let (tx, rx) = oneshot::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let stop = stop.clone();
            std::thread::spawn(move || read_keys(&stop, tx, on_key, prompt))
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

impl Drop for KeyWatch {
    fn drop(&mut self) {
        // Stop reading before the line editor does.
        if let Some((stop, reader)) = self.reader.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = reader.join();
        }
        if self.paste {
            let _ = io::stdout().write_all(b"\x1b[?2004l").and_then(|()| io::stdout().flush());
        }
        if let Some(saved) = &self.saved {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved) };
        }
    }
}

fn read_keys(stop: &AtomicBool, esc: oneshot::Sender<()>, mut on_key: Option<OnKey>, prompt: bool) {
    let mut buf = [0u8; 4096];
    // Bytes read and not yet taken apart.
    let mut pending: Vec<u8> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        if pending.is_empty() {
            if !readable(POLL_MS) {
                continue;
            }
        } else {
            // What is left is the start of a sequence. The rest comes right
            // behind it; a long paste may take a little longer.
            let pasting = pending.starts_with(PASTE_START);
            if !readable(if pasting { POLL_MS } else { SEQUENCE_MS }) {
                if pasting {
                    continue;
                }
                // A lone Esc with nothing behind it is the key.
                if pending == [ESC] {
                    if prompt {
                        pending.clear();
                        if let Some(on_key) = on_key.as_mut() { on_key(Key::Esc); }
                        continue;
                    }
                    let _ = esc.send(());
                    return;
                }
                pending.clear();
                continue;
            }
        }
        match read(&mut buf) {
            Some(0) => continue,
            Some(n) => pending.extend_from_slice(&buf[..n]),
            None => return,
        }
        loop {
            match parse(&pending) {
                Parsed::Esc => {
                    if prompt {
                        pending.drain(..1);
                        if let Some(on_key) = on_key.as_mut() { on_key(Key::Esc); }
                        continue;
                    }
                    let _ = esc.send(());
                    return;
                }
                Parsed::Key(key, n) => {
                    pending.drain(..n);
                    if let Some(on_key) = on_key.as_mut() {
                        on_key(key);
                    }
                }
                Parsed::Skip(n) => {
                    pending.drain(..n);
                }
                Parsed::Incomplete => break,
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum Parsed {
    /// A key, and how many bytes it took.
    Key(Key, usize),
    /// The Esc key.
    Esc,
    /// Bytes that mean nothing here.
    Skip(usize),
    /// Nothing yet, or the start of a sequence that needs more bytes.
    Incomplete,
}

/// Takes the first key off the front of `buf`.
fn parse(buf: &[u8]) -> Parsed {
    let Some(&first) = buf.first() else { return Parsed::Incomplete };
    let key = match first {
        ESC => return parse_escape(buf),
        b'\r' | b'\n' => Key::Enter,
        0x7f | 0x08 => Key::Backspace,
        0x01 => Key::Home,
        0x05 => Key::End,
        0x02 => Key::Left,
        0x06 => Key::Right,
        0x10 => Key::Up,
        0x0e => Key::Down,
        0x04 => Key::Eof,
        b'\t' => Key::Tab,
        0x15 => Key::KillToStart,
        0x0b => Key::KillToEnd,
        0x17 => Key::KillWord,
        0x16 => Key::PasteImage,
        0x0f => Key::ToolOutput,
        0x00..=0x1f => return Parsed::Skip(1),
        _ => return parse_text(buf),
    };
    Parsed::Key(key, 1)
}

/// A run of text up to the next control byte.
fn parse_text(buf: &[u8]) -> Parsed {
    let end = buf.iter().position(|&b| b < 0x20 || b == 0x7f).unwrap_or(buf.len());
    match std::str::from_utf8(&buf[..end]) {
        Ok(s) => Parsed::Key(Key::Text(s.to_string()), end),
        Err(e) if e.valid_up_to() > 0 => {
            let n = e.valid_up_to();
            Parsed::Key(Key::Text(String::from_utf8_lossy(&buf[..n]).into_owned()), n)
        }
        // A character cut off at the end of what was read.
        Err(e) if e.error_len().is_none() => Parsed::Incomplete,
        Err(_) => Parsed::Skip(1),
    }
}

fn parse_escape(buf: &[u8]) -> Parsed {
    let Some(&second) = buf.get(1) else { return Parsed::Incomplete };
    let key = match second {
        b'[' => return parse_csi(buf),
        b'O' => {
            let Some(&third) = buf.get(2) else { return Parsed::Incomplete };
            let key = match third {
                b'A' => Key::Up,
                b'B' => Key::Down,
                b'C' => Key::Right,
                b'D' => Key::Left,
                b'H' => Key::Home,
                b'F' => Key::End,
                _ => return Parsed::Skip(3),
            };
            return Parsed::Key(key, 3);
        }
        // Esc pressed twice.
        ESC => return Parsed::Esc,
        b'\r' | b'\n' => Key::Newline,
        0x7f | 0x08 => Key::KillWord,
        b'b' => Key::WordLeft,
        b'f' => Key::WordRight,
        _ => return Parsed::Skip(2),
    };
    Parsed::Key(key, 2)
}

/// `ESC [ params final`: cursor keys, and pasted text between markers.
fn parse_csi(buf: &[u8]) -> Parsed {
    let Some(end) = buf.iter().skip(2).position(|b| (0x40..=0x7e).contains(b)).map(|i| i + 2) else {
        return if buf.len() > MAX_SEQUENCE { Parsed::Skip(buf.len()) } else { Parsed::Incomplete };
    };
    let (params, last) = (&buf[2..end], buf[end]);
    if buf[..=end] == *PASTE_START {
        let body = &buf[end + 1..];
        let Some(stop) = body.windows(PASTE_END.len()).position(|w| w == PASTE_END) else {
            return Parsed::Incomplete;
        };
        let text = String::from_utf8_lossy(&body[..stop]).replace("\r\n", "\n").replace('\r', "\n");
        return Parsed::Key(Key::Text(text), end + 1 + stop + PASTE_END.len());
    }
    // With a modifier (`1;5D` is Ctrl+Left), the arrows move by words.
    let modified = params.contains(&b';');
    let key = match (params, last) {
        (b"13", b'u') => Key::Enter,
        (b"13;2" | b"13;3" | b"13;5" | b"13;6", b'u')
        | (b"27;2;13" | b"27;3;13" | b"27;5;13", b'~') => Key::Newline,
        (_, b'A') => Key::Up,
        (_, b'B') => Key::Down,
        (_, b'C') if modified => Key::WordRight,
        (_, b'D') if modified => Key::WordLeft,
        (_, b'C') => Key::Right,
        (_, b'D') => Key::Left,
        (_, b'H') | (b"1" | b"7", b'~') => Key::Home,
        (_, b'F') | (b"4" | b"8", b'~') => Key::End,
        (b"3", b'~') => Key::Delete,
        (b"5", b'~') => Key::PageUp,
        (b"6", b'~') => Key::PageDown,
        // A cursor position report, `row;col R`.
        (_, b'R') if modified => match report_row(&buf[..=end]) {
            Some(row) => Key::Position(row),
            None => return Parsed::Skip(end + 1),
        },
        _ => return Parsed::Skip(end + 1),
    };
    Parsed::Key(key, end + 1)
}

/// The row in a cursor position report, `ESC [ row ; col R`, from 0.
fn report_row(reply: &[u8]) -> Option<usize> {
    let start = reply.windows(2).rposition(|w| w == b"\x1b[")? + 2;
    let body = std::str::from_utf8(&reply[start..reply.len() - 1]).ok()?;
    body.split_once(';')?.0.parse::<usize>().ok()?.checked_sub(1)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str, n: usize) -> Parsed {
        Parsed::Key(Key::Text(s.into()), n)
    }

    #[test]
    fn reads_the_row_from_a_position_report() {
        assert_eq!(report_row(b"\x1b[12;1R"), Some(11));
        assert_eq!(report_row(b"x\x1b[1;80R"), Some(0));
        assert_eq!(report_row(b"\x1b[;1R"), None);
        // Read with the keys while a turn runs.
        assert_eq!(parse(b"\x1b[12;5Rx"), Parsed::Key(Key::Position(11), 7));
        assert_eq!(parse(b"\x1b[12;5"), Parsed::Incomplete);
    }

    #[test]
    fn takes_keys_apart() {
        assert_eq!(parse(b"hi\rx"), text("hi", 2));
        assert_eq!(parse(b"\rx"), Parsed::Key(Key::Enter, 1));
        assert_eq!(parse(b"\x7f"), Parsed::Key(Key::Backspace, 1));
        assert_eq!(parse(b"\x1b[D"), Parsed::Key(Key::Left, 3));
        assert_eq!(parse(b"\x1b[1;5C"), Parsed::Key(Key::WordRight, 6));
        assert_eq!(parse(b"\x1b[3~"), Parsed::Key(Key::Delete, 4));
        assert_eq!(parse(b"\x1bOH"), Parsed::Key(Key::Home, 3));
        assert_eq!(parse(b"\x1b\r"), Parsed::Key(Key::Newline, 2));
        assert_eq!(parse(b"\x1b\x1b"), Parsed::Esc);
        assert_eq!(parse(b"\x1b[5~"), Parsed::Key(Key::PageUp, 4));
        assert_eq!(parse(b"\x1b[6~"), Parsed::Key(Key::PageDown, 4));
        assert_eq!(parse(b"\x0f"), Parsed::Key(Key::ToolOutput, 1));
        assert_eq!(parse(b"\x07"), Parsed::Skip(1));
    }

    #[test]
    fn distinguishes_submit_linebreak_tab_and_eof() {
        assert_eq!(parse(b"\t"), Parsed::Key(Key::Tab, 1));
        assert_eq!(parse(b"\x04"), Parsed::Key(Key::Eof, 1));
        assert_eq!(parse(b"\x1b[13u"), Parsed::Key(Key::Enter, 5));
        assert_eq!(parse(b"\x1b[13;2u"), Parsed::Key(Key::Newline, 7));
        assert_eq!(parse(b"\x1b[27;2;13~"), Parsed::Key(Key::Newline, 10));
    }

    #[test]
    fn waits_for_the_rest_of_a_sequence() {
        assert_eq!(parse(b"\x1b"), Parsed::Incomplete);
        assert_eq!(parse(b"\x1b[1;"), Parsed::Incomplete);
        assert_eq!(parse(b"\x1b[200~half"), Parsed::Incomplete);
        // 看 is three bytes; two arrived.
        assert_eq!(parse(&"看".as_bytes()[..2]), Parsed::Incomplete);
        assert_eq!(parse("看看".as_bytes().split_at(4).0), text("看", 3));
    }

    #[test]
    fn pasted_text_arrives_whole() {
        assert_eq!(parse(b"\x1b[200~a\r\nb\rc\x1b[201~d"), text("a\nb\nc", 18));
    }
}
