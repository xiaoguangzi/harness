//! Shared keyboard-driven panels for slash commands, kept out of scrollback.

use std::cell::Cell;
use std::sync::{Arc, Mutex};

use super::{Output, text};
use super::input::Draft;
use super::keys::{Key, KeyWatch};
use super::style::{ACCENT, DIM, PLAIN};

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

/// What Enter and Esc do in a list, for its key hints.
#[derive(Clone, Copy)]
pub struct Keys {
    pub enter: &'static str,
    pub esc: &'static str,
}

/// Most items a list shows at once.
const LIST_ROWS: usize = 8;
/// Rows under a list for a description too long for its row.
const SUMMARY_ROWS: usize = 2;
/// Narrowest description worth showing beside the labels.
const MIN_DESCRIPTION: usize = 16;

/// Owns keyboard input and panel cleanup for every slash-command dialog.
pub(super) async fn run<P: Panel + 'static, R>(
    out: &Output, panel: P, mut handle: impl FnMut(&mut P, Key) -> Option<Option<R>>,
) -> Option<R> {
    struct Guard(Output);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.return_keys();
            self.0.set_panel(None);
        }
    }
    let panel = Arc::new(Mutex::new(panel));
    let _guard = Guard(out.clone());
    out.set_panel(Some(panel.clone()));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    // Over a running turn, its key reader hands the keys over: two would
    // take turns at stdin.
    let watch = (!out.borrow_keys(tx.clone())).then(|| KeyWatch::for_prompt(move |key| { let _ = tx.send(key); }));
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

pub async fn choose(out: &Output, title: &str, subtitle: &str, keys: Keys, items: Vec<Item>, initial: usize) -> Option<usize> {
    run(out, List::new(title, subtitle, keys, items, initial), List::handle).await
}

pub async fn inspect(out: &Output, title: &str, subtitle: &str, items: Vec<Item>) {
    let _ = choose(out, title, subtitle, Keys { enter: "close", esc: "close" }, items, 0).await;
}

pub async fn document(out: &Output, title: &str, lines: Vec<String>) {
    let mut panel = List::new(title, "", Keys { enter: "close", esc: "close" }, vec![Item::new("", "").details(lines)], 0);
    (panel.detail, panel.back) = (true, false);
    let _ = run(out, panel, |panel, key| panel.handle(key).map(|_| Some(()))).await;
}

pub async fn input(out: &Output, title: &str, prompt: &str) -> Option<String> {
    run(out, TextInput { title: title.into(), prompt: prompt.into(), draft: Draft::default() }, TextInput::handle).await
}

/// The first of `count` rows shown out of `len` so that `selected` is among
/// them: `top` moves only as far as it must, so the list doesn't shift
/// under a selection that is already visible.
pub(super) fn window(top: &Cell<usize>, selected: usize, count: usize, len: usize) -> usize {
    let mut start = top.get().min(len.saturating_sub(count));
    if selected < start {
        start = selected;
    } else if selected >= start + count {
        start = selected + 1 - count;
    }
    top.set(start);
    start
}

/// Columns for a panel's content, inside its two-column margin.
fn room(width: usize) -> usize {
    width.saturating_sub(1).max(3) - 2
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
        let room = room(width);
        let row = |content: String| format!("  {}", text::clip(&content, room));
        let rows = vec![row(paint(ACCENT, &text::printable(&self.title))),
            row(paint(DIM, &text::printable(&self.prompt))), String::new(),
            row(format!("❯ {}", text::printable(self.draft.text()))),
            row(paint(DIM, "Enter continue · Esc cancel"))];
        let cursor = (3, (4 + text::width(&self.draft.text()[..self.draft.position()])).min(room + 1));
        (rows, Some(cursor))
    }
}

pub(super) struct List {
    title: String,
    subtitle: String,
    keys: Keys,
    items: Vec<Item>,
    query: Draft,
    /// Position within the filtered list; chosen values use the original index.
    selected: usize,
    /// The first filtered item shown.
    top: Cell<usize>,
    detail: bool,
    /// Whether Tab/Esc go from the details back to the list, rather than close.
    back: bool,
    /// The first detail line shown.
    offset: usize,
}

impl List {
    fn new(title: &str, subtitle: &str, keys: Keys, items: Vec<Item>, initial: usize) -> Self {
        Self {
            title: title.into(), subtitle: subtitle.into(), keys, items, query: Draft::default(),
            selected: initial, top: Cell::new(0), detail: false, back: true, offset: 0,
        }
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

    /// Title, subtitle (when there is one) and the blank row under them.
    fn header_rows(&self) -> usize {
        if self.subtitle.is_empty() { 2 } else { 3 }
    }

    /// Detail lines shown at once in a panel `height` rows high.
    fn detail_rows(&self, height: usize) -> usize {
        height.saturating_sub(self.header_rows() + 3).max(1)
    }

    fn detail_lines(&self, room: usize) -> Vec<String> {
        let lines = self.current().map(|i| {
            let item = &self.items[i];
            [&item.label, &item.description].into_iter().filter(|s| !s.is_empty()).cloned().chain(item.details.clone()).collect::<Vec<_>>()
        }).unwrap_or_default();
        lines.iter().flat_map(|line| line.split('\n')).flat_map(|line| text::wrap(&text::printable(line), room)).collect()
    }

    fn handle(&mut self, key: Key) -> Option<Option<usize>> {
        if self.detail {
            let count = self.detail_rows(text::terminal_height().unwrap_or(24));
            let last = self.detail_lines(room(text::terminal_width())).len().saturating_sub(count);
            self.offset = self.offset.min(last);
            match key {
                Key::Esc | Key::Tab | Key::Left if self.back => { self.detail = false; self.offset = 0; }
                Key::Esc | Key::Tab | Key::Left | Key::Eof => return Some(None),
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

    fn render_detail(&self, mut rows: Vec<String>, room: usize, height: usize, paint: &dyn Fn(&str, &str) -> String) -> Frame {
        let row = |content: String| format!("  {}", text::clip(&content, room));
        let lines = self.detail_lines(room);
        let count = self.detail_rows(height);
        let start = self.offset.min(lines.len().saturating_sub(count));
        for line in lines.iter().skip(start).take(count) { rows.push(row(line.clone())); }
        if lines.is_empty() { rows.push(row(paint(DIM, "No details"))); }
        let range = if lines.len() > count {
            format!("{}–{} of {} lines", start + 1, (start + count).min(lines.len()), lines.len())
        } else { String::new() };
        rows.push(row(paint(DIM, &range)));
        let keys = if self.back { format!("↑/↓ scroll · Tab/Esc back · Enter {}", self.keys.enter) } else { "↑/↓ scroll · Enter/Esc close".into() };
        rows.push(row(paint(DIM, &keys)));
        (rows, None)
    }
}

impl Panel for List {
    fn render(&self, width: usize, height: usize, paint: &dyn Fn(&str, &str) -> String) -> Frame {
        let room = room(width);
        let row = |content: String| format!("  {}", text::clip(&content, room));
        let mut rows = vec![row(paint(ACCENT, &text::fit(&text::printable(&self.title), room)))];
        if !self.subtitle.is_empty() { rows.push(row(paint(DIM, &text::fit(&text::printable(&self.subtitle), room)))); }
        rows.push(String::new());
        if self.detail {
            return self.render_detail(rows, room, height, paint);
        }
        let query = if self.query.is_empty() { paint(DIM, "type to filter") } else { text::printable(self.query.text()) };
        let cursor = (rows.len(), (10 + text::width(&self.query.text()[..self.query.position()])).min(room + 1));
        rows.push(row(format!("Search: {query}")));

        // Sizes come from all the items, not the filtered ones, so the panel
        // keeps its shape while the selection moves or the filter changes.
        let label_width = self.items.iter().map(|i| text::width(&text::printable(&i.label))).max().unwrap_or(0).min(room / 2);
        let description_room = room.saturating_sub(label_width + 4);
        let beside = description_room >= MIN_DESCRIPTION;
        let fits = |item: &Item| item.description.is_empty()
            || (beside && !item.description.contains('\n') && text::width(&text::printable(&item.description)) <= description_room);
        let summary_rows = if self.items.iter().all(fits) { 0 } else { SUMMARY_ROWS };
        let count = height.saturating_sub(rows.len() + summary_rows + 4).clamp(1, LIST_ROWS).min(self.items.len().max(1));

        let filtered = self.filtered();
        let selected = self.selected.min(filtered.len().saturating_sub(1));
        let start = window(&self.top, selected, count, filtered.len());
        for (position, i) in filtered.iter().enumerate().skip(start).take(count) {
            let item = &self.items[*i];
            let (mark, style) = if position == selected { ("❯", ACCENT) } else { (" ", PLAIN) };
            let label = text::fit(&text::printable(&item.label), if beside { label_width } else { room.saturating_sub(2) });
            let mut line = paint(style, &format!("{mark} {}", if beside { text::pad(&label, label_width) } else { label }));
            if beside && !item.description.is_empty() {
                let first = item.description.lines().next().unwrap_or("");
                line = format!("{line}  {}", paint(DIM, &text::fit(&text::printable(first), description_room)));
            }
            rows.push(row(line));
        }
        if filtered.is_empty() { rows.push(row(paint(DIM, if self.items.is_empty() { "No items available" } else { "No matching items" }))); }
        while rows.len() < self.header_rows() + 1 + count { rows.push(String::new()); }
        rows.push(String::new());

        let summary: Vec<String> = self.current().map(|i| &self.items[i]).filter(|item| !fits(item)).map(|item| {
            item.description.split('\n').flat_map(|s| text::wrap(&text::printable(s), room)).take(summary_rows).collect()
        }).unwrap_or_default();
        for i in 0..summary_rows { rows.push(summary.get(i).map_or_else(String::new, |line| row(paint(DIM, line)))); }

        let shown = if filtered.len() > count {
            format!("{}–{} of {}", start + 1, start + count, filtered.len())
        } else { filtered.len().to_string() };
        let noun = if self.query.is_empty() { if filtered.len() == 1 { " item" } else { " items" } } else { " matching" };
        rows.push(row(paint(DIM, &format!("{shown}{noun} · ↑/↓ select · Tab details"))));
        rows.push(row(paint(DIM, &format!("Enter {} · Esc {}", self.keys.enter, self.keys.esc))));
        (rows, Some(cursor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: Keys = Keys { enter: "select", esc: "cancel" };

    fn list() -> List {
        List::new("Test", "Choose", KEYS, vec![
            Item::new("alpha", "mentions beta").details(vec!["Long details".repeat(100)]), Item::new("beta", "second"),
        ], 0)
    }

    fn plain(list: &List, width: usize, height: usize) -> Vec<String> {
        list.render(width, height, &|_, s| s.into()).0
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
                let rows = plain(&list, width, height);
                assert!(rows.len() < height);
                assert!(rows.iter().all(|r| text::width(r) < width));
            }
        }
    }

    #[test]
    fn descriptions_sit_beside_labels_when_there_is_room() {
        let list = list();
        let rows = plain(&list, 80, 24);
        assert!(rows.iter().any(|r| r.contains("❯ alpha  mentions beta")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("  beta   second")), "{rows:?}");
        let narrow = plain(&list, 20, 24);
        assert!(!narrow.iter().any(|r| r.contains("second")), "no room beside the labels");
        assert!(narrow.iter().any(|r| r.contains("mentions beta")), "the selected description goes underneath");
    }

    #[test]
    fn the_window_moves_only_when_the_selection_leaves_it() {
        let items = (0..20).map(|i| Item::new(format!("item{i:02}"), "")).collect();
        let mut list = List::new("Test", "", KEYS, items, 0);
        let first_item = |list: &List| plain(list, 80, 24).iter().find_map(|r| r.find("item").map(|at| r[at..at + 6].to_string())).unwrap();
        for _ in 0..10 { list.handle(Key::Down); }
        assert_eq!(first_item(&list), "item03", "item10 at the bottom of eight rows");
        list.handle(Key::Up);
        assert_eq!(first_item(&list), "item03", "item09 is still shown, nothing moves");
        for _ in 0..7 { list.handle(Key::Up); }
        assert_eq!(first_item(&list), "item02");
    }

    #[test]
    fn the_panel_keeps_its_height_while_filtering_and_moving() {
        let mut list = list();
        let height = plain(&list, 80, 24).len();
        list.handle(Key::Down);
        assert_eq!(plain(&list, 80, 24).len(), height);
        list.handle(Key::Text("beta".into()));
        assert_eq!(plain(&list, 80, 24).len(), height);
        list.handle(Key::Text("zzz".into()));
        assert_eq!(plain(&list, 80, 24).len(), height);
    }

    #[test]
    fn a_document_closes_on_left_instead_of_turning_into_a_list() {
        let mut doc = List::new("Doc", "", KEYS, vec![Item::new("", "").details(vec!["text".into()])], 0);
        (doc.detail, doc.back) = (true, false);
        assert_eq!(doc.handle(Key::Left), Some(None));
        assert_eq!(doc.handle(Key::Esc), Some(None));
        assert_eq!(doc.handle(Key::Enter), Some(Some(0)));
        assert!(plain(&doc, 80, 24)[2].starts_with("  text"), "the empty label and description add no lines");
    }
}
