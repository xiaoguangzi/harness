//! A model's question replaces the composer without touching its draft.

use std::cell::Cell;
use tokio::sync::oneshot;
use crate::tools::question::{Answer, Request};
use super::{dialog::Frame, input::Draft, keys::Key, text};
use super::style::{ACCENT, BOLD, DIM, PLAIN};

/// Columns before an option's label: `❯ 1. `.
const LEAD: usize = 5;
/// What the custom answer's row says until something is typed.
const PLACEHOLDER: &str = "Type something.";

pub(super) struct Question {
    request: Request,
    agent: Option<String>,
    selected: usize,
    draft: Draft,
    offset: Cell<usize>,
    follow: Cell<bool>,
    answer: Option<oneshot::Sender<Answer>>,
}

impl Question {
    pub fn new(request: Request, agent: Option<String>, answer: oneshot::Sender<Answer>) -> Self {
        Self { request, agent, selected: 0, draft: Draft::default(), offset: Cell::new(0), follow: Cell::new(false), answer: Some(answer) }
    }

    pub fn key(&mut self, key: &Key) -> bool {
        let count = self.request.options.len();
        let answer = match key {
            Key::Esc | Key::Eof => Some(Answer::Cancelled {}),
            Key::Enter if self.selected < count => Some(Answer::Selected { index: self.selected }),
            Key::Enter if !self.draft.text().trim().is_empty() => Some(Answer::Custom { text: self.draft.text().trim().into() }),
            Key::Enter => None,
            Key::Up | Key::Down | Key::Tab | Key::BackTab => {
                self.selected = if matches!(key, Key::Up | Key::BackTab) {
                    self.selected.checked_sub(1).unwrap_or(count)
                } else { (self.selected + 1) % (count + 1) };
                self.follow.set(true);
                None
            }
            Key::PageUp | Key::PageDown => {
                let step = text::terminal_height().unwrap_or(24).saturating_sub(5).max(1);
                self.offset.set(if *key == Key::PageUp { self.offset.get().saturating_sub(step) } else { self.offset.get().saturating_add(step) });
                self.follow.set(false);
                None
            }
            Key::Text(s) if self.selected < count && s.parse::<usize>().ok().is_some_and(|n| (1..=count).contains(&n)) => {
                self.selected = s.parse::<usize>().unwrap() - 1;
                self.follow.set(true);
                None
            }
            Key::Text(_) | Key::Backspace | Key::Delete | Key::Left | Key::Right | Key::Home | Key::End
                | Key::WordLeft | Key::WordRight | Key::KillToStart | Key::KillToEnd | Key::KillWord | Key::Newline => {
                self.selected = count;
                // Bound pasted input before it can make the panel or result enormous.
                if !matches!(key, Key::Text(s) if self.draft.text().len() + s.len() > 16000)
                    && !(matches!(key, Key::Newline) && self.draft.text().len() >= 16000)
                { self.draft.edit(key.clone()); }
                self.follow.set(true);
                None
            }
            _ => None,
        };
        let Some(answer) = answer else { return false };
        if let Some(tx) = self.answer.take() { let _ = tx.send(answer); }
        true
    }

    /// A rule, the question, the numbered options with their descriptions
    /// under their labels and the custom answer as the last one, then the
    /// keys. Everything but the rule and the keys scrolls.
    pub fn render(&self, width: usize, height: usize, paint: &dyn Fn(&str, &str) -> String) -> Frame {
        let room = width.saturating_sub(1).max(1);
        let count = self.request.options.len();
        let custom = self.selected == count;
        // Each line of `s` wrapped to what is left after `lead` columns.
        let wrapped = |s: &str, lead: usize| -> Vec<String> {
            s.split('\n').flat_map(|line| text::wrap(&text::printable(line), room.saturating_sub(lead).max(1))).collect()
        };
        let mut lines = Vec::new();
        if let Some(agent) = &self.agent {
            lines.extend(wrapped(&format!("From the {agent} subagent"), 0).iter().map(|l| paint(DIM, l)));
        }
        lines.extend(wrapped(&self.request.question, 0).iter().map(|l| paint(BOLD, l)));
        lines.push(String::new());
        // The selected item's rows, kept in view when the selection moves.
        let mut focus = (0, 0);
        let lead = |i: usize, chosen: bool| {
            let lead = format!("{} {}. ", if chosen { "❯" } else { " " }, i + 1);
            (if chosen { paint(ACCENT, &lead) } else { lead }, LEAD)
        };
        for (i, option) in self.request.options.iter().enumerate() {
            let chosen = i == self.selected;
            let first = lines.len();
            let (mark, indent) = lead(i, chosen);
            for (j, line) in wrapped(&option.label, indent).iter().enumerate() {
                let lead = if j == 0 { mark.clone() } else { " ".repeat(indent) };
                lines.push(format!("{lead}{}", paint(if chosen { ACCENT } else { PLAIN }, line)));
            }
            if !option.description.trim().is_empty() {
                lines.extend(wrapped(&option.description, indent).iter().map(|l| format!("{}{}", " ".repeat(indent), paint(DIM, l))));
            }
            if chosen { focus = (first, lines.len() - 1); }
        }
        // Without options the answer is a plain input line.
        let (mark, indent) = if count > 0 { lead(count, custom) } else { (paint(ACCENT, "❯ "), 2) };
        let first = lines.len();
        let mut cursor = None;
        if self.draft.text().is_empty() {
            let placeholder = text::fit(PLACEHOLDER, room.saturating_sub(indent));
            lines.push(format!("{mark}{}", paint(if custom { DIM } else { PLAIN }, &placeholder)));
            if custom { cursor = Some((first, indent)); }
        } else {
            let (rows, (row, col)) = self.draft.wrapped(room.saturating_sub(indent).max(1));
            for (j, line) in rows.iter().enumerate() {
                let lead = if j == 0 { mark.clone() } else { " ".repeat(indent) };
                lines.push(format!("{lead}{}", paint(if custom { PLAIN } else { DIM }, &text::printable(line))));
            }
            if custom { cursor = Some((first + row, indent + col)); }
        }
        if let Some((row, _)) = cursor { focus = (row, row); }

        let shown = height.saturating_sub(4).max(1);
        let last = lines.len().saturating_sub(shown);
        let mut start = self.offset.get().min(last);
        if self.follow.replace(false) {
            if focus.1 >= start + shown { start = focus.1 + 1 - shown; }
            if focus.0 < start { start = focus.0; }
        }
        self.offset.set(start);
        let mut rows = vec![paint(DIM, &super::input::rule(width))];
        rows.extend(lines.iter().skip(start).take(shown).map(|line| text::clip(line, room)));
        rows.push(String::new());
        let mut keys = vec![if custom { "Enter to submit" } else { "Enter to select" }];
        if count > 0 { keys.push("↑/↓ to navigate"); }
        if last > 0 { keys.push("PgUp/PgDn to scroll"); }
        keys.push("Esc to cancel");
        rows.push(paint(DIM, &text::fit(&keys.join(" · "), room)));
        let cursor = cursor.filter(|(row, _)| (start..start + shown).contains(row)).map(|(row, col)| (row - start + 1, col.min(room)));
        (rows, cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::question::OptionItem;

    fn question() -> (Question, oneshot::Receiver<Answer>) {
        let (tx, rx) = oneshot::channel();
        let request = Request { question: "使用哪种方案？".into(), options: vec![
            OptionItem { label: "方案 A".into(), description: "兼容现有实现".into() },
            OptionItem { label: "方案 B".into(), description: "改动较大".into() },
        ] };
        (Question::new(request, None, tx), rx)
    }

    #[test]
    fn selecting_requires_submit_and_custom_text_is_returned_verbatim() {
        let (mut q, mut rx) = question();
        q.key(&Key::Down);
        assert!(rx.try_recv().is_err());
        assert!(q.key(&Key::Enter));
        assert!(matches!(rx.try_recv(), Ok(Answer::Selected { index: 1 })));
        let (mut q, mut rx) = question();
        q.key(&Key::Text("另一个方案".into()));
        q.key(&Key::Newline);
        q.key(&Key::Text("保留接口".into()));
        assert!(q.key(&Key::Enter));
        assert!(matches!(rx.try_recv(), Ok(Answer::Custom { text }) if text == "另一个方案\n保留接口"));
    }

    #[test]
    fn escape_and_eof_cancel_and_blank_custom_answers_do_not_submit() {
        for key in [Key::Esc, Key::Eof] {
            let (mut q, mut rx) = question();
            q.key(&Key::Text(" ".into()));
            assert!(!q.key(&Key::Enter));
            assert!(q.key(&key));
            assert!(matches!(rx.try_recv(), Ok(Answer::Cancelled {})));
        }
        let (mut q, mut rx) = question();
        q.request.options.clear();
        assert!(!q.key(&Key::Enter));
        assert!(rx.try_recv().is_err());
        q.key(&Key::Text("free text".into()));
        assert!(q.key(&Key::Enter));
    }

    #[test]
    fn options_are_numbered_with_descriptions_under_their_labels_and_typing_goes_on_the_last_one() {
        let (mut q, _rx) = question();
        let shown = |q: &Question| {
            let (rows, cursor) = q.render(60, 30, &|_, s| s.into());
            (rows.iter().map(|r| text::strip_ansi(r)).collect::<Vec<_>>(), cursor)
        };
        let (rows, cursor) = shown(&q);
        assert_eq!(rows[1..], ["使用哪种方案？", "", "❯ 1. 方案 A", "     兼容现有实现", "  2. 方案 B", "     改动较大",
            "  3. Type something.", "", "Enter to select · ↑/↓ to navigate · Esc to cancel"]);
        assert!(rows[0].starts_with('─') && cursor.is_none());
        q.key(&Key::Text("都不要".into()));
        let (rows, cursor) = shown(&q);
        assert_eq!(rows[3], "  1. 方案 A");
        assert_eq!(rows[7], "❯ 3. 都不要");
        assert_eq!(cursor, Some((7, 11)));
        assert!(rows[9].starts_with("Enter to submit"));
        q.request.options.clear();
        q.selected = 0;
        let (rows, _) = shown(&q);
        assert_eq!(rows[1..], ["使用哪种方案？", "", "❯ 都不要", "", "Enter to submit · Esc to cancel"]);
    }

    #[test]
    fn long_chinese_content_scrolls_and_fits_small_terminals() {
        let (mut q, _rx) = question();
        q.request.question = "很长的问题，需要能看完整。".repeat(40);
        q.request.options[0].description = "权衡和限制。".repeat(60);
        for (width, height) in [(100, 30), (36, 12), (20, 8)] {
            q.offset.set(0);
            q.follow.set(false);
            let (rows, _) = q.render(width, height, &|_, s| s.into());
            assert!(rows.len() < height, "{rows:?}");
            assert!(rows.iter().all(|r| text::width(r) < width), "{rows:?}");
            q.key(&Key::Down);
            let (rows, _) = q.render(width, height, &|_, s| s.into());
            assert!(rows.iter().any(|r| r.contains("❯ 2.")), "{rows:?}");
            q.selected = 0;
        }
        q.key(&Key::Text("自定义文本".repeat(20)));
        let (rows, cursor) = q.render(36, 12, &|_, s| s.into());
        assert!(cursor.is_some_and(|(r, c)| r < rows.len() && c < 36));
    }
}
