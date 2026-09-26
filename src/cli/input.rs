//! The input box: a line between two rules, pinned under the output. The
//! line editor draws it while the REPL waits; while a turn runs, the output
//! draws it under the status and what is typed there lands in a [`Draft`].
//! A message sent from it meanwhile goes to the model at its next step; a
//! command waits for the turn to end.

use super::clipboard::Image;
use super::keys::Key;
use super::repl::spaced;
use super::text;
use crate::agent::Inbox;
use crate::mentions;

/// Rows of the draft shown at once; the box scrolls with the cursor.
const DRAFT_ROWS: usize = 8;
/// Queued messages listed above the box.
const QUEUED_ROWS: usize = 4;
/// Shown in the empty box while a turn runs.
const PLACEHOLDER: &str = "Type a message; the model reads it at its next step";
pub const PROMPT: &str = "❯ ";
pub const PROMPT_SGR: &str = "1;35";

/// The rule above and below the box, a column short of the terminal's
/// width so it never wraps.
pub fn rule(width: usize) -> String {
    "─".repeat(width.saturating_sub(1))
}

/// Text typed into the box while a turn runs.
#[derive(Default)]
pub struct Draft {
    text: String,
    /// Byte offset of the cursor in `text`.
    cursor: usize,
    /// Images pasted into it, by number.
    images: Vec<(usize, Image)>,
    /// The highest image number given out, so the next one gets its own.
    last_image: usize,
}

impl Draft {
    /// Hands the draft over to the line editor: the text before and after
    /// the cursor, and its images.
    pub fn take(&mut self) -> (String, String, Vec<(usize, Image)>) {
        let after = self.text.split_off(self.cursor);
        self.cursor = 0;
        (std::mem::take(&mut self.text), after, std::mem::take(&mut self.images))
    }

    /// Makes sure images pasted from now on are numbered after `n`.
    pub fn note_image(&mut self, n: usize) {
        self.last_image = self.last_image.max(n);
    }

    pub fn last_image(&self) -> usize {
        self.last_image
    }

    pub fn edit(&mut self, key: Key) {
        let (text, cursor) = (&self.text, self.cursor);
        let line_start = text[..cursor].rfind('\n').map_or(0, |i| i + 1);
        let line_end = text[cursor..].find('\n').map_or(text.len(), |i| cursor + i);
        match key {
            Key::Text(s) => self.insert(&s),
            Key::Newline => self.insert("\n"),
            Key::Backspace => {
                let start = prev_char(text, cursor);
                self.remove(start, cursor);
            }
            Key::Delete => {
                let end = next_char(text, cursor);
                self.remove(cursor, end);
            }
            Key::Left => self.cursor = prev_char(text, cursor),
            Key::Right => self.cursor = next_char(text, cursor),
            Key::Home => self.cursor = line_start,
            Key::End => self.cursor = line_end,
            Key::Up | Key::Down => {
                let column = text[line_start..cursor].chars().count();
                let (start, end) = if key == Key::Up {
                    let Some(end) = line_start.checked_sub(1) else { return };
                    (text[..end].rfind('\n').map_or(0, |i| i + 1), end)
                } else {
                    let start = line_end + 1;
                    if start > text.len() {
                        return;
                    }
                    (start, text[start..].find('\n').map_or(text.len(), |i| start + i))
                };
                let line = &text[start..end];
                self.cursor = start + line.char_indices().nth(column).map_or(line.len(), |(i, _)| i);
            }
            Key::WordLeft => self.cursor = word_start(text, cursor),
            Key::WordRight => {
                let rest = &text[cursor..];
                let word = rest.find(|c: char| !c.is_whitespace()).unwrap_or(rest.len());
                let end = rest[word..].find(char::is_whitespace).map_or(rest.len(), |i| word + i);
                self.cursor += end;
            }
            Key::KillToStart => self.remove(line_start, cursor),
            Key::KillToEnd => self.remove(cursor, line_end),
            Key::KillWord => {
                let start = word_start(text, cursor);
                self.remove(start, cursor);
            }
            Key::Enter | Key::PasteImage => {}
        }
    }

    /// Types `s` at the cursor. Line breaks are kept, tabs become spaces
    /// and other control characters are dropped.
    fn insert(&mut self, s: &str) {
        let mut clean = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '\t' => clean.push_str("    "),
                '\n' => clean.push('\n'),
                c if c.is_control() => {}
                c => clean.push(c),
            }
        }
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    fn remove(&mut self, start: usize, end: usize) {
        self.text.replace_range(start..end, "");
        self.cursor = start;
    }

    /// Adds a pasted image, standing in the text as `[Image #n]`.
    pub fn paste_image(&mut self, image: Image) {
        self.last_image += 1;
        self.images.push((self.last_image, image));
        self.paste(&mentions::image_placeholder(self.last_image));
    }

    /// Types `s` at the cursor, set off from the words around it by spaces.
    pub fn paste(&mut self, s: &str) {
        let text = spaced(&self.text, self.cursor, s);
        self.insert(&text);
    }

    /// What Enter sends: the text and the images still in it, leaving the
    /// box empty. Nothing for an empty box, or when the text ends in `\`,
    /// which continues on a new line instead.
    pub fn submit(&mut self) -> Option<(String, Vec<(usize, Image)>)> {
        if self.cursor == self.text.len() && self.text.ends_with('\\') {
            self.text.pop();
            self.cursor -= 1;
            self.insert("\n");
            return None;
        }
        let text = self.text.trim().to_string();
        if text.is_empty() {
            return None;
        }
        self.text.clear();
        self.cursor = 0;
        let images = std::mem::take(&mut self.images)
            .into_iter()
            .filter(|(n, _)| text.contains(&mentions::image_placeholder(*n)))
            .collect();
        Some((text, images))
    }

    /// The box's rows, at most `width - 1` columns wide, and where the
    /// cursor goes in them: (row, column).
    pub fn render(&self, width: usize, paint: impl Fn(&str, &str) -> String) -> (Vec<String>, (usize, usize)) {
        let prompt = paint(PROMPT_SGR, PROMPT.trim_end()) + " ";
        let room = width.saturating_sub(3).max(1);
        if self.text.is_empty() {
            return (vec![format!("{prompt}{}", paint("2", &text::fit(PLACEHOLDER, room)))], (0, 2));
        }
        let (mut rows, mut row, mut used) = (Vec::new(), String::new(), 0);
        let mut cursor = (0, 0);
        for (i, c) in self.text.char_indices() {
            let w = text::char_width(c);
            if c != '\n' && used + w > room {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            if i == self.cursor {
                cursor = (rows.len(), used);
            }
            if c == '\n' {
                rows.push(std::mem::take(&mut row));
                used = 0;
                continue;
            }
            row.push(c);
            used += w;
        }
        if self.cursor == self.text.len() {
            cursor = (rows.len(), used);
        }
        rows.push(row);
        // Show the rows around the cursor.
        let first = (cursor.0 + 1).saturating_sub(DRAFT_ROWS).min(rows.len().saturating_sub(DRAFT_ROWS));
        let shown = rows
            .into_iter()
            .enumerate()
            .skip(first)
            .take(DRAFT_ROWS)
            .map(|(i, row)| if i == 0 { format!("{prompt}{row}") } else { format!("  {row}") })
            .collect();
        (shown, (cursor.0 - first, cursor.1 + 2))
    }
}

fn prev_char(s: &str, at: usize) -> usize {
    s[..at].char_indices().next_back().map_or(0, |(i, _)| i)
}

fn next_char(s: &str, at: usize) -> usize {
    s[at..].chars().next().map_or(at, |c| at + c.len_utf8())
}

/// Where the word before `at` starts, past any spaces before `at`.
fn word_start(s: &str, at: usize) -> usize {
    let before = s[..at].trim_end();
    before.rfind(char::is_whitespace).map_or(0, |i| i + before[i..].chars().next().map_or(1, char::len_utf8))
}

/// A message sent from the box that the model hasn't got yet.
pub struct Queued {
    pub text: String,
    pub images: Vec<(usize, Image)>,
    /// Its id in the agent's inbox; `None` for a command, run once the
    /// turn is over.
    pub id: Option<u64>,
    /// How it shows in the conversation once the model has it.
    pub shown: Vec<String>,
}

/// What the box holds, kept from one turn to the next.
#[derive(Default)]
pub struct Typing {
    pub draft: Draft,
    pub queued: Vec<Queued>,
    /// Lines sent from the box, for the line editor's history.
    pub sent: Vec<String>,
}

impl Typing {
    /// Dim rows listing the queued messages, shown above the box.
    pub fn queued_rows(&self, width: usize, paint: impl Fn(&str, &str) -> String) -> Vec<String> {
        let room = width.saturating_sub(3);
        let first = self.queued.len().saturating_sub(QUEUED_ROWS);
        let mut rows: Vec<String> = self.queued[first..]
            .iter()
            .map(|q| {
                let line = text::printable(q.text.lines().next().unwrap_or(""));
                let more = if q.text.contains('\n') { " …" } else { "" };
                paint("2", &format!("{PROMPT}{}{more}", text::fit(&line, room.saturating_sub(text::width(more)))))
            })
            .collect();
        if first > 0 {
            rows.insert(0, paint("2", &format!("  … {first} more queued")));
        }
        rows
    }

    /// Takes out the messages the model has been handed, which no longer
    /// wait in `inbox`.
    pub fn take_delivered(&mut self, inbox: &Inbox) -> Vec<Queued> {
        let (delivered, waiting) =
            std::mem::take(&mut self.queued).into_iter().partition(|q| q.id.is_some_and(|id| !inbox.waiting(id)));
        self.queued = waiting;
        delivered
    }

    /// After a turn was cut short, puts what the model never got back into
    /// the draft, ahead of what was typed since: messages still in `inbox`
    /// (taken out of it) and commands.
    pub fn recall(&mut self, inbox: &Inbox) {
        let mut texts = Vec::new();
        for q in std::mem::take(&mut self.queued) {
            if q.id.is_none_or(|id| inbox.withdraw(id)) {
                texts.push(q.text);
                self.draft.images.extend(q.images);
            }
        }
        if texts.is_empty() {
            return;
        }
        if !self.draft.text.is_empty() {
            texts.push(std::mem::take(&mut self.draft.text));
        }
        self.draft.text = texts.join("\n");
        self.draft.cursor = self.draft.text.len();
    }

    /// The next command queued to run after the turn.
    pub fn next_command(&mut self) -> Option<String> {
        let i = self.queued.iter().position(|q| q.id.is_none())?;
        Some(self.queued.remove(i).text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(text: &str, cursor: usize) -> Draft {
        Draft { text: text.into(), cursor, ..Draft::default() }
    }

    fn plain(_: &str, s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn edits_the_text() {
        let mut d = draft("", 0);
        d.edit(Key::Text("hello world".into()));
        d.edit(Key::KillWord);
        assert_eq!((d.text.as_str(), d.cursor), ("hello ", 6));
        d.edit(Key::Text("看\tx".into()));
        d.edit(Key::Left);
        d.edit(Key::Backspace);
        assert_eq!(d.text, "hello 看   x");
        d.edit(Key::Home);
        d.edit(Key::WordRight);
        assert_eq!(d.cursor, 5);
        d.edit(Key::KillToEnd);
        assert_eq!(d.text, "hello");
    }

    #[test]
    fn moves_between_lines() {
        let mut d = draft("ab\n看看看\nc", 1);
        d.edit(Key::Down);
        assert_eq!(&d.text[d.cursor..], "看看\nc");
        d.edit(Key::Down);
        assert_eq!(d.cursor, d.text.len());
        d.edit(Key::Up);
        d.edit(Key::Up);
        assert_eq!(d.cursor, 1);
        d.edit(Key::Up);
        assert_eq!(d.cursor, 1);
    }

    #[test]
    fn a_trailing_backslash_continues_the_text() {
        let mut d = draft("one\\", 4);
        assert!(d.submit().is_none());
        d.edit(Key::Text("two ".into()));
        assert_eq!(d.submit().map(|(t, _)| t), Some("one\ntwo".into()));
        assert!(d.submit().is_none());
    }

    #[test]
    fn renders_rows_and_the_cursor() {
        // 10 columns leave 7 for text.
        let d = draft("abcdefghij\nk", 9);
        let (rows, cursor) = d.render(10, plain);
        assert_eq!(rows, vec!["❯ abcdefg", "  hij", "  k"]);
        assert_eq!(cursor, (1, 4));
        let (rows, cursor) = draft("", 0).render(20, plain);
        assert_eq!((rows[0].as_str(), cursor), ("❯ Type a message; …", (0, 2)));
        let (_, cursor) = draft("abc\n", 4).render(20, plain);
        assert_eq!(cursor, (1, 2));
    }

    #[test]
    fn recalls_what_the_model_never_got() {
        let inbox = Inbox::default();
        let queued = |text: &str, id| Queued { text: text.into(), images: Vec::new(), id, shown: Vec::new() };
        let taken = inbox.push_content(Vec::new(), true);
        inbox.take();
        let waiting = inbox.push_content(Vec::new(), true);
        let mut typing = Typing {
            draft: draft("typed", 5),
            queued: vec![queued("seen", Some(taken)), queued("unseen", Some(waiting)), queued("/model", None)],
            sent: Vec::new(),
        };
        assert_eq!(typing.take_delivered(&inbox).len(), 1);
        typing.recall(&inbox);
        assert_eq!(typing.draft.text, "unseen\n/model\ntyped");
        assert!(!inbox.wants_turn());
    }
}
