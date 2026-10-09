//! Shared keyboard-driven panels for slash commands, kept out of scrollback.

use std::sync::{Arc, Mutex};

use super::{Output, text};
use super::input::Draft;
use super::keys::{Key, KeyWatch};

pub type Frame = (Vec<String>, Option<(usize, usize)>);

pub(super) trait Panel: Send {
    fn render(&self, width: usize, height: usize, paint: &dyn Fn(&str, &str) -> String) -> Frame;
}

#[derive(Clone)]
pub struct Item {
    pub label: String,
    pub description: String,
    pub details: Vec<String>,
}

impl Item {
    pub fn new(label: impl Into<String>, description: impl Into<String>) -> Self {
        Self { label: label.into(), description: description.into(), details: Vec::new() }
    }

    pub fn details(mut self, details: impl IntoIterator<Item = String>) -> Self {
        self.details = details.into_iter().collect();
        self
    }
}

/// Owns keyboard input and panel cleanup for every slash-command dialog.
pub(super) async fn run<P: Panel + 'static, R>(
    out: &Output, panel: P, mut handle: impl FnMut(&mut P, Key) -> Option<Option<R>>,
) -> Option<R> {
    struct Guard(Output);
    impl Drop for Guard {
        fn drop(&mut self) { self.0.set_panel(None); }
    }
    let panel = Arc::new(Mutex::new(panel));
    let _guard = Guard(out.clone());
    out.set_panel(Some(panel.clone()));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let watch = KeyWatch::for_prompt(move |key| { let _ = tx.send(key); });
    let result = loop {
        let key = tokio::select! {
            _ = tokio::signal::ctrl_c() => break None,
            key = rx.recv() => match key { Some(key) => key, None => break None },
        };
        let done = handle(&mut panel.lock().unwrap(), key);
        if let Some(done) = done { break done; }
        out.set_panel(Some(panel.clone()));
    };
    drop(watch);
    result
}

pub async fn choose(out: &Output, title: &str, subtitle: &str, items: Vec<Item>, initial: usize) -> Option<usize> {
    let panel = List {
        title: title.into(), subtitle: subtitle.into(), items, query: Draft::default(),
        selected: initial, detail: false, inspect_only: false, offset: 0,
    };
    run(out, panel, List::handle).await
}

pub async fn inspect(out: &Output, title: &str, items: Vec<Item>) {
    let _ = choose(out, title, "Enter closes · Tab opens full details", items, 0).await;
}

pub async fn document(out: &Output, title: &str, lines: Vec<String>) {
    let panel = List {
        title: title.into(), subtitle: "Read-only details".into(),
        items: vec![Item::new(title, "").details(lines)], query: Draft::default(), selected: 0,
        detail: true, inspect_only: false, offset: 0,
    };
    let _ = run(out, panel, |panel, key| {
        if matches!(key, Key::Enter | Key::Esc | Key::Eof | Key::Tab) { Some(Some(())) }
        else { panel.handle(key); None }
    }).await;
}

pub async fn input(out: &Output, title: &str, prompt: &str) -> Option<String> {
    run(out, TextInput { title: title.into(), prompt: prompt.into(), draft: Draft::default() }, TextInput::handle).await
}

struct TextInput {
    title: String,
    prompt: String,
    draft: Draft,
}

impl TextInput {
    fn handle(&mut self, key: Key) -> Option<Option<String>> {
        match key {
            Key::Esc | Key::Eof => return Some(None),
            Key::Enter if !self.draft.text().trim().is_empty() => return Some(Some(self.draft.text().trim().into())),
            Key::Enter | Key::Tab | Key::Newline | Key::PasteImage => {},
            key => self.draft.edit(key),
        }
        None
    }
}

impl Panel for TextInput {
    fn render(&self, width: usize, _height: usize, paint: &dyn Fn(&str, &str) -> String) -> Frame {
        let width = width.saturating_sub(1).max(3);
        let room = width.saturating_sub(2);
        let row = |content: String| format!("  {}", text::clip(&content, room));
        let rows = vec![row(paint("1;35", &text::printable(&self.title))),
            row(paint("2", &text::printable(&self.prompt))), String::new(),
            row(format!("❯ {}", text::printable(self.draft.text()))),
            row(paint("2", "Enter continue · Esc cancel"))];
        let cursor = (3, (4 + text::width(&self.draft.text()[..self.draft.position()])).min(width - 1));
        (rows, Some(cursor))
    }
}

pub(super) struct List {
    title: String,
    subtitle: String,
    items: Vec<Item>,
    query: Draft,
    /// Position within the filtered list; chosen values use the original index.
    selected: usize,
    detail: bool,
    inspect_only: bool,
    offset: usize,
}

impl List {
    pub(super) fn tool_output(items: Vec<Item>) -> Self {
        let selected = items.len().saturating_sub(1);
        let detail = !items.is_empty();
        Self { title: "Tool output".into(), subtitle: "Full returned text and input · Ctrl+O closes".into(),
            items, query: Draft::default(), selected, detail, inspect_only: true, offset: 0 }
    }

    pub(super) fn set_items(&mut self, items: Vec<Item>) { self.items = items; }

    pub(super) fn inspect_key(&mut self, key: Key) -> bool {
        if key == Key::Enter && !self.detail {
            self.detail = self.current().is_some();
            self.offset = 0;
            return false;
        }
        self.handle(key).is_some()
    }

    fn filtered(&self) -> Vec<usize> {
        let query = self.query.text().trim().to_lowercase();
        let mut matches: Vec<_> = self.items.iter().enumerate().filter_map(|(i, item)| {
            let label = item.label.to_lowercase();
            let rank = if label.starts_with(&query) { 0 } else if label.contains(&query) { 1 }
                else if item.description.to_lowercase().contains(&query) { 2 } else { return None };
            Some((i, rank))
        }).collect();
        matches.sort_by_key(|(_, rank)| *rank);
        matches.into_iter().map(|(i, _)| i).collect()
    }

    fn current(&self) -> Option<usize> {
        let filtered = self.filtered();
        filtered.get(self.selected.min(filtered.len().saturating_sub(1))).copied()
    }

    fn detail_lines(&self, room: usize) -> Vec<String> {
        let lines = self.current().map(|i| {
            let item = &self.items[i];
            std::iter::once(item.label.clone()).chain(std::iter::once(item.description.clone())).chain(item.details.clone()).collect::<Vec<_>>()
        }).unwrap_or_default();
        lines.iter().flat_map(|line| line.split('\n')).flat_map(|line| text::wrap(&text::printable(line), room)).collect()
    }

    fn handle(&mut self, key: Key) -> Option<Option<usize>> {
        if self.detail {
            let room = text::terminal_width().saturating_sub(3).max(1);
            let count = text::terminal_height().unwrap_or(24).saturating_sub(6).max(1);
            let last = self.detail_lines(room).len().saturating_sub(count);
            self.offset = self.offset.min(last);
            match key {
                Key::Esc | Key::Tab | Key::Left => { self.detail = false; self.offset = 0; }
                Key::Eof => return Some(None),
                Key::Enter => return self.current().map(Some),
                Key::Up => self.offset = self.offset.saturating_sub(1),
                Key::Down => self.offset = (self.offset + 1).min(last),
                Key::PageUp => self.offset = self.offset.saturating_sub(count),
                Key::PageDown => self.offset = (self.offset + count).min(last),
                Key::Home => self.offset = 0,
                Key::End => self.offset = last,
                _ => {}
            }
            return None;
        }
        match key {
            Key::Esc | Key::Eof => return Some(None),
            Key::Enter => return self.current().map(Some),
            Key::Tab | Key::Right => { self.detail = self.current().is_some(); self.offset = 0; }
            Key::Up | Key::Down => {
                let count = self.filtered().len();
                if count > 0 {
                    let selected = self.selected.min(count - 1);
                    let step = if key == Key::Up { -1 } else { 1 };
                    self.selected = (selected as isize + step).rem_euclid(count as isize) as usize;
                }
            }
            Key::Newline | Key::PasteImage | Key::Position(_) => {}
            key => { self.query.edit(key); self.selected = 0; self.offset = 0; }
        }
        None
    }
}

impl Panel for List {
    fn render(&self, width: usize, height: usize, paint: &dyn Fn(&str, &str) -> String) -> Frame {
        let width = width.saturating_sub(1).max(3);
        let room = width.saturating_sub(2);
        let row = |content: String| format!("  {}", text::clip(&content, room));
        let mut rows = vec![row(paint("1;35", &text::fit(&text::printable(&self.title), room)))];
        rows.push(row(paint("2", &text::fit(&text::printable(&self.subtitle), room))));
        rows.push(String::new());
        if self.detail {
            let lines = self.detail_lines(room);
            let count = height.saturating_sub(6).max(1);
            let start = self.offset.min(lines.len().saturating_sub(count));
            for line in lines.iter().skip(start).take(count) { rows.push(row(line.clone())); }
            if lines.is_empty() { rows.push(row(paint("2", "No details"))); }
            rows.push(row(paint("2", &format!("{}–{} of {} lines", start + 1, (start + count).min(lines.len()), lines.len()))));
            rows.push(row(paint("2", if self.inspect_only { "↑/↓ PgUp/PgDn scroll · Tab/Esc list · Enter/Ctrl+O close" }
                else { "↑/↓ scroll · Tab/Esc back · Enter select" })));
            return (rows, None);
        }
        let query = if self.query.is_empty() { paint("2", "type to filter") } else { text::printable(self.query.text()) };
        let cursor = (rows.len(), (10 + text::width(&self.query.text()[..self.query.position()])).min(width - 1));
        rows.push(row(format!("Search: {query}")));
        let filtered = self.filtered();
        let selected = self.selected.min(filtered.len().saturating_sub(1));
        let count = height.saturating_sub(11).clamp(1, 8);
        let start = (selected + 1).saturating_sub(count);
        for (position, i) in filtered.iter().enumerate().skip(start).take(count) {
            let item = &self.items[*i];
            let mark = if position == selected { "❯" } else { " " };
            rows.push(row(paint(if position == selected { "1;35" } else { "0" }, &format!("{mark} {}", text::fit(&text::printable(&item.label), room.saturating_sub(2))))));
        }
        if filtered.is_empty() { rows.push(row(paint("2", if self.items.is_empty() { if self.inspect_only { "No tool calls in this conversation" } else { "No items available" } } else { "No matching items" }))); }
        rows.push(String::new());
        if let Some(i) = self.current() {
            let item = &self.items[i];
            let summary: Vec<_> = std::iter::once(&item.description).chain(item.details.iter()).flat_map(|s| s.split('\n')).flat_map(|s| text::wrap(&text::printable(s), room)).take(3).collect();
            for line in summary { rows.push(row(paint("2", &line))); }
        }
        rows.push(row(paint("2", &format!("{} matching · ↑/↓ select · Tab details", filtered.len()))));
        rows.push(row(paint("2", if self.inspect_only { "Enter/Tab details · Esc/Ctrl+O close" } else { "Enter select · Esc cancel" })));
        (rows, Some(cursor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> List {
        List { title: "Test".into(), subtitle: "Choose".into(), items: vec![
            Item::new("alpha", "mentions beta").details(vec!["Long details".repeat(100)]), Item::new("beta", "second"),
        ], query: Draft::default(), selected: 0, detail: false, inspect_only: false, offset: 0 }
    }

    #[test]
    fn tool_inspector_opens_latest_and_can_browse_full_wrapped_output() {
        let mut panel = List::tool_output(vec![Item::new("first", "done"),
            Item::new("second", "done").details(vec!["0123456789".repeat(1000), "last output line".into()])]);
        assert_eq!(panel.current(), Some(1));
        assert!(panel.detail);
        let (rows, _) = panel.render(36, 18, &|_, s| s.into());
        assert!(rows.len() < 18);
        assert!(rows.iter().all(|r| text::width(r) < 36));
        panel.inspect_key(Key::End);
        assert!(panel.offset > 0);
        panel.inspect_key(Key::Home);
        assert_eq!(panel.offset, 0);
        assert!(!panel.inspect_key(Key::Esc));
        assert!(!panel.detail);
        panel.inspect_key(Key::Up);
        assert_eq!(panel.current(), Some(0));
        assert!(!panel.inspect_key(Key::Enter));
        assert!(panel.detail);
        assert!(panel.inspect_key(Key::Enter));
    }

    #[test]
    fn filters_return_original_indexes_and_empty_results_do_not_accept() {
        let mut list = list();
        list.handle(Key::Text("BETA".into()));
        assert_eq!(list.handle(Key::Enter), Some(Some(1)));
        list.handle(Key::Text("unknown".into()));
        assert_eq!(list.handle(Key::Enter), None);
        assert_eq!(list.handle(Key::Esc), Some(None));
    }

    #[test]
    fn details_scroll_then_return_to_selection_and_eof_cancels() {
        let mut list = list();
        list.handle(Key::Tab);
        list.handle(Key::Down);
        assert_eq!(list.offset, 1);
        list.handle(Key::Esc);
        assert!(!list.detail);
        list.handle(Key::Down);
        assert_eq!(list.handle(Key::Enter), Some(Some(1)));
        assert_eq!(list.handle(Key::Eof), Some(None));
        for (width, height) in [(80,24), (36,18)] {
            for detail in [false,true] {
                list.detail = detail;
                let (rows, _) = list.render(width,height,&|_,s| s.into());
                assert!(rows.len() < height);
                assert!(rows.iter().all(|r| text::width(r) < width));
            }
        }
    }
}
