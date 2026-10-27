//! Terminal output: the shared handle on stdout, which prints the
//! conversation above a status spinner and the input box (or a panel) and
//! keeps those drawn under it.

use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::calls::call_label;
use super::input::{self, Queued, Typing};
use super::keys::Key;
use super::menu::Menu;
use super::screen::Screen;
use super::text;
use super::tool_output::{self, Record};
use super::transcript::{Entry, Transcript};
use crate::agent::Inbox;
use crate::llm::Message;
use crate::tools::ToolOutput;

/// Spinner glyphs, played forward and back.
const SPINNER: [&str; 6] = ["·", "✢", "✳", "✶", "✻", "✽"];
/// Spinner and status label color, and the shimmer that sweeps the label.
const STATUS_SGR: &str = "38;5;173";
const SHIMMER_SGR: &str = "1;38;5;216";
/// Wrap a redraw, so terminals that support it show it all at once.
const SYNC_START: &str = "\x1b[?2026h\x1b[?25l";
const SYNC_END: &str = "\x1b[?25h\x1b[?2026l";

/// Shared handle on stdout. Output printed while an agent runs goes through
/// it, so the status lines and input box are cleared first and redrawn
/// after.
#[derive(Clone)]
pub struct Output {
    state: Arc<Mutex<State>>,
    /// Whether stdout is a terminal that gets colors and a spinner.
    styled: bool,
}

struct State {
    screen: Option<Screen>,
    /// Keep Ctrl+O available across key-reader handoffs; restored on exit.
    screen_termios: Option<libc::termios>,
    menu: Option<Arc<Menu>>,
    panel: Option<Arc<Mutex<dyn super::dialog::Panel>>>,
    status: Option<Status>,
    tool_records: Vec<Record>,
    tool_panel: Option<Transcript>,
    transcript: Vec<Entry>,
    /// How many tool records there were at the last compaction.
    compacted_at: usize,
    /// The input box, while a turn runs in the REPL.
    input: Option<InputBox>,
    /// Whether the status or input box is on screen, on the last `rows`;
    /// the cursor is on `cursor_row` of them.
    drawn: bool,
    rows: usize,
    cursor_row: usize,
    size: (usize, Option<usize>),
    frame: usize,
    /// Lines printed while the line editor reads, shown once it is done:
    /// printing then would tear up the line being edited.
    held: Option<Vec<String>>,
    /// Nothing is drawn: stdout belongs to something else (JSON output).
    silent: bool,
    /// The status line under the input box.
    footer: Option<String>,
    /// A tool call waiting for the user's approval, in the input box's place.
    question: Option<super::approve::Question>,
    /// How many key readers hand keys to [`Output::question_key`] now.
    routed: usize,
    /// Where those readers hand keys while a panel is open over a turn.
    panel_keys: Option<tokio::sync::mpsc::UnboundedSender<Key>>,
    /// The conversation's permissions, whose mode shows under the box.
    permissions: Option<Arc<crate::permissions::Permissions>>,
}

/// Restores a usable shell cursor on normal exit and error unwinding.
pub struct ScreenGuard(Output);

/// A key reader handing keys to the questions, while it lives.
pub(super) struct RoutedKeys(Output);

impl Drop for RoutedKeys {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.routed = state.routed.saturating_sub(1);
    }
}

pub struct ScreenHandoff {
    out: Output,
    active: bool,
}

impl Drop for ScreenHandoff {
    fn drop(&mut self) {
        if self.active { self.out.claim_screen(); }
    }
}

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        self.0.leave_screen();
    }
}

struct Status {
    label: String,
    since: Instant,
    /// Shown dim on a second line: the latest reasoning, say.
    detail: String,
    /// Output tokens so far, once any have streamed.
    tokens: u64,
}

struct InputBox {
    typing: Typing,
    /// Where the messages sent from it go.
    inbox: Arc<Inbox>,
    notice: Option<String>,
}

impl Output {
    pub fn stdout() -> Self {
        let styled = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        let state = State {
            screen: None,
            screen_termios: None,
            menu: None,
            panel: None,
            status: None,
            tool_records: Vec::new(),
            tool_panel: None,
            transcript: Vec::new(),
            compacted_at: 0,
            input: None,
            drawn: false,
            rows: 0,
            cursor_row: 0,
            size: (0, None),
            frame: 0,
            held: None,
            silent: false,
            footer: None,
            question: None,
            routed: 0,
            panel_keys: None,
            permissions: None,
        };
        Self { state: Arc::new(Mutex::new(state)), styled }
    }

    /// Claims the current screen, pushing the shell contents into native
    /// scrollback without using the alternate screen or clearing history.
    pub fn enter_screen(&self) -> ScreenGuard {
        self.claim_screen();
        ScreenGuard(self.clone())
    }

    fn claim_screen(&self) {
        let mut state = self.state.lock().unwrap();
        if state.screen_termios.is_none() {
            let mut saved: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut saved) } == 0 {
                let mut mode = saved;
                // Otherwise macOS can consume Ctrl+O as VDISCARD between
                // a running turn's key reader and the next prompt's reader.
                mode.c_lflag &= !libc::IEXTEN;
                if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &mode) } == 0 {
                    state.screen_termios = Some(saved);
                }
            }
        }
        let height = text::terminal_height().unwrap_or(24);
        let mut stdout = io::stdout().lock();
        let _ = write!(stdout, "{}\x1b[H\x1b[J", "\r\n".repeat(height));
        let _ = stdout.flush();
        state.screen = Some(Screen::default());
    }

    pub fn handoff(&self) -> ScreenHandoff {
        let active = self.state.lock().unwrap().screen.is_some();
        self.leave_screen();
        ScreenHandoff { out: self.clone(), active }
    }

    pub fn interactive(&self) -> bool {
        io::stdout().is_terminal() && std::env::var("TERM").as_deref() != Ok("dumb")
    }

    pub fn input_is_empty(&self) -> bool {
        self.state.lock().unwrap().input.as_ref().is_some_and(|input| input.typing.draft.is_empty())
    }

    pub fn set_menu(&self, menu: Option<Arc<Menu>>) {
        let mut state = self.state.lock().unwrap();
        state.menu = menu;
        self.show(&mut state, "");
    }

    pub(super) fn set_panel(&self, panel: Option<Arc<Mutex<dyn super::dialog::Panel>>>) {
        let mut state = self.state.lock().unwrap();
        state.panel = panel;
        self.show(&mut state, "");
    }

    pub fn restore_tool_output(&self, messages: &[Message]) {
        let mut state = self.state.lock().unwrap();
        state.tool_records = tool_output::restore(messages);
        state.compacted_at = state.tool_records.len();
        state.transcript.clear();
        let mut index = 0;
        for message in messages {
            for block in &message.content {
                match block["type"].as_str() {
                    Some("tool_use") => { state.transcript.push(Entry::Tool(index)); index += 1; }
                    Some("text") if crate::agent::is_timestamp(block) => {}
                    Some("text" | "thinking") => {
                        let thinking = block["type"] == "thinking";
                        let content = block[if thinking { "thinking" } else { "text" }].as_str().unwrap_or("");
                        if !content.is_empty() {
                            let label = if thinking { "Thinking" } else if message.role == "user" { "❯ You" } else { "● Assistant" };
                            state.transcript.push(Entry::Text { label, content: content.into() });
                        }
                    }
                    _ => {}
                }
            }
        }
        state.tool_panel = None;
    }

    /// Marks a compaction in the expanded transcript with `note` and the
    /// tool calls made since the one before.
    pub(super) fn compaction_note(&self, note: &str) {
        let mut state = self.state.lock().unwrap();
        let since = state.compacted_at.min(state.tool_records.len());
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for record in &state.tool_records[since..] {
            match counts.iter_mut().find(|(n, _)| *n == record.name) {
                Some((_, count)) => *count += 1,
                None => counts.push((&record.name, 1)),
            }
        }
        counts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        let mut content = note.trim_start_matches("✻ ").to_string();
        if !counts.is_empty() {
            let listed: Vec<String> = counts.iter().map(|(name, count)| format!("{name} ×{count}")).collect();
            content.push_str(&format!("\nTool calls before it: {}", listed.join(", ")));
        }
        state.compacted_at = state.tool_records.len();
        state.transcript.push(Entry::Text { label: "✻ Compaction", content });
    }

    pub(super) fn transcript_text(&self, label: &'static str, content: &str, append: bool) {
        if content.is_empty() { return; }
        let mut state = self.state.lock().unwrap();
        if append && let Some(Entry::Text { label: previous, content: text }) = state.transcript.last_mut()
            && *previous == label
        { text.push_str(content); }
        else { state.transcript.push(Entry::Text { label, content: content.into() }); }
    }

    pub fn show_tool_output(&self, interactive: bool) {
        if interactive { self.tool_output_key(&Key::ToolOutput); }
        else {
            let items = tool_output::items(&self.state.lock().unwrap().tool_records);
            if items.is_empty() { self.line("No tool calls in this conversation"); }
            for item in items {
                self.line(item.label);
                self.line(item.description);
                self.lines(&item.details.iter().flat_map(|line| line.split('\n')).map(text::printable).collect::<Vec<_>>());
            }
        }
    }

    /// Hands `key` to the question waiting for an answer, if there is
    /// one; Shift+Tab goes to the next permission mode. True if it was
    /// taken.
    pub fn question_key(&self, key: &Key) -> bool {
        let mut state = self.state.lock().unwrap();
        if *key == Key::BackTab && let Some(permissions) = &state.permissions {
            permissions.set_mode(permissions.mode().next());
            self.show(&mut state, "");
            return true;
        }
        let Some(question) = state.question.as_mut() else { return false };
        if question.key(key) {
            state.question = None;
        }
        self.show(&mut state, "");
        true
    }

    pub(super) fn set_question(&self, question: Option<super::approve::Question>) {
        let mut state = self.state.lock().unwrap();
        state.question = question;
        self.show(&mut state, "");
    }

    /// Whether some key reader hands keys to the questions now.
    pub(super) fn keys_routed(&self) -> bool {
        self.state.lock().unwrap().routed > 0
    }

    /// Has the key readers counted by [`Output::route_keys`] hand keys to
    /// `keys` rather than read them itself, so a panel opens while
    /// a turn runs; false if no such reader runs.
    pub(super) fn borrow_keys(&self, keys: tokio::sync::mpsc::UnboundedSender<Key>) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.routed == 0 {
            return false;
        }
        state.panel_keys = Some(keys);
        true
    }

    pub(super) fn return_keys(&self) {
        self.state.lock().unwrap().panel_keys = None;
    }

    /// Hands `key` to the panel open over a turn; false if none is.
    pub(super) fn panel_key(&self, key: &Key) -> bool {
        let state = self.state.lock().unwrap();
        state.panel_keys.as_ref().is_some_and(|keys| keys.send(key.clone()).is_ok())
    }

    /// Counts a key reader that hands keys to [`Output::question_key`],
    /// until the guard is dropped.
    pub(super) fn route_keys(&self) -> RoutedKeys {
        self.state.lock().unwrap().routed += 1;
        RoutedKeys(self.clone())
    }

    /// Shows `permissions`' mode under the input box, and has Shift+Tab
    /// change it.
    pub fn set_permissions(&self, permissions: Arc<crate::permissions::Permissions>) {
        self.state.lock().unwrap().permissions = Some(permissions);
    }

    /// Routes inspector keys before composer editing or turn interruption.
    /// Uses the existing key reader, so opening it never pauses the agent.
    pub fn tool_output_key(&self, key: &Key) -> bool {
        let mut state = self.state.lock().unwrap();
        if key == &Key::ToolOutput && state.panel.is_none() {
            state.tool_panel = if state.tool_panel.is_some() { None }
                else { Some(Transcript::default()) };
        } else if let Some(mut panel) = state.tool_panel.take() {
            if !panel.key(key, &state.transcript, &state.tool_records,
                text::terminal_width(), text::terminal_height().unwrap_or(24)) { state.tool_panel = Some(panel); }
        } else { return false; }
        self.show(&mut state, "");
        true
    }

    /// Keeps a tool call for the expanded transcript; its index there.
    pub(super) fn record_call(&self, id: &str, name: &str, input: &Value) -> usize {
        let mut state = self.state.lock().unwrap();
        let index = state.tool_records.len();
        state.tool_records.push(Record::pending(id, name, input));
        state.transcript.push(Entry::Tool(index));
        self.show(&mut state, "");
        index
    }

    /// Completes the call kept at `index` with its `output` (`None` if it was
    /// interrupted); the call's name and input.
    pub(super) fn record_result(&self, index: usize, output: Option<&ToolOutput>) -> (String, Value) {
        let mut state = self.state.lock().unwrap();
        let record = &mut state.tool_records[index];
        if let Some(output) = output { record.complete(output); } else { record.interrupt(); }
        (record.name.clone(), record.input.clone())
    }

    /// Hands the terminal to a conventional line-based command prompt.
    pub fn leave_screen(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(mut viewport) = state.screen.take() {
            let frame = viewport.finish(text::terminal_width(), text::terminal_height().unwrap_or(24));
            let mut stdout = io::stdout().lock();
            let _ = write!(stdout, "{SYNC_START}{frame}\x1b[0m\x1b[?2004l{SYNC_END}");
            let _ = stdout.flush();
        }
        if let Some(saved) = state.screen_termios.take() {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved) };
        }
    }

    /// Shows the input box under the status, holding `typing`, until
    /// [`Output::close_input`]. Messages sent from it go to `inbox`.
    pub fn open_input(&self, typing: Typing, inbox: Arc<Inbox>) {
        let mut state = self.state.lock().unwrap();
        state.input = Some(InputBox { typing, inbox, notice: None });
        self.show(&mut state, "");
    }

    /// Removes the input box; what it held.
    pub fn close_input(&self) -> Typing {
        let mut state = self.state.lock().unwrap();
        let typing = state.input.take().map(|b| b.typing).unwrap_or_default();
        self.show(&mut state, "");
        typing
    }

    /// Changes what the input box holds, and redraws it; `None` if it isn't
    /// shown.
    pub fn edit_input<R>(&self, edit: impl FnOnce(&mut Typing) -> R) -> Option<R> {
        let mut state = self.state.lock().unwrap();
        let input = state.input.as_mut()?;
        input.notice = None;
        let result = edit(&mut input.typing);
        self.show(&mut state, "");
        Some(result)
    }

    pub fn input_notice(&self, notice: String) {
        let mut state = self.state.lock().unwrap();
        if let Some(input) = state.input.as_mut() { input.notice = Some(notice); }
        self.show(&mut state, "");
    }

    /// Puts the queued messages the model hasn't got yet back into the input
    /// box, to edit (see [`Typing::edit_queued`]); false if there were none.
    pub fn edit_queued(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        let Some(input) = state.input.as_mut() else { return false };
        let recalled = input.typing.edit_queued(&input.inbox);
        if recalled {
            self.show(&mut state, "");
        }
        recalled
    }

    /// Takes the queued messages the model has now been handed out of the
    /// input box.
    pub fn take_delivered(&self) -> Vec<Queued> {
        let mut state = self.state.lock().unwrap();
        let Some(input) = state.input.as_mut() else { return Vec::new() };
        let delivered = input.typing.take_delivered(&input.inbox);
        if !delivered.is_empty() {
            self.show(&mut state, "");
        }
        delivered
    }

    /// Shows `line` under the input box, or nothing there.
    pub fn set_footer(&self, line: Option<String>) {
        let mut state = self.state.lock().unwrap();
        if state.footer != line {
            state.footer = line;
            self.show(&mut state, "");
        }
    }

    /// Draws nothing from now on, with `silent`.
    pub fn set_silent(&self, silent: bool) {
        self.state.lock().unwrap().silent = silent;
    }

    /// Holds back output while the line editor reads (`true`), and prints
    /// what was held once it is done (`false`).
    pub fn hold(&self, hold: bool) {
        let held = {
            let mut state = self.state.lock().unwrap();
            if hold {
                state.held.get_or_insert_with(Vec::new);
                return;
            }
            state.held.take().unwrap_or_default()
        };
        self.lines(&held);
    }

    pub fn styled(&self) -> bool {
        self.styled
    }

    /// Whether output is shown as in a terminal: styled, or on the screen.
    pub(super) fn presented(&self) -> bool {
        self.styled || self.state.lock().unwrap().screen.is_some()
    }

    /// Wraps `s` in an SGR color, when styled.
    pub fn paint(&self, sgr: &str, s: &str) -> String {
        if self.styled { format!("\x1b[{sgr}m{s}\x1b[0m") } else { s.to_string() }
    }

    /// Animates the spinner while a status is set. Needs a Tokio runtime.
    pub fn spawn_spinner(&self) {
        if !self.interactive() {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            loop {
                tick.tick().await;
                let mut state = this.state.lock().unwrap();
                let resized = state.size != (text::terminal_width(), text::terminal_height());
                if state.status.is_some() || ((state.input.is_some() || state.panel.is_some() || state.tool_panel.is_some()) && resized) {
                    state.frame += 1;
                    this.show(&mut state, "");
                }
            }
        });
    }

    /// Shows `label` next to the spinner, keeping the elapsed time and token
    /// count; `None` hides it.
    pub fn set_status(&self, label: Option<&str>) {
        let mut state = self.state.lock().unwrap();
        match label {
            Some(label) => {
                let (since, tokens) = match state.status.take() {
                    Some(s) => (s.since, s.tokens),
                    None => (Instant::now(), 0),
                };
                state.status = Some(Status { label: label.to_string(), since, detail: String::new(), tokens });
            }
            None => state.status = None,
        }
        self.show(&mut state, "");
    }

    /// Sets the dim line under the status; shown from the next frame on.
    pub fn set_status_detail(&self, detail: &str) {
        if let Some(status) = &mut self.state.lock().unwrap().status {
            status.detail = detail.to_string();
        }
    }

    /// Sets the token count next to the status; shown from the next frame on.
    pub fn set_status_tokens(&self, tokens: u64) {
        if let Some(status) = &mut self.state.lock().unwrap().status {
            status.tokens = tokens;
        }
    }

    /// Prints `lines`, each followed by a newline.
    pub fn lines<S: AsRef<str>>(&self, lines: &[S]) {
        if lines.is_empty() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if let Some(held) = &mut state.held {
            held.extend(lines.iter().map(|l| l.as_ref().to_string()));
            return;
        }
        let mut printed = String::new();
        for line in lines {
            printed.push_str(line.as_ref());
            printed.push('\n');
        }
        self.show(&mut state, &printed);
    }

    /// Prints `printed` above the status and input box, and redraws them,
    /// in one write.
    fn show(&self, state: &mut State, printed: &str) {
        if state.silent {
            return;
        }
        if printed.is_empty() && !self.redraws(state) && !state.drawn && state.screen.is_none() {
            return;
        }
        let mut screen = String::new();
        if self.interactive() {
            screen.push_str(SYNC_START);
        }
        let size = (text::terminal_width(), text::terminal_height());
        if size != state.size {
            state.size = size;
        }
        let (mut rows, cursor) = render(state, |sgr, s| self.paint(sgr, s));
        if let Some(viewport) = state.screen.as_mut() {
            if !self.styled { rows = rows.iter().map(|row| text::strip_ansi(row)).collect(); }
            let plain = (!self.styled).then(|| text::strip_ansi(printed));
            let printed = plain.as_deref().unwrap_or(printed);
            let height = size.1.unwrap_or(24);
            let frame = if state.tool_panel.is_some() { viewport.overlay(printed, rows, cursor, size.0, height) }
                else { viewport.draw(printed, rows, cursor, size.0, height) };
            screen.push_str(&frame);
            screen.push_str(SYNC_END);
            let mut stdout = io::stdout().lock();
            let _ = stdout.write_all(screen.as_bytes());
            let _ = stdout.flush();
            return;
        }
        self.draw_under(state, &mut screen, printed);
        if self.interactive() {
            screen.push_str(SYNC_END);
        }
        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(screen.as_bytes());
        let _ = stdout.flush();
    }

    /// Draws right under the output: the status and input box follow it
    /// down, and are redrawn from the cursor's row in them.
    fn draw_under(&self, state: &mut State, screen: &mut String, printed: &str) {
        if std::mem::take(&mut state.drawn) {
            screen.push_str(&clear_sequence(state.cursor_row));
        }
        screen.push_str(printed);
        if !self.redraws(state) {
            return;
        }
        let (rows, cursor) = render(state, |sgr, s| self.paint(sgr, s));
        let last = rows.len() - 1;
        screen.push_str(&rows.join("\n"));
        let (row, col) = cursor.unwrap_or((last, text::width(&rows[last])));
        if row < last {
            screen.push_str(&format!("\x1b[{}A", last - row));
        }
        screen.push('\r');
        if col > 0 {
            screen.push_str(&format!("\x1b[{col}C"));
        }
        state.drawn = true;
        state.rows = rows.len();
        state.cursor_row = row;
    }

    /// Whether the status or input box is to be drawn.
    fn redraws(&self, state: &State) -> bool {
        (self.styled || state.screen.is_some()) && (state.status.is_some() || state.input.is_some() || state.panel.is_some() || state.tool_panel.is_some())
    }

    pub fn line(&self, line: impl AsRef<str>) {
        self.lines(&[line]);
    }

    /// Shows a subagent's progress: a foreground one's tool calls, indented
    /// under its `agent` call, and how a background one ended. A background
    /// one's tool calls are left out, as they would interleave with
    /// whatever the main agent is doing.
    pub fn subagent_event(&self, event: &Value) {
        let label = event["label"].as_str().unwrap_or("");
        let background = event["background"].as_bool().unwrap_or(false);
        match event["type"].as_str() {
            Some("subagent") if !background => {
                let prefix = format!("    ◦ {} › ", text::fit(label, 32));
                let room = text::terminal_width().saturating_sub(text::width(&prefix) + 1);
                let call = call_label(event["tool"].as_str().unwrap_or(""), &event["input"], room);
                self.line(self.paint("2", &format!("{prefix}{call}")));
            }
            Some("subagent_status") if background => {
                let status = event["status"].as_str().unwrap_or("");
                let color = match status {
                    "completed" => "32",
                    "failed" => "31",
                    "stopped" => "33",
                    _ => return,
                };
                let detail = format!(
                    " · {} · {} tool calls · {}",
                    event["id"].as_str().unwrap_or(""),
                    event["tool_calls"].as_u64().unwrap_or(0),
                    elapsed(event["seconds"].as_u64().unwrap_or(0)),
                );
                let prefix = format!("  ◦ {} › ", text::fit(label, 48));
                self.line(format!("{}{}{}", self.paint("2", &prefix), self.paint(color, status), self.paint("2", &detail)));
            }
            _ => {}
        }
    }

    /// Prints text as is, without a newline; only when not styled, where
    /// there is no spinner to interleave with.
    pub(super) fn raw(&self, s: &str) {
        let mut stdout = io::stdout().lock();
        let _ = write!(stdout, "{s}");
        let _ = stdout.flush();
    }
}

/// The rows under the output: the status (spinner, label, elapsed time and
/// token count, and the detail line under them), then the input box. The
/// input box also says where the cursor goes: (row, column).
fn render(state: &State, paint: impl Fn(&str, &str) -> String) -> (Vec<String>, Option<(usize, usize)>) {
    let width = text::terminal_width();
    if let Some(panel) = &state.tool_panel {
        return panel.render(&state.transcript, &state.tool_records, width,
            text::terminal_height().unwrap_or(24), &paint);
    }
    if let Some(panel) = &state.panel {
        return panel.lock().unwrap().render(width, text::terminal_height().unwrap_or(24), &paint);
    }
    let mut rows = Vec::new();
    if let Some(status) = &state.status {
        let mut hint = vec![elapsed(status.since.elapsed().as_secs())];
        if status.tokens > 0 {
            hint.push(format!("↓ {} tokens", tokens(status.tokens)));
        }
        hint.push("esc to interrupt · ctrl+o transcript".to_string());
        let hint = format!(" ({})", hint.join(" · "));
        let label = text::fit(&status.label, width.saturating_sub(text::width(&hint) + 3));
        // Forward then back through the glyphs, without repeating the ends.
        let cycle = 2 * SPINNER.len() - 2;
        let step = state.frame % cycle;
        let glyph = SPINNER[if step < SPINNER.len() { step } else { cycle - step }];
        rows.push(format!("\x1b[{STATUS_SGR}m{glyph}\x1b[0m {}\x1b[2m{hint}\x1b[0m", shimmer(&label, state.frame)));
        if !status.detail.is_empty() {
            let detail = text::fit_tail(&status.detail, width.saturating_sub(5));
            rows.push(format!("\x1b[2m  ⎿ {detail}\x1b[0m"));
        }
    }
    if let Some(question) = &state.question {
        if !rows.is_empty() {
            rows.push(String::new());
        }
        rows.extend(question.render(width, &paint));
        return (rows, None);
    }
    let Some(input) = &state.input else { return (rows, None) };
    if !rows.is_empty() {
        rows.push(String::new());
    }
    rows.extend(input.typing.queued_rows(width, &paint));
    let rule = paint("2", &input::rule(width));
    rows.push(rule.clone());
    let (draft, (row, col)) = if state.menu.is_some() {
        input.typing.draft.render(width, "Type a message · / commands · @ files", &paint)
    } else {
        input.typing.render(width, &paint)
    };
    let cursor = (rows.len() + row, col);
    rows.extend(draft);
    rows.push(rule);
    if let Some(notice) = &input.notice {
        rows.push(paint("33", &format!("  {}", text::printable(notice))));
    }
    if let Some(menu) = &state.menu
        && let Some(line) = input.typing.draft.at_end()
        && let Some(plain) = menu.render(line, width.saturating_sub(1))
    {
        let shown = menu.styled(&plain).unwrap_or(plain);
        rows.extend(shown.trim_start_matches('\n').split('\n').map(str::to_string));
    } else {
        if let Some(line) = state.permissions.as_ref().and_then(|p| mode_line(p)) {
            rows.push(paint("2", &text::fit(&line, width.saturating_sub(1))));
        }
        if let Some(footer) = &state.footer {
            // Its own colors, unless it is too wide for them.
            let plain = text::printable(&text::strip_ansi(footer));
            rows.push(if text::width(&plain) < width { footer.clone() } else { text::fit(&plain, width.saturating_sub(1)) });
        }
    }
    (rows, Some(cursor))
}

/// The permission mode under the input box, unless it is the one asking
/// for nothing that the settings start in.
fn mode_line(permissions: &crate::permissions::Permissions) -> Option<String> {
    use crate::permissions::Mode;
    let mode = permissions.mode();
    let label = match mode {
        Mode::BypassPermissions if permissions.default_mode() == Mode::BypassPermissions => return None,
        Mode::BypassPermissions => "⏵⏵ bypass permissions on",
        Mode::AcceptEdits => "⏵⏵ accept edits on",
        Mode::Plan => "⏸ plan mode on",
        Mode::Default => "◇ asking before changes",
    };
    Some(format!("  {label} (shift+tab to cycle)"))
}

/// Erases the rows under the output, from `cursor_row` of them, leaving the
/// cursor at the start of the first.
fn clear_sequence(cursor_row: usize) -> String {
    if cursor_row == 0 { "\r\x1b[J".to_string() } else { format!("\r\x1b[{cursor_row}A\x1b[J") }
}

/// Colors `label` with a highlight sweeping across it as `frame` advances.
fn shimmer(label: &str, frame: usize) -> String {
    let chars: Vec<char> = label.chars().collect();
    // The highlight travels a little past both ends before wrapping.
    let center = (frame % (chars.len() + 8)) as isize - 4;
    let mut out = String::new();
    let mut current = "";
    for (i, c) in chars.iter().enumerate() {
        let sgr = if (i as isize - center).abs() <= 1 { SHIMMER_SGR } else { STATUS_SGR };
        if sgr != current {
            out.push_str(&format!("\x1b[0;{sgr}m"));
            current = sgr;
        }
        out.push(*c);
    }
    out.push_str("\x1b[0m");
    out
}

/// `75` -> `1m 15s`.
pub(super) fn elapsed(secs: u64) -> String {
    if secs < 60 { format!("{secs}s") } else { format!("{}m {:02}s", secs / 60, secs % 60) }
}

/// `12345` -> `12.3k`.
pub(super) fn tokens(n: u64) -> String {
    if n < 1000 { n.to_string() } else { format!("{:.1}k", n as f64 / 1000.0) }
}

#[cfg(test)]
impl Output {
    /// An unstyled handle that keeps what it prints, for [`Output::captured`].
    pub(super) fn capture() -> Self {
        let mut out = Output::stdout();
        out.styled = false;
        out.state.lock().unwrap().held = Some(Vec::new());
        out
    }

    /// The lines printed so far, by one from [`Output::capture`].
    pub(super) fn captured(&self) -> Vec<String> {
        self.state.lock().unwrap().held.clone().unwrap_or_default()
    }

    pub(super) fn with_tool_records<R>(&self, f: impl FnOnce(&[Record]) -> R) -> R {
        f(&self.state.lock().unwrap().tool_records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inspector_consumes_keys_without_editing_the_draft_and_clear_resets_it() {
        let out = Output::capture();
        let inbox = Arc::new(Inbox::default());
        out.open_input(Typing::default(), inbox.clone());
        out.edit_input(|t| t.draft.set_text("unsent draft"));
        assert!(!out.tool_output_key(&Key::Enter));
        out.record_call("a", "bash", &json!({"command":"df -h"}));
        assert!(out.tool_output_key(&Key::ToolOutput));
        assert!(out.tool_output_key(&Key::Text("search".into())));
        assert!(out.tool_output_key(&Key::Esc));
        assert!(!out.tool_output_key(&Key::Enter));
        assert_eq!(out.close_input().draft.text(), "unsent draft");
        assert!(!inbox.wants_turn());
        out.record_call("a", "bash", &json!({"command":"df -h"}));
        out.restore_tool_output(&[]);
        assert!(out.state.lock().unwrap().tool_records.is_empty());
        assert!(out.state.lock().unwrap().transcript.is_empty());
    }

    #[test]
    fn expanded_history_restores_prompts_thoughts_tools_and_replies_in_order() {
        let out = Output::capture();
        out.restore_tool_output(&[
            Message::user(vec![json!({"type":"text","text":"check inode"})]),
            Message { role: "assistant", source: None, content: vec![
                json!({"type":"thinking","thinking":"inspect first","signature":"not displayed"}),
                json!({"type":"tool_use","id":"a","name":"bash","input":{"command":"df -i"}}),
                json!({"type":"tool_use","id":"b","name":"bash","input":{"command":"pwd"}}),
            ]},
            Message::user(vec![
                json!({"type":"tool_result","tool_use_id":"b","content":"SECOND"}),
                json!({"type":"tool_result","tool_use_id":"a","content":"FIRST"}),
            ]),
            Message { role: "assistant", source: None, content: vec![json!({"type":"text","text":"done"})] },
        ]);
        let state = out.state.lock().unwrap();
        assert!(matches!(&state.transcript[0], Entry::Text { label: "❯ You", content } if content == "check inode"));
        assert!(matches!(&state.transcript[1], Entry::Text { label: "Thinking", content } if content == "inspect first"));
        assert!(matches!(&state.transcript[2], Entry::Tool(0)));
        assert!(matches!(&state.transcript[3], Entry::Tool(1)));
        assert!(matches!(&state.transcript[4], Entry::Text { label: "● Assistant", content } if content == "done"));
        assert_eq!(state.transcript.len(), 5);
        assert_eq!(state.tool_records[0].item(1).details.last().unwrap(), "FIRST");
        assert_eq!(state.tool_records[1].item(2).details.last().unwrap(), "SECOND");
    }

    #[test]
    fn draws_the_spinner_status() {
        assert_eq!(elapsed(75), "1m 15s");
        assert_eq!(text::strip_ansi(&shimmer("Thinking…", 3)), "Thinking…");
        assert_eq!(clear_sequence(2), "\r\x1b[2A\x1b[J");
    }
}
