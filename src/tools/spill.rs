//! Tool output too large to return whole: the model gets its start and its
//! end, and the whole of it goes to a file it can `read` or `grep` instead.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::io::AsyncWriteExt;

/// Past this, a saved output is cut off rather than fill the disk.
const MAX_SAVED_BYTES: usize = 64 << 20;

/// `$TMPDIR/harness-<uid>/tool-output`, readable by its owner only.
fn dir() -> std::io::Result<PathBuf> {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let dir = std::env::temp_dir().join(format!("harness-{uid}")).join("tool-output");
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir.parent().unwrap_or(&dir), std::fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

/// A new file for `tool`'s output, created readable by its owner only.
fn create(tool: &str) -> std::io::Result<(std::fs::File, PathBuf)> {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    let name = format!("{}-{}-{tool}.txt", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
    let path = dir()?.join(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    Ok((options.open(&path)?, path))
}

/// The note between the start and the end of a shortened output.
fn note(omitted: usize, total: usize, path: Option<&PathBuf>) -> String {
    match path {
        Some(path) => format!(
            "\n... [{omitted} of {total} bytes left out here; the whole output is in {}: read or grep it there] ...\n",
            path.display()
        ),
        None => format!("\n... [{omitted} of {total} bytes left out here] ...\n"),
    }
}

/// `text` if it is at most `max` bytes; otherwise its first two thirds and
/// last third of `max`, the whole of it saved to a file the note between
/// them names.
pub(crate) fn shorten(text: &str, max: usize, tool: &str) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let path = create(tool).and_then(|(mut file, path)| {
        file.write_all(&text.as_bytes()[..text.len().min(MAX_SAVED_BYTES)])?;
        Ok(path)
    });
    let mut head = max / 3 * 2;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - (max - head);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}{}{}", &text[..head], note(tail - head, text.len(), path.as_ref().ok()), &text[tail..])
}

/// Collects a stream of output as it comes: the first `head` bytes and the
/// last `tail`, and, once there is more than both hold, all of it in a file.
pub(crate) struct Capture {
    tool: &'static str,
    head_max: usize,
    tail_max: usize,
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
    /// The file holding the whole output, once it needed one; `None` inside
    /// after it could not be written.
    file: Option<Option<(tokio::fs::File, PathBuf)>>,
}

impl Capture {
    /// Keeps up to `max` bytes in memory: two thirds from the start, the
    /// rest from the end.
    pub(crate) fn new(tool: &'static str, max: usize) -> Self {
        let head_max = max / 3 * 2;
        Self { tool, head_max, tail_max: max - head_max, head: Vec::new(), tail: Vec::new(), total: 0, file: None }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.total == 0
    }

    pub(crate) async fn push(&mut self, mut bytes: &[u8]) {
        let saved = self.total;
        self.total += bytes.len();
        let room = self.head_max - self.head.len();
        if room > 0 {
            let n = room.min(bytes.len());
            self.head.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            if bytes.is_empty() {
                return;
            }
        }
        if self.file.is_none() && self.tail.len() + bytes.len() > self.tail_max {
            // The tail is about to lose bytes: from now on all of it is kept
            // in the file, starting with what came before.
            let file = create(self.tool).ok().map(|(file, path)| (tokio::fs::File::from_std(file), path));
            self.file = Some(file);
            let earlier = [self.head.clone(), self.tail.clone()].concat();
            self.save(&earlier, 0).await;
        }
        if self.file.is_some() {
            self.save(bytes, saved.max(self.head_max)).await;
        }
        self.tail.extend_from_slice(bytes);
        let excess = self.tail.len().saturating_sub(self.tail_max);
        self.tail.drain(..excess);
    }

    /// Appends `bytes`, which start at `offset` in the output, to the file.
    async fn save(&mut self, bytes: &[u8], offset: usize) {
        let Some(Some((file, _))) = &mut self.file else { return };
        let room = MAX_SAVED_BYTES.saturating_sub(offset);
        if file.write_all(&bytes[..bytes.len().min(room)]).await.is_err() {
            self.file = Some(None);
        }
    }

    /// What was collected: all of it, or its start and end around a note
    /// naming the file with the whole of it.
    pub(crate) async fn text(&mut self) -> String {
        if let Some(Some((file, _))) = &mut self.file {
            let _ = file.flush().await;
        }
        let mut text = String::from_utf8_lossy(&self.head).into_owned();
        let omitted = self.total - self.head.len() - self.tail.len();
        if omitted > 0 {
            let path = self.file.as_ref().and_then(|f| f.as_ref()).map(|(_, path)| path);
            text.push_str(&note(omitted, self.total, path));
        }
        text.push_str(&String::from_utf8_lossy(&self.tail));
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved_path(text: &str) -> PathBuf {
        let at = text.find("the whole output is in ").unwrap() + "the whole output is in ".len();
        PathBuf::from(&text[at..text[at..].find(": read").unwrap() + at])
    }

    #[test]
    fn shortens_to_the_start_and_end_and_saves_the_rest() {
        assert_eq!(shorten("short", 10, "t"), "short");
        let text: String = (0..1000).map(|i| format!("{i:04}\n")).collect();
        let short = shorten(&text, 300, "t");
        assert!(short.starts_with("0000\n0001\n") && short.ends_with("0998\n0999\n"), "{short}");
        assert!(short.contains("[4700 of 5000 bytes left out here;"), "{short}");
        let path = saved_path(&short);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        std::fs::remove_file(path).unwrap();
        // Cuts land on character boundaries.
        let wide = "é".repeat(500);
        let short = shorten(&wide, 101, "t");
        std::fs::remove_file(saved_path(&short)).unwrap();
    }

    #[tokio::test]
    async fn captures_streams_whole_while_holding_only_their_ends() {
        let mut small = Capture::new("t", 30);
        small.push(b"0123456789").await;
        small.push(b"abcdefghijklmnopqrst").await;
        assert_eq!(small.text().await, "0123456789abcdefghijklmnopqrst");
        assert!(small.file.is_none(), "it all fit");

        let mut big = Capture::new("t", 30);
        let all: Vec<u8> = (0..500u32).map(|i| b'a' + (i % 26) as u8).collect();
        for chunk in all.chunks(7) {
            big.push(chunk).await;
        }
        let text = big.text().await;
        assert!(text.starts_with(std::str::from_utf8(&all[..20]).unwrap()), "{text}");
        assert!(text.ends_with(std::str::from_utf8(&all[490..]).unwrap()), "{text}");
        assert!(text.contains("[470 of 500 bytes left out here;"), "{text}");
        let path = saved_path(&text);
        assert_eq!(std::fs::read(&path).unwrap(), all);
        std::fs::remove_file(path).unwrap();
    }
}
