//! Shared keyboard-driven panels for slash commands, kept out of scrollback.
//!
//! Every panel is framed the way the input box is: a rule, which carries
//! the panel's title (and where a long list or text is scrolled to), the
//! content, a rule, and one line of the keys that do something now.

use std::cell::Cell;
use std::sync::{Arc, Mutex};

use super::{Output, text};
use super::input::Draft;
use super::keys::{Key, KeyWatch};
use super::style::{ACCENT, BOLD, DIM, GREEN, PLAIN, RED};

pub type Frame = (Vec<String>, Option<(usize, usize)>);
pub(super) type Paint<'a> = &'a dyn Fn(&str, &str) -> String;

pub(super) trait Panel: Send {
    fn render(&self, width: usize, height: usize, paint: Paint) -> Frame;
}

/// What stands before an item's label, in a column of its own.
#[derive(Clone, Copy, Default, PartialEq)]
pub enum Mark {
    #[default]
    None,
    /// The choice in effect, or something that ended well.
    Check,
    /// Something on or running.
    Dot,
    /// Something that failed.
    Cross,
    /// Something off or over.
    Ring,
}

impl Mark {
    fn glyph(self) -> (&'static str, &'static str) {
        match self {
            Mark::None => (" ", PLAIN),
            Mark::Check => ("✓", GREEN),
            Mark::Dot => ("●", GREEN),
            Mark::Cross => ("✗", RED),
            Mark::Ring => ("○", DIM),
        }
    }
}

#[derive(Clone, Default)]
pub struct Item {
    pub label: String,
    /// Shown dim after the label: the arguments a command takes, say.
    pub hint: String,
    pub description: String,
    /// The whole of what `description` sums up, for the item's details.
    pub about: String,
    pub mark: Mark,
    /// Named values, shown aligned in the item's details.
    pub fields: Vec<(String, String)>,
    /// The heading over `details`.
    pub section: String,
    pub details: Vec<String>,
}

impl Item {
    pub fn new(label: impl Into<String>, description: impl Into<String>) -> Self {
        Self { label: label.into(), description: description.into(), ..Self::default() }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = hint.into();
        self
    }

    pub fn about(mut self, about: impl Into<String>) -> Self {
        self.about = about.into();
        self
    }

    /// What says the most about the item: `about`, or else the description.
    fn whole(&self) -> &str {
        if self.about.is_empty() { &self.description } else { &self.about }
    }

    pub fn mark(mut self, mark: Mark) -> Self {
        self.mark = mark;
        self
    }

    pub fn field(mut self, name: &str, value: impl Into<String>) -> Self {
        self.fields.push((name.into(), value.into()));
        self
    }

    pub fn section(mut self, heading: &str) -> Self {
        self.section = heading.into();
        self
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

/// The keys of a list to look through.
const BROWSE: Keys = Keys { enter: "details", esc: "close" };

/// Most items a list shows at once. A longer list scrolls, and filters by
/// what is typed.
pub(super) const LIST_ROWS: usize = 10;
/// Rows under a list for a description too long for its row.
const PREVIEW_ROWS: usize = 2;
/// Narrowest description worth showing beside the labels.
const MIN_DESCRIPTION: usize = 16;
/// Widest column of field names.
const FIELD_NAMES: usize = 18;
/// Rows of the terminal a panel leaves to the conversation above it.
const MARGIN: usize = 3;

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

/// Asks for one of `items`; `None` if none was taken.
pub async fn choose(out: &Output, title: &str, subtitle: &str, keys: Keys, items: Vec<Item>, initial: usize) -> Option<usize> {
    run(out, List::new(title, subtitle, keys, items, initial), List::handle).await
}

/// Shows `items` to look through: Enter opens the selected one's details.
/// Without items, the panel says `empty`.
pub async fn inspect(out: &Output, title: &str, subtitle: &str, empty: &str, items: Vec<Item>) {
    let _ = run(out, List::new(title, subtitle, Keys { enter: "details", esc: "close" }, items, 0).browse(empty), List::handle).await;
}

/// Shows `lines`, which may be styled, to read and scroll through.
pub async fn sheet(out: &Output, title: &str, lines: Vec<String>) {
    let _ = run(out, Sheet::new(title, lines), Sheet::handle).await;
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

/// Columns a panel's rules span: one short of the terminal's, so they
/// never wrap.
pub(super) fn span(width: usize) -> usize {
    width.saturating_sub(1).max(4)
}

/// Columns of a panel's content in this terminal, inside its margin.
pub(super) fn room() -> usize {
    span(text::terminal_width()) - 2
}

/// The rule a panel starts with, `── Title ───── right ──`: `right` says
/// where a list or text is scrolled to, and is left out when there is no
/// room for it.
pub(super) fn heading(title: &str, right: &str, width: usize, paint: Paint) -> String {
    let title = text::fit(&text::printable(title), width.saturating_sub(5).max(1));
    let used = text::width(&title) + 4;
    let right = if right.is_empty() || used + text::width(right) + 6 > width { String::new() } else { format!(" {right} ──") };
    let fill = "─".repeat(width.saturating_sub(used + text::width(&right)));
    text::clip(&format!("{} {} {}", paint(DIM, "──"), paint(ACCENT, &title), paint(DIM, &format!("{fill}{right}"))), width)
}

/// The rule under a panel's content.
pub(super) fn rule(width: usize, paint: Paint) -> String {
    paint(DIM, &"─".repeat(width))
}

/// A content row, inside the panel's two-column margin.
pub(super) fn row(content: &str, room: usize) -> String {
    if content.is_empty() { String::new() } else { format!("  {}", text::clip(content, room)) }
}

/// The line a panel ends with: each key and, dim, what it does. Without
/// room for all of them, the first ones (which move around) go.
pub(super) fn hints(keys: &[(&str, &str)], room: usize, paint: Paint) -> String {
    let mut keys: Vec<_> = keys.iter().filter(|(key, _)| !key.is_empty()).collect();
    let width = |keys: &[&(&str, &str)]| keys.iter().map(|(key, what)| text::width(key) + text::width(what) + 4).sum::<usize>();
    while keys.len() > 1 && width(&keys) > room + 3 {
        keys.remove(0);
    }
    let parts: Vec<String> = keys.iter().map(|(key, what)| format!("{key} {}", paint(DIM, what))).collect();
    row(&parts.join(&paint(DIM, " · ")), room)
}

/// A row of alternatives, one of them in effect: `● this  ○ that`.
pub(super) fn radio<S: AsRef<str>>(names: &[S], active: usize, paint: Paint) -> String {
    let names = names.iter().enumerate().map(|(i, name)| {
        if i == active { paint(ACCENT, &format!("● {}", name.as_ref())) } else { paint(DIM, &format!("○ {}", name.as_ref())) }
    });
    names.collect::<Vec<_>>().join("  ")
}

/// `fields` as rows, the names dim in a column and the values wrapped
/// beside them.
pub(super) fn fields(fields: &[(String, String)], room: usize, paint: Paint) -> Vec<String> {
    let names = fields.iter().map(|(name, _)| text::width(name)).max().unwrap_or(0).min(FIELD_NAMES);
    let mut rows = Vec::new();
    for (name, value) in fields {
        let name = text::pad(&text::fit(&text::printable(name), names), names);
        let values = value.split('\n').flat_map(|line| text::wrap(&text::printable(line), room.saturating_sub(names + 2).max(1)));
        for (i, value) in values.enumerate() {
            let name = if i == 0 { paint(DIM, &name) } else { " ".repeat(names) };
            rows.push(format!("{name}  {value}"));
        }
    }
    rows
}

/// Moves `offset` into `last + 1` positions for a key that scrolls, a page
/// being `page` rows; false if `key` isn't one.
fn scroll(offset: &mut usize, key: &Key, last: usize, page: usize) -> bool {
    *offset = match key {
        Key::Up => offset.saturating_sub(1),
        Key::Down => *offset + 1,
        Key::PageUp => offset.saturating_sub(page),
        Key::PageDown => *offset + page,
        Key::Home => 0,
        Key::End => last,
        _ => return false,
    }.min(last);
    true
}

/// A panel of `lines` to read, from `offset`: what a sheet and an item's
/// details look like.
fn page(
    title: &str, tabs: Option<&(Vec<String>, usize)>, lines: &[String], offset: usize, keys: &[(&str, &str)],
    (width, height): (usize, usize), paint: Paint,
) -> Frame {
    let (width, count) = (span(width), page_rows(tabs.is_some(), height));
    let room = width - 2;
    let start = offset.min(lines.len().saturating_sub(count));
    let range = if lines.len() > count {
        format!("{}–{} of {}", start + 1, (start + count).min(lines.len()), lines.len())
    } else { String::new() };
    let mut rows = vec![heading(title, &range, width, paint)];
    if let Some((names, active)) = tabs { rows.extend([row(&radio(names, *active, paint), room), String::new()]); }
    rows.extend(lines.iter().skip(start).take(count).map(|line| row(line, room)));
    rows.push(rule(width, paint));
    let scrolls = if lines.len() > count { "↑↓" } else { "" };
    let keys: Vec<_> = [(scrolls, "scroll")].into_iter().chain(keys.iter().copied()).collect();
    rows.push(hints(&keys, room, paint));
    (rows, None)
}

/// Lines a [`page`] shows at once in a terminal `height` rows high.
fn page_rows(tabs: bool, height: usize) -> usize {
    height.saturating_sub(3 + if tabs { 2 } else { 0 } + MARGIN).max(1)
}

/// What Tab does in a panel with tabs: goes to the next one.
fn tab_hint(tabs: Option<&(Vec<String>, usize)>) -> (&'static str, String) {
    match tabs {
        Some((names, active)) => ("tab", names[(active + 1) % names.len()].to_lowercase()),
        None => ("", String::new()),
    }
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
    fn render(&self, width: usize, _height: usize, paint: Paint) -> Frame {
        let width = span(width);
        let room = width - 2;
        let rows = vec![
            heading(&self.title, "", width, paint),
            row(&paint(DIM, &text::printable(&self.prompt)), room),
            row(&format!("{} {}", paint(ACCENT, "❯"), text::printable(self.draft.text())), room),
            rule(width, paint),
            hints(&[("enter", "continue"), ("esc", "cancel")], room, paint),
        ];
        let cursor = (2, (4 + text::width(&self.draft.text()[..self.draft.position()])).min(room + 1));
        (rows, Some(cursor))
    }
}

/// Text to read and scroll through.
pub(super) struct Sheet {
    title: String,
    lines: Vec<String>,
    offset: usize,
    tabs: Option<(Vec<String>, usize)>,
}

impl Sheet {
    pub(super) fn new(title: &str, lines: Vec<String>) -> Self {
        Self { title: title.into(), lines, offset: 0, tabs: None }
    }

    /// Shows `names` over the text, the one at `active` as this panel's.
    pub(super) fn tabs(mut self, names: &[&str], active: usize) -> Self {
        self.tabs = Some((names.iter().map(|name| name.to_string()).collect(), active));
        self
    }

    fn wrapped(&self, room: usize) -> Vec<String> {
        self.lines.iter().flat_map(|line| line.split('\n')).flat_map(|line| text::wrap(line, room)).collect()
    }

    pub(super) fn handle(&mut self, key: Key) -> Option<Option<()>> {
        let count = page_rows(self.tabs.is_some(), text::terminal_height().unwrap_or(24));
        let last = self.wrapped(span(text::terminal_width()) - 2).len().saturating_sub(count);
        self.offset = self.offset.min(last);
        if !scroll(&mut self.offset, &key, last, count) && matches!(key, Key::Esc | Key::Enter | Key::Left | Key::Eof) {
            return Some(None);
        }
        None
    }
}

impl Panel for Sheet {
    fn render(&self, width: usize, height: usize, paint: Paint) -> Frame {
        let tab = tab_hint(self.tabs.as_ref());
        let keys = [(tab.0, tab.1.as_str()), ("esc", "close")];
        page(&self.title, self.tabs.as_ref(), &self.wrapped(span(width) - 2), self.offset, &keys, (width, height), paint)
    }
}

pub(super) struct List {
    title: String,
    subtitle: String,
    keys: Keys,
    items: Vec<Item>,
    /// What the panel says when there are no items.
    empty: String,
    /// Whether Enter opens the selected item's details rather than take it.
    browse: bool,
    tabs: Option<(Vec<String>, usize)>,
    query: Draft,
    /// Position within the filtered list; chosen values use the original index.
    selected: usize,
    /// The first filtered item shown.
    top: Cell<usize>,
    detail: bool,
    /// The first detail line shown.
    offset: usize,
}

/// Where a list's columns are. They come from all the items, not the
/// filtered ones, so the panel keeps its shape while the selection moves or
/// the filter changes.
struct Layout {
    /// Whether the items have a column of marks.
    marks: bool,
    /// Columns of a label and its hint.
    label: usize,
    /// Columns of a description beside the labels, if they go there.
    description: Option<usize>,
}

impl Layout {
    /// Columns before the label: the selection's `❯` and the mark.
    fn lead(&self) -> usize {
        if self.marks { 4 } else { 2 }
    }

    /// Whether all there is to say about `item` shows on its row.
    fn whole(&self, item: &Item) -> bool {
        item.about.is_empty() && (item.description.is_empty() || self.description.is_some_and(|room| {
            !item.description.contains('\n') && text::width(&text::printable(&item.description)) <= room
        }))
    }
}

impl List {
    pub(super) fn new(title: &str, subtitle: &str, keys: Keys, items: Vec<Item>, initial: usize) -> Self {
        Self {
            title: title.into(), subtitle: subtitle.into(), keys, items, empty: "Nothing to show".into(), browse: false,
            tabs: None, query: Draft::default(), selected: initial, top: Cell::new(0), detail: false, offset: 0,
        }
    }

    pub(super) fn browse(mut self, empty: &str) -> Self {
        (self.browse, self.empty, self.keys) = (true, empty.into(), BROWSE);
        self
    }

    /// Shows `names` over the list, the one at `active` as this panel's.
    /// Tab is then left to whoever switches between them.
    pub(super) fn tabs(mut self, names: &[&str], active: usize) -> Self {
        self.tabs = Some((names.iter().map(|name| name.to_string()).collect(), active));
        self
    }

    /// Whether typing filters the list: when it is too long to show whole.
    fn filterable(&self) -> bool {
        self.items.len() > LIST_ROWS
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

    fn layout(&self, room: usize) -> Layout {
        let marks = self.items.iter().any(|item| item.mark != Mark::None);
        let cell = |item: &Item| {
            let hint = if item.hint.is_empty() { 0 } else { text::width(&text::printable(&item.hint)) + 1 };
            text::width(&text::printable(&item.label)) + hint
        };
        let mut layout = Layout { marks, label: 0, description: None };
        let widest = self.items.iter().map(cell).max().unwrap_or(0).min(room / 2);
        let beside = room.saturating_sub(layout.lead() + widest + 2);
        let described = self.items.iter().any(|item| !item.description.is_empty());
        if described && beside >= MIN_DESCRIPTION {
            (layout.label, layout.description) = (widest, Some(beside));
        } else {
            layout.label = room.saturating_sub(layout.lead());
        }
        layout
    }

    /// Rows under the list for the selected item's description, when some
    /// item's doesn't fit on its row.
    fn preview_rows(&self, layout: &Layout) -> usize {
        if self.items.iter().all(|item| layout.whole(item)) { 0 } else { PREVIEW_ROWS }
    }

    /// Items shown at once in a panel `height` rows high.
    fn count(&self, layout: &Layout, height: usize) -> usize {
        let preview = match self.preview_rows(layout) { 0 => 0, rows => rows + 1 };
        let chrome = 3 + if self.tabs.is_some() { 2 } else { 0 } + if self.subtitle.is_empty() { 0 } else { 2 }
            + usize::from(self.filterable()) + preview;
        height.saturating_sub(chrome + MARGIN).clamp(1, LIST_ROWS).min(self.items.len().max(1))
    }

    /// Whether `item` has more to show than its row and the preview do.
    fn expandable(&self, item: &Item, layout: &Layout, room: usize) -> bool {
        let shown = text::wrap(&text::printable(item.whole()), room).len() <= PREVIEW_ROWS && !item.whole().contains('\n');
        !item.fields.is_empty() || !item.details.is_empty() || !(layout.whole(item) || shown)
    }

    fn detail_lines(&self, room: usize, paint: Paint) -> Vec<String> {
        let Some(item) = self.current().map(|i| &self.items[i]) else { return Vec::new() };
        let wrapped = |text: &str| text.split('\n').flat_map(|line| text::wrap(&text::printable(line), room)).collect::<Vec<_>>();
        let mut lines = Vec::new();
        if !item.whole().is_empty() { lines.extend(wrapped(item.whole())); }
        let mut part = |part: Vec<String>| {
            if !part.is_empty() && !lines.is_empty() { lines.push(String::new()); }
            lines.extend(part);
        };
        part(fields(&item.fields, room, paint));
        let heading = (!item.section.is_empty() && !item.details.is_empty()).then(|| paint(BOLD, &text::printable(&item.section)));
        part(heading.into_iter().chain(item.details.iter().flat_map(|line| wrapped(line))).collect());
        lines
    }

    pub(super) fn handle(&mut self, key: Key) -> Option<Option<usize>> {
        let (room, height) = (span(text::terminal_width()) - 2, text::terminal_height().unwrap_or(24));
        if self.detail {
            let count = page_rows(self.tabs.is_some(), height);
            let last = self.detail_lines(room, &|_, s| s.into()).len().saturating_sub(count);
            self.offset = self.offset.min(last);
            match key {
                Key::Esc | Key::Tab | Key::Left => { self.detail = false; self.offset = 0; }
                Key::Eof => return Some(None),
                Key::Enter if self.browse => { self.detail = false; self.offset = 0; }
                Key::Enter => return self.current().map(Some),
                key => { scroll(&mut self.offset, &key, last, count); }
            }
            return None;
        }
        let layout = self.layout(room);
        let open = |list: &mut Self| {
            list.detail = list.current().is_some_and(|i| list.expandable(&list.items[i], &layout, room));
            list.offset = 0;
        };
        match key {
            Key::Esc | Key::Eof => return Some(None),
            Key::Enter if self.browse => open(self),
            Key::Enter => return self.current().map(Some),
            Key::Tab | Key::Right => open(self),
            Key::Up | Key::Down | Key::PageUp | Key::PageDown => {
                let count = self.filtered().len();
                if count > 0 {
                    let selected = self.selected.min(count - 1);
                    let page = self.count(&layout, height);
                    self.selected = match key {
                        Key::Up => selected.checked_sub(1).unwrap_or(count - 1),
                        Key::Down => (selected + 1) % count,
                        Key::PageUp => selected.saturating_sub(page),
                        _ => (selected + page).min(count - 1),
                    };
                }
            }
            key if self.filterable() && !matches!(key, Key::Newline | Key::PasteImage | Key::Position(_)) => {
                self.query.edit(key);
                self.selected = 0;
            }
            _ => {}
        }
        None
    }

    fn render_detail(&self, width: usize, height: usize, paint: Paint) -> Frame {
        let label = self.current().map(|i| self.items[i].label.as_str()).filter(|label| !label.is_empty());
        let title = label.map_or_else(|| self.title.clone(), |label| format!("{} › {label}", self.title));
        let enter = if self.browse { ("", "") } else { ("enter", self.keys.enter) };
        let keys = [enter, ("esc", "back")];
        let lines = self.detail_lines(span(width) - 2, paint);
        page(&title, self.tabs.as_ref(), &lines, self.offset, &keys, (width, height), paint)
    }
}

impl Panel for List {
    fn render(&self, width: usize, height: usize, paint: Paint) -> Frame {
        if self.detail {
            return self.render_detail(width, height, paint);
        }
        let width = span(width);
        let room = width - 2;
        let layout = self.layout(room);
        let count = self.count(&layout, height);
        let filtered = self.filtered();
        let selected = self.selected.min(filtered.len().saturating_sub(1));
        let start = window(&self.top, selected, count, filtered.len());

        let position = if filtered.len() > count {
            format!("{}–{} of {}", start + 1, start + count, filtered.len())
        } else if !self.query.is_empty() {
            format!("{} of {}", filtered.len(), self.items.len())
        } else { String::new() };
        let mut rows = vec![heading(&self.title, &position, width, paint)];
        if let Some((names, active)) = &self.tabs { rows.extend([row(&radio(names, *active, paint), room), String::new()]); }
        if !self.subtitle.is_empty() {
            rows.extend([row(&paint(DIM, &text::fit(&text::printable(&self.subtitle), room)), room), String::new()]);
        }
        let mut cursor = None;
        if self.filterable() {
            let query = if self.query.is_empty() { paint(DIM, "type to filter") } else { text::printable(self.query.text()) };
            cursor = Some((rows.len(), (4 + text::width(&self.query.text()[..self.query.position()])).min(room + 1)));
            rows.push(row(&format!("{} {query}", paint(DIM, "⌕")), room));
        }

        let list_at = rows.len();
        for (position, i) in filtered.iter().enumerate().skip(start).take(count) {
            let item = &self.items[*i];
            let chosen = position == selected;
            let mut line = if chosen { paint(ACCENT, "❯ ") } else { "  ".to_string() };
            if layout.marks {
                let (glyph, style) = item.mark.glyph();
                line.push_str(&format!("{} ", paint(style, glyph)));
            }
            let label = text::fit(&text::printable(&item.label), layout.label);
            let left = layout.label - text::width(&label);
            let hint = if item.hint.is_empty() || left < 3 { String::new() } else { format!(" {}", text::fit(&text::printable(&item.hint), left - 1)) };
            let pad = left - text::width(&hint);
            line.push_str(&paint(if chosen { ACCENT } else { PLAIN }, &label));
            if !hint.is_empty() { line.push_str(&paint(DIM, &hint)); }
            if let Some(room) = layout.description && !item.description.is_empty() {
                let first = text::fit(&text::printable(item.description.lines().next().unwrap_or("")), room);
                line.push_str(&format!("{}  {}", " ".repeat(pad), paint(if chosen { PLAIN } else { DIM }, &first)));
            }
            rows.push(row(&line, room));
        }
        if filtered.is_empty() {
            rows.push(row(&paint(DIM, if self.items.is_empty() { &self.empty } else { "No matches" }), room));
        }
        while rows.len() < list_at + count { rows.push(String::new()); }

        let current = self.current().map(|i| &self.items[i]);
        let preview_rows = self.preview_rows(&layout);
        if preview_rows > 0 {
            let mut preview: Vec<String> = current.filter(|item| !layout.whole(item)).map(|item| {
                item.whole().split('\n').flat_map(|s| text::wrap(&text::printable(s), room)).collect()
            }).unwrap_or_default();
            if preview.len() > preview_rows {
                preview.truncate(preview_rows);
                let last = &mut preview[preview_rows - 1];
                *last = format!("{}…", text::fit(last, room.saturating_sub(1)).trim_end_matches('…'));
            }
            rows.push(String::new());
            for i in 0..preview_rows { rows.push(preview.get(i).map_or_else(String::new, |line| row(&paint(DIM, line), room))); }
        }

        rows.push(rule(width, paint));
        let expandable = current.is_some_and(|item| self.expandable(item, &layout, room));
        let moves = if filtered.len() > 1 { "↑↓" } else { "" };
        let details = if expandable && !self.browse { "→" } else { "" };
        let enter = if current.is_none() || (self.browse && !expandable) { "" } else { "enter" };
        let tab = tab_hint(self.tabs.as_ref());
        let keys = [(moves, "select"), (details, "details"), (enter, self.keys.enter), (tab.0, tab.1.as_str()), ("esc", self.keys.esc)];
        rows.push(hints(&keys, room, paint));
        (rows, cursor)
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

    fn many(count: usize) -> List {
        List::new("Test", "", KEYS, (0..count).map(|i| Item::new(format!("item{i:02}"), "")).collect(), 0)
    }

    fn plain<P: Panel>(panel: &P, width: usize, height: usize) -> Vec<String> {
        panel.render(width, height, &|_, s| s.into()).0.iter().map(|row| text::strip_ansi(row)).collect()
    }

    #[test]
    fn a_panel_is_framed_by_its_title_and_its_keys() {
        let rows = plain(&list(), 60, 24);
        assert_eq!(rows[0], format!("── Test {}", "─".repeat(51)));
        assert_eq!(rows[1], "  Choose");
        assert_eq!(rows[rows.len() - 2], "─".repeat(59));
        assert_eq!(rows[rows.len() - 1], "  ↑↓ select · → details · enter select · esc cancel");
        assert!(!rows.iter().any(|r| r.contains('⌕')), "a list shown whole has no filter");
        let narrow = plain(&list(), 30, 24);
        assert_eq!(narrow[narrow.len() - 1], "  enter select · esc cancel", "the keys that move go first");
    }

    #[test]
    fn long_lists_filter_and_return_original_indexes() {
        let mut list = many(12);
        assert!(plain(&list, 80, 24).iter().any(|r| r == "  ⌕ type to filter"));
        list.handle(Key::Text("ITEM07".into()));
        let rows = plain(&list, 80, 24);
        assert!(rows[0].ends_with(" 1 of 12 ──"), "{rows:?}");
        assert_eq!(list.handle(Key::Enter), Some(Some(7)));
        list.handle(Key::Text("unknown".into()));
        assert!(plain(&list, 80, 24).iter().any(|r| r == "  No matches"));
        assert_eq!(list.handle(Key::Enter), None);
        assert_eq!(list.handle(Key::Esc), Some(None));
    }

    #[test]
    fn short_lists_take_no_filter() {
        let mut list = list();
        list.handle(Key::Text("beta".into()));
        assert_eq!(list.handle(Key::Enter), Some(Some(0)));
    }

    #[test]
    fn details_scroll_then_return_to_selection_and_eof_cancels() {
        let mut list = list();
        list.handle(Key::Tab);
        assert!(list.detail);
        list.handle(Key::Down);
        assert_eq!(list.offset, 1);
        let rows = plain(&list, 80, 24);
        assert!(rows[0].starts_with("── Test › alpha ─") && rows[0].ends_with(" of 19 ──"), "{rows:?}");
        assert_eq!(rows[rows.len() - 1], "  ↑↓ scroll · enter select · esc back");
        list.handle(Key::Esc);
        assert!(!list.detail);
        list.handle(Key::Down);
        list.handle(Key::Right);
        assert!(!list.detail, "beta has nothing more to show");
        assert_eq!(list.handle(Key::Enter), Some(Some(1)));
        assert_eq!(list.handle(Key::Eof), Some(None));
        for (width, height) in [(80, 24), (36, 18)] {
            for detail in [false, true] {
                list.detail = detail;
                let rows = plain(&list, width, height);
                assert!(rows.len() < height);
                assert!(rows.iter().all(|r| text::width(r) < width), "{rows:?}");
            }
        }
    }

    #[test]
    fn descriptions_sit_beside_labels_when_there_is_room() {
        let list = list();
        let rows = plain(&list, 80, 24);
        assert!(rows.iter().any(|r| r == "  ❯ alpha  mentions beta"), "{rows:?}");
        assert!(rows.iter().any(|r| r == "    beta   second"), "{rows:?}");
        let narrow = plain(&list, 20, 24);
        assert!(!narrow.iter().any(|r| r.contains("second")), "no room beside the labels");
        assert!(narrow.iter().any(|r| r.contains("mentions beta")), "the selected description goes underneath");
    }

    #[test]
    fn marks_and_hints_have_columns_of_their_own() {
        let items = vec![Item::new("/model", "Choose the model").hint("[name]"), Item::new("/help", "Help").mark(Mark::Check)];
        let rows = plain(&List::new("Test", "", KEYS, items, 0), 80, 24);
        assert_eq!(rows[1..3], ["  ❯   /model [name]  Choose the model", "    ✓ /help          Help"]);
    }

    #[test]
    fn details_show_the_description_the_fields_and_the_text() {
        let item = Item::new("bash", "Runs a command").field("Access", "all").field("Source", "built-in")
            .section("Input schema").details(vec!["{}".into()]);
        let mut list = List::new("Tools", "", KEYS, vec![item.about("Runs a command, in bash")], 0).browse("No tools");
        assert_eq!(plain(&list, 80, 24)[1..5], ["  ❯ bash  Runs a command", "", "  Runs a command, in bash", ""], "what it sums up, underneath");
        assert_eq!(plain(&list, 80, 24).last().unwrap(), "  enter details · esc close");
        list.handle(Key::Enter);
        let rows = plain(&list, 80, 24);
        assert_eq!(rows[1..8], ["  Runs a command, in bash", "", "  Access  all", "  Source  built-in", "", "  Input schema", "  {}"]);
        assert_eq!(rows.last().unwrap(), "  esc back");
        list.handle(Key::Enter);
        assert!(!list.detail, "Enter in something to look through goes back");
        let rows = plain(&List::new("Tools", "", KEYS, Vec::new(), 0).browse("No tools"), 80, 24);
        assert_eq!(rows[1..], ["  No tools", &"─".repeat(79), "  esc close"]);
    }

    #[test]
    fn the_window_moves_only_when_the_selection_leaves_it() {
        let mut list = many(20);
        let first_item = |list: &List| plain(list, 80, 24).iter().find_map(|r| r.find("item").map(|at| r[at..at + 6].to_string())).unwrap();
        for _ in 0..12 { list.handle(Key::Down); }
        assert_eq!(first_item(&list), "item03", "item12 at the bottom of ten rows");
        assert!(plain(&list, 80, 24)[0].ends_with(" 4–13 of 20 ──"));
        list.handle(Key::Up);
        assert_eq!(first_item(&list), "item03", "item11 is still shown, nothing moves");
        for _ in 0..9 { list.handle(Key::Up); }
        assert_eq!(first_item(&list), "item02");
        list.handle(Key::PageDown);
        assert_eq!(list.selected, 12);
    }

    #[test]
    fn the_panel_keeps_its_height_while_filtering_and_moving() {
        let mut list = many(12);
        list.items[3].description = "long ".repeat(60);
        let height = plain(&list, 80, 24).len();
        for key in [Key::Down, Key::Down, Key::Down, Key::Text("item03".into()), Key::Text("zzz".into())] {
            list.handle(key);
            assert_eq!(plain(&list, 80, 24).len(), height);
        }
    }

    #[test]
    fn a_sheet_scrolls_and_closes() {
        let mut sheet = Sheet::new("Doc", (0..40).map(|i| format!("line {i}")).collect());
        let rows = plain(&sheet, 80, 24);
        assert!(rows[0].ends_with(" 1–18 of 40 ──") && rows[1] == "  line 0", "{rows:?}");
        assert_eq!(rows.last().unwrap(), "  ↑↓ scroll · esc close");
        assert!(rows.len() < 24);
        assert_eq!(Sheet::new("Doc", vec!["text".into()]).tabs(&["One", "Two"], 0).render(80, 24, &|_, s| s.into()).0[1..], [
            "  ● One  ○ Two\x1b[0m".to_string(), String::new(), "  text\x1b[0m".into(), "─".repeat(79), "  tab two · esc close\x1b[0m".into(),
        ]);
        assert_eq!(sheet.handle(Key::Down), None);
        assert_eq!(sheet.offset, 1);
        assert_eq!(sheet.handle(Key::Left), Some(None));
        assert_eq!(sheet.handle(Key::Esc), Some(None));
    }

    #[test]
    fn text_input_puts_the_cursor_after_what_was_typed() {
        let mut input = TextInput { title: "Add".into(), prompt: "Name".into(), draft: Draft::default() };
        assert_eq!(input.handle(Key::Enter), None, "nothing typed yet");
        input.handle(Key::Text("你a".into()));
        let (rows, cursor) = input.render(40, 24, &|_, s| s.into());
        assert_eq!(text::strip_ansi(&rows[2]), "  ❯ 你a");
        assert_eq!(cursor, Some((2, 7)));
        assert_eq!(input.handle(Key::Enter), Some(Some("你a".into())));
    }
}
