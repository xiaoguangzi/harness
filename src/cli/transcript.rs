//! Expanded conversation viewer. Navigation never edits the prompt or pauses a turn.

use super::{text, tool_output::Record};
use super::input::Draft;
use super::keys::Key;
use super::dialog::Frame;
use super::style::{ACCENT, BOLD, DIM, PLAIN};

pub(super) enum Entry {
    Text { label: &'static str, content: String },
    Tool(usize),
}

pub(super) struct Transcript {
    offset: usize,
    anchor: Option<(usize, usize)>,
    follow: bool,
    inputs: bool,
    search: Option<Draft>,
    query: String,
    match_at: Option<(usize, usize)>,
    saved: (usize, bool, Option<(usize, usize)>),
}

impl Default for Transcript {
    fn default() -> Self {
        Self { offset: 0, anchor: None, follow: true, inputs: false, search: None,
            query: String::new(), match_at: None, saved: (0, true, None) }
    }
}

impl Transcript {
    fn lines(&self, entries: &[Entry], records: &[Record], room: usize) -> Vec<(String, &'static str, usize, usize)> {
        let mut lines = Vec::new();
        for (entry_index, entry) in entries.iter().enumerate() {
            let mut local = 0;
            let mut add = |s: &str, sgr| {
                for line in s.split('\n') {
                    for wrapped in text::wrap(&text::printable(line), room.saturating_sub(4).max(1)) {
                        lines.push((format!("  {wrapped}"), sgr, entry_index, local));
                        local += 1;
                    }
                }
            };
            match entry {
                Entry::Text { label, content } => {
                    add(label, if *label == "❯ You" { ACCENT } else { BOLD });
                    add(content, if *label == "Thinking" { DIM } else { PLAIN });
                }
                Entry::Tool(index) => {
                    if let Some(record) = records.get(*index) {
                        for (line, sgr) in record.expanded(self.inputs) { add(&line, sgr); }
                    }
                }
            }
            add("", PLAIN);
        }
        lines
    }

    fn start(&self, lines: &[(String, &str, usize, usize)], count: usize) -> usize {
        let last = lines.len().saturating_sub(count);
        if self.follow { last } else {
            self.anchor.and_then(|(entry, local)| lines.iter().position(|(_, _, e, l)| *e == entry && *l >= local))
                .unwrap_or(self.offset).min(last)
        }
    }

    fn find(&mut self, lines: &[(String, &str, usize, usize)], count: usize, reverse: bool, include_current: bool) {
        if self.query.is_empty() { return; }
        let query = self.query.to_lowercase();
        let matches: Vec<_> = lines.iter().enumerate().filter_map(|(i, (s, _, _, _))|
            s.to_lowercase().contains(&query).then_some(i)).collect();
        let at = if include_current { self.offset } else {
            self.match_at.and_then(|(entry, local)| lines.iter().position(|(_, _, e, l)| *e == entry && *l == local))
                .unwrap_or(self.offset)
        };
        let found = if reverse {
            matches.iter().rev().find(|&&i| i < at).or(matches.last())
        } else {
            matches.iter().find(|&&i| i > at || (include_current && i == at)).or(matches.first())
        };
        if let Some(&i) = found {
            self.match_at = Some((lines[i].2, lines[i].3));
            self.offset = i.min(lines.len().saturating_sub(count));
            self.follow = false;
            self.remember(lines);
        }
    }

    pub fn key(&mut self, key: &Key, entries: &[Entry], records: &[Record], width: usize, height: usize) -> bool {
        let lines = self.lines(entries, records, width.saturating_sub(1).max(1));
        let count = height.saturating_sub(4).max(1);
        let last = lines.len().saturating_sub(count);
        self.offset = self.start(&lines, count);
        if let Some(draft) = &mut self.search {
            match key {
                Key::Esc => {
                    self.search = None;
                    (self.offset, self.follow, self.anchor) = self.saved;
                }
                Key::Enter => {
                    self.query = draft.text().to_string();
                    self.search = None;
                    self.find(&lines, count, false, true);
                }
                Key::ToolOutput | Key::Eof => return true,
                key => { draft.edit(key.clone()); }
            }
            return false;
        }
        let command = match key { Key::Text(s) => s.as_str(), _ => "" };
        match key {
            Key::ToolOutput | Key::Esc | Key::Eof => return true,
            Key::Up | Key::Down | Key::PageUp | Key::PageDown | Key::Home | Key::End => {}
            Key::Tab => { self.inputs = !self.inputs; return false; }
            _ => match command {
                "q" => return true,
                "i" => { self.inputs = !self.inputs; return false; }
                "/" => {
                    self.saved = (self.offset, self.follow, self.anchor);
                    self.follow = false;
                    self.remember(&lines);
                    self.search = Some(Draft::default());
                    return false;
                }
                "n" | "N" => { self.find(&lines, count, command == "N", false); return false; }
                "j" | "k" | "g" | "G" | " " | "b" => {}
                _ => return false,
            }
        }
        if key == &Key::Home || command == "g" { self.offset = 0; self.follow = false; }
        else if key == &Key::End || command == "G" { self.offset = last; self.follow = true; }
        else {
            let up = matches!(key, Key::Up | Key::PageUp) || matches!(command, "k" | "b");
            let page = matches!(key, Key::PageUp | Key::PageDown) || matches!(command, " " | "b");
            let step = if page { count } else { 1 };
            self.offset = if up { self.offset.saturating_sub(step) } else { (self.offset + step).min(last) };
            self.follow = !up && self.offset == last;
        }
        self.remember(&lines);
        false
    }

    fn remember(&mut self, lines: &[(String, &str, usize, usize)]) {
        self.anchor = lines.get(self.offset).map(|(_, _, entry, local)| (*entry, *local));
    }

    pub fn render(&self, entries: &[Entry], records: &[Record], width: usize, height: usize,
        paint: &dyn Fn(&str, &str) -> String) -> Frame {
        let room = width.saturating_sub(1).max(1);
        let count = height.saturating_sub(4).max(1);
        let lines = self.lines(entries, records, room);
        let start = self.start(&lines, count);
        let mut rows = vec![paint(ACCENT, &text::fit("  Detailed transcript", room)), String::new()];
        for (line, sgr, _, _) in lines.iter().skip(start).take(count) {
            let matched = !self.query.is_empty() && line.to_lowercase().contains(&self.query.to_lowercase());
            rows.push(paint(if matched { "30;43" } else { sgr }, &text::clip(line, room)));
        }
        if lines.is_empty() { rows.push(paint(DIM, &text::fit("  No conversation yet", room))); }
        while rows.len() < height.saturating_sub(2) { rows.push(String::new()); }
        let range = if lines.is_empty() { "0 lines".into() }
            else if room < 65 { format!("{}–{}/{}", start + 1, (start + count).min(lines.len()), lines.len()) }
            else { format!("{}–{} / {} lines", start + 1, (start + count).min(lines.len()), lines.len()) };
        let mode = match (self.follow, room < 65) {
            (true, true) => "live", (true, false) => "following",
            (false, true) => "paused · G", (false, false) => "paused · G to follow",
        };
        let input = if self.inputs { "hide inputs" } else { "show inputs" };
        let matches = if self.query.is_empty() { String::new() } else {
            let hits = lines.iter().filter(|(s, _, _, _)| s.to_lowercase().contains(&self.query.to_lowercase())).count();
            format!(" · {hits} matches")
        };
        let inputs = if room < 65 { "i inputs".into() } else { format!("i {input}") };
        rows.push(paint(DIM, &text::fit(&format!("  {range} · {mode}{matches} · {inputs}"), room)));
        let mut cursor = None;
        if let Some(draft) = &self.search {
            rows.push(text::clip(&format!("  /{}", text::printable(draft.text())), room));
            cursor = Some((rows.len() - 1, (3 + text::width(&draft.text()[..draft.position()])).min(room - 1)));
        } else {
            let hint = if room >= 85 {
                "  ↑↓/jk scroll · PgUp/PgDn page · g/G ends · / search · n/N match · Esc/Ctrl+O close"
            } else if room >= 60 { "  ↑↓/jk · PgUp/PgDn · / search · Esc/Ctrl+O close" }
                else if room >= 32 { "  ↑↓ · / find · Ctrl+O close" }
                else { "  ↑↓ · Ctrl+O close" };
            rows.push(paint(DIM, &text::fit(hint, room)));
        }
        if rows.len() > height { rows.truncate(height); cursor = None; }
        (rows, cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use crate::tools::ToolOutput;

    #[test]
    fn expands_all_calls_searches_output_and_follows_only_at_bottom() {
        let mut a = Record::pending("a", "bash", &json!({"command":"df -i"}));
        a.complete(&ToolOutput::ok("文件系统\n\n/dev/vda3   4% /"));
        let mut b = Record::pending("b", "bash", &json!({"command":"printf done"}));
        b.complete(&ToolOutput::ok("end\n".repeat(30)));
        let mut records = vec![a, b];
        let entries = vec![Entry::Tool(0), Entry::Tool(1)];
        let mut panel = Transcript::default();
        panel.key(&Key::Home, &entries, &records, 80, 16);
        let (rows, _) = panel.render(&entries, &records, 80, 16, &|_, s| s.into());
        assert!(rows.iter().any(|s| s.contains("df -i")));
        assert!(rows.iter().any(|s| s.contains("/dev/vda3")));
        assert!(!rows.iter().any(|s| s.contains("Call ID")));
        panel.key(&Key::Text("/".into()), &entries, &records, 80, 16);
        panel.key(&Key::Text("/dev/vda3".into()), &entries, &records, 80, 16);
        panel.key(&Key::Enter, &entries, &records, 80, 16);
        let offset = panel.offset;
        records[1].complete(&ToolOutput::ok("new output\n".repeat(80)));
        assert_eq!(panel.start(&panel.lines(&entries, &records, 79), 12), offset);
        panel.key(&Key::End, &entries, &records, 80, 16);
        assert!(panel.follow);
        panel.key(&Key::Up, &entries, &records, 80, 16);
        assert!(!panel.follow);
        panel.key(&Key::Text("/".into()), &entries, &records, 80, 16);
        assert!(!panel.key(&Key::Esc, &entries, &records, 80, 16));
        assert!(panel.key(&Key::Esc, &entries, &records, 80, 16));
    }

    #[test]
    fn reflows_unicode_and_multiline_commands_without_clipping_results() {
        let mut record = Record::pending("a", "bash", &json!({"command":"echo first\necho second"}));
        record.complete(&ToolOutput::error("中文 output\n".repeat(30) + "last result"));
        let entries = [Entry::Tool(0)];
        let records = [record];
        let panel = Transcript::default();
        for (width, height) in [(80, 24), (30, 12), (10, 6), (2, 2)] {
            let (rows, _) = panel.render(&entries, &records, width, height, &|_, s| s.into());
            assert!(rows.len() <= height);
            assert!(rows.iter().all(|s| text::width(s) < width));
            if width == 30 { assert!(rows.last().unwrap().contains("Ctrl+O close")); }
        }
        let lines = panel.lines(&entries, &records, 79);
        assert!(lines.iter().any(|(s, _, _, _)| s.contains("echo second")));
        assert!(lines.iter().any(|(s, _, _, _)| s.contains("last result")));
    }

    #[test]
    fn completion_of_an_earlier_concurrent_call_keeps_the_reading_anchor() {
        let records = &mut [Record::pending("a", "bash", &json!({"command":"first"})),
            Record::pending("b", "bash", &json!({"command":"second"}))];
        records[1].complete(&ToolOutput::ok("reading here\n".repeat(30)));
        let entries = [Entry::Tool(0), Entry::Tool(1)];
        let mut panel = Transcript::default();
        panel.key(&Key::Up, &entries, records, 80, 12);
        let before = panel.lines(&entries, records, 79);
        let anchor = panel.anchor.unwrap();
        records[0].complete(&ToolOutput::ok("earlier result\n".repeat(40)));
        let after = panel.lines(&entries, records, 79);
        let start = panel.start(&after, 8);
        assert_eq!((after[start].2, after[start].3), anchor);
        assert_eq!(before[panel.offset].0, after[start].0);
    }

    #[test]
    fn next_match_advances_even_when_matches_share_the_last_page() {
        let entries = [Entry::Text { label: "● Assistant", content: "needle one\nneedle two\nneedle three".into() }];
        let mut panel = Transcript::default();
        panel.key(&Key::Text("/".into()), &entries, &[], 80, 24);
        panel.key(&Key::Text("needle".into()), &entries, &[], 80, 24);
        panel.key(&Key::Enter, &entries, &[], 80, 24);
        let first = panel.match_at;
        panel.key(&Key::Text("n".into()), &entries, &[], 80, 24);
        assert_ne!(panel.match_at, first);
        panel.key(&Key::Text("N".into()), &entries, &[], 80, 24);
        assert_eq!(panel.match_at, first);
    }
}
