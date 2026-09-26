//! Terminal output: a status spinner with the input box under it, and the
//! observer that shows an agent turn as it runs (Markdown text, tool calls
//! and results, token counts).

use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::diff;
use super::input::{self, Queued, Typing};
use super::markdown::Markdown;
use super::text;
use crate::agent::{INTERRUPTED, Inbox, Observer};
use crate::llm::{MAX_RETRIES, Message, Usage, retry_reason};
use crate::tools::ToolOutput;

/// Spinner glyphs, played forward and back.
const SPINNER: [&str; 6] = ["·", "✢", "✳", "✶", "✻", "✽"];
/// Spinner and status label color, and the shimmer that sweeps the label.
const STATUS_SGR: &str = "38;5;173";
const SHIMMER_SGR: &str = "1;38;5;216";
/// Lines of a tool result shown under its call.
const RESULT_LINES: usize = 3;
/// Reasoning time worth a "Thought for" note.
const NOTABLE_THINKING: Duration = Duration::from_secs(2);
const WAITING: &str = "Waiting for the model…";
/// Turns of a resumed conversation shown again.
const REPLAYED_TURNS: usize = 10;
/// Lines of a replayed prompt.
const PROMPT_LINES: usize = 4;
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
    status: Option<Status>,
    /// The input box, while a turn runs in the REPL.
    input: Option<InputBox>,
    /// Whether the status or input box is on screen, on the last rows; the
    /// cursor is on `cursor_row` of them.
    drawn: bool,
    cursor_row: usize,
    frame: usize,
    /// Lines printed while the line editor reads, shown once it is done:
    /// printing then would tear up the line being edited.
    held: Option<Vec<String>>,
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
}

impl Output {
    pub fn stdout() -> Self {
        let styled = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        let state = State { status: None, input: None, drawn: false, cursor_row: 0, frame: 0, held: None };
        Self { state: Arc::new(Mutex::new(state)), styled }
    }

    /// Shows the input box under the status, holding `typing`, until
    /// [`Output::close_input`]. Messages sent from it go to `inbox`.
    pub fn open_input(&self, typing: Typing, inbox: Arc<Inbox>) {
        let mut state = self.state.lock().unwrap();
        state.input = Some(InputBox { typing, inbox });
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
        let result = edit(&mut input.typing);
        self.show(&mut state, "");
        Some(result)
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

    /// Wraps `s` in an SGR color, when styled.
    pub fn paint(&self, sgr: &str, s: &str) -> String {
        if self.styled { format!("\x1b[{sgr}m{s}\x1b[0m") } else { s.to_string() }
    }

    /// Animates the spinner while a status is set. Needs a Tokio runtime.
    pub fn spawn_spinner(&self) {
        if !self.styled {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            loop {
                tick.tick().await;
                let mut state = this.state.lock().unwrap();
                if state.status.is_some() {
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
        let redraw = self.styled && (state.status.is_some() || state.input.is_some());
        if printed.is_empty() && !redraw && !state.drawn {
            return;
        }
        let mut screen = String::new();
        if self.styled {
            screen.push_str(SYNC_START);
        }
        if std::mem::take(&mut state.drawn) {
            screen.push_str(&clear_sequence(state.cursor_row));
        }
        screen.push_str(printed);
        if redraw {
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
            state.cursor_row = row;
        }
        if self.styled {
            screen.push_str(SYNC_END);
        }
        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(screen.as_bytes());
        let _ = stdout.flush();
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
    fn raw(&self, s: &str) {
        let mut stdout = io::stdout().lock();
        let _ = write!(stdout, "{s}");
        let _ = stdout.flush();
    }
}

/// Shows the end of a resumed conversation the way it looked: prompts,
/// the assistant's text, and tool calls with the start of their results.
/// Reasoning and attachments are left out.
pub fn replay(out: &Output, messages: &[Message]) {
    // A turn starts at a user message that is not tool results.
    let starts: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "user" && !m.content.iter().any(|b| b["type"] == "tool_result"))
        .map(|(i, _)| i)
        .collect();
    let first = starts.len().saturating_sub(REPLAYED_TURNS);
    if first > 0 {
        out.line(out.paint("2", &format!("… {first} earlier turns")));
        out.line("");
    }
    let width = text::terminal_width();
    let mut names = std::collections::HashMap::new();
    // Tool lines were the last shown, so text needs a blank line first.
    let mut tools = false;
    for m in &messages[starts.get(first).copied().unwrap_or(messages.len())..] {
        if m.content.iter().any(|b| b["type"] == "text" && b["text"] == INTERRUPTED) {
            if std::mem::take(&mut tools) {
                out.line("");
            }
            out.line(out.paint("33", "■ Interrupted"));
            out.line("");
        }
        if m.role == "user" {
            let prompt = m.content.iter().rev().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).find(|t| *t != INTERRUPTED);
            if let Some(prompt) = prompt.filter(|_| !m.content.iter().any(|b| b["type"] == "tool_result")) {
                if std::mem::take(&mut tools) {
                    out.line("");
                }
                let lines: Vec<&str> = prompt.lines().collect();
                for (i, line) in lines.iter().take(PROMPT_LINES).enumerate() {
                    let lead = if i == 0 { out.paint("1;35", "❯ ") } else { "  ".to_string() };
                    out.line(format!("{lead}{}", text::fit(&text::printable(line), width.saturating_sub(3))));
                }
                if lines.len() > PROMPT_LINES {
                    out.line(out.paint("2", &format!("  … +{} lines", lines.len() - PROMPT_LINES)));
                }
                out.line("");
            }
            for b in m.content.iter().filter(|b| b["type"] == "tool_result") {
                let name = b["tool_use_id"].as_str().and_then(|id| names.get(id)).map_or("", String::as_str);
                let is_error = b["is_error"].as_bool().unwrap_or(false);
                let content = match &b["content"] {
                    Value::String(s) => s.clone(),
                    Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
                    _ => String::new(),
                };
                let color = if is_error { "31" } else { "2" };
                for (i, line) in result_lines(name, &content, is_error, width.saturating_sub(6)).iter().enumerate() {
                    let lead = if i == 0 { "  ⎿ " } else { "    " };
                    out.line(out.paint(color, &text::fit(&format!("{lead}{line}"), width.saturating_sub(2))));
                }
            }
            continue;
        }
        for b in &m.content {
            match b["type"].as_str() {
                Some("text") => {
                    if std::mem::take(&mut tools) {
                        out.line("");
                    }
                    let text = b["text"].as_str().unwrap_or("");
                    if out.styled() {
                        let mut md = Markdown::new(width);
                        let mut lines = md.push(text);
                        lines.extend(md.finish());
                        out.lines(&lines);
                    } else {
                        out.line(text);
                    }
                    out.line("");
                }
                Some("tool_use") => {
                    let name = b["name"].as_str().unwrap_or("");
                    if let Some(id) = b["id"].as_str() {
                        names.insert(id.to_string(), name.to_string());
                    }
                    out.line(format!("{} {}", out.paint("32", "●"), call_label(name, &b["input"], width.saturating_sub(4))));
                    tools = true;
                }
                _ => {}
            }
        }
    }
}

/// The rows under the output: the status (spinner, label, elapsed time and
/// token count, and the detail line under them), then the input box. The
/// input box also says where the cursor goes: (row, column).
fn render(state: &State, paint: impl Fn(&str, &str) -> String) -> (Vec<String>, Option<(usize, usize)>) {
    let width = text::terminal_width();
    let mut rows = Vec::new();
    if let Some(status) = &state.status {
        let mut hint = vec![elapsed(status.since.elapsed().as_secs())];
        if status.tokens > 0 {
            hint.push(format!("↓ {} tokens", tokens(status.tokens)));
        }
        hint.push("esc to interrupt".to_string());
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
    let Some(input) = &state.input else { return (rows, None) };
    if !rows.is_empty() {
        rows.push(String::new());
    }
    rows.extend(input.typing.queued_rows(width, &paint));
    let rule = paint("2", &input::rule(width));
    rows.push(rule.clone());
    let (draft, (row, col)) = input.typing.draft.render(width, &paint);
    let cursor = (rows.len() + row, col);
    rows.extend(draft);
    rows.push(rule);
    (rows, Some(cursor))
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

#[derive(PartialEq)]
enum Block {
    None,
    Text,
    Tools,
    /// A message the user sent while the turn ran.
    Prompt,
}

/// What the model is doing, as the status line says.
#[derive(Clone, Copy, PartialEq)]
enum Phase {
    /// A request is out and nothing has streamed back yet.
    Waiting,
    Thinking,
    Responding,
    /// Streaming a tool call's input.
    Preparing,
    Running,
    /// Between blocks of one response.
    Working,
}

/// Shows one agent turn in the terminal.
pub struct Terminal {
    out: Output,
    markdown: Markdown,
    last: Block,
    phase: Phase,
    /// Unstyled output only: a text line is open.
    raw_open: bool,
    /// Tool calls in the current batch, and how many results came back.
    calls: usize,
    results: usize,
    started: Instant,
    requests: usize,
    output_tokens: u64,
    /// Input tokens of the latest request: the context size.
    context_tokens: u64,
    /// Bytes streamed by the running request, for a token estimate until
    /// its usage arrives.
    streamed: usize,
    /// The end of the reasoning streamed so far, and when it began.
    thinking: String,
    thinking_since: Option<Instant>,
    /// The tool call being streamed, and the start of its input.
    tool_name: String,
    tool_input: String,
    tool_labeled: bool,
}

impl Terminal {
    pub fn new(out: Output) -> Self {
        Self {
            markdown: Markdown::new(text::terminal_width()),
            out,
            last: Block::None,
            phase: Phase::Waiting,
            raw_open: false,
            calls: 0,
            results: 0,
            started: Instant::now(),
            requests: 0,
            output_tokens: 0,
            context_tokens: 0,
            streamed: 0,
            thinking: String::new(),
            thinking_since: None,
            tool_name: String::new(),
            tool_input: String::new(),
            tool_labeled: false,
        }
    }

    pub fn begin_turn(&mut self) {
        let out = self.out.clone();
        *self = Self::new(out);
        self.enter(Phase::Waiting, WAITING);
    }

    /// Flushes pending text and prints how the turn went.
    pub fn end_turn(&mut self, result: &Result<(), TurnEnd>) {
        self.end_text();
        self.out.set_status(None);
        // Piped output gets just the text; problems go to stderr.
        if !self.out.styled() {
            match result {
                Ok(()) => {}
                Err(TurnEnd::Interrupted) => eprintln!("interrupted"),
                Err(TurnEnd::Failed(e)) => eprintln!("error: {e:#}"),
            }
            return;
        }
        let secs = self.started.elapsed().as_secs_f64();
        let mut stats = vec![format!("{secs:.1}s")];
        if self.requests > 1 {
            stats.push(format!("{} requests", self.requests));
        }
        if self.output_tokens > 0 {
            stats.push(format!("{} tokens out", tokens(self.output_tokens)));
            stats.push(format!("context {}", tokens(self.context_tokens)));
        }
        self.out.line("");
        match result {
            Ok(()) => {}
            Err(TurnEnd::Interrupted) => self.out.line(self.out.paint("33", "■ Interrupted")),
            Err(TurnEnd::Failed(e)) => self.out.line(self.out.paint("31", &format!("✗ {e:#}"))),
        }
        self.out.line(self.out.paint("2", &format!("✻ {}", stats.join(" · "))));
    }

    /// Switches the status to `phase`, noting how long any reasoning took.
    fn enter(&mut self, phase: Phase, label: &str) {
        if self.phase == Phase::Thinking && phase != Phase::Thinking {
            self.end_thinking();
        }
        self.phase = phase;
        self.out.set_status(Some(label));
    }

    fn end_thinking(&mut self) {
        self.thinking.clear();
        let Some(since) = self.thinking_since.take() else { return };
        let took = since.elapsed();
        if took >= NOTABLE_THINKING && self.out.styled() {
            self.separate(Block::Tools);
            self.out.line(self.out.paint("2", &format!("∴ Thought for {}", elapsed(took.as_secs()))));
        }
    }

    /// Counts streamed output toward the token estimate.
    fn streamed(&mut self, bytes: usize) {
        self.streamed += bytes;
        // About four bytes a token; replaced by the real count at the end.
        self.out.set_status_tokens(self.output_tokens + (self.streamed / 4) as u64);
    }

    /// Puts a blank line between text and tool calls, and after the prompt.
    fn separate(&mut self, block: Block) {
        if self.last != block {
            if self.last != Block::None || self.out.styled() {
                self.out.line("");
            }
            self.last = block;
        }
    }

    fn end_text(&mut self) {
        if self.out.styled() {
            let lines = self.markdown.finish();
            self.out.lines(&lines);
        } else if std::mem::take(&mut self.raw_open) {
            self.out.raw("\n");
        }
    }
}

/// How a turn ended early.
pub enum TurnEnd {
    Interrupted,
    Failed(anyhow::Error),
}

impl Observer for Terminal {
    fn on_request_start(&mut self) {
        self.streamed = 0;
        // Messages sent from the input box go with this request.
        for queued in self.out.take_delivered() {
            if self.last != Block::None {
                self.out.line("");
            }
            self.out.lines(&queued.shown);
            self.last = Block::Prompt;
        }
        if self.phase != Phase::Waiting {
            self.enter(Phase::Waiting, WAITING);
        }
    }

    fn on_thinking_delta(&mut self, delta: &str) {
        self.streamed(delta.len());
        if self.phase != Phase::Thinking {
            self.enter(Phase::Thinking, "Thinking…");
            self.thinking_since = Some(Instant::now());
        }
        if delta.is_empty() {
            return;
        }
        self.thinking.push_str(delta);
        // Only the latest line is shown; keep a bounded tail.
        if self.thinking.len() > 2048 {
            let mut cut = self.thinking.len() - 1024;
            while !self.thinking.is_char_boundary(cut) {
                cut += 1;
            }
            self.thinking.drain(..cut);
        }
        let latest = self.thinking.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
        self.out.set_status_detail(&text::printable(latest));
    }

    fn on_text_delta(&mut self, delta: &str) {
        self.streamed(delta.len());
        if self.last != Block::Text {
            if delta.trim().is_empty() {
                return;
            }
            self.enter(Phase::Responding, "Responding…");
            self.separate(Block::Text);
        }
        if self.out.styled() {
            let lines = self.markdown.push(delta);
            self.out.lines(&lines);
        } else {
            self.out.raw(delta);
            self.raw_open = !delta.ends_with('\n');
        }
    }

    fn on_text_end(&mut self) {
        self.end_text();
        self.enter(Phase::Working, "Working…");
    }

    fn on_tool_input_start(&mut self, name: &str) {
        self.tool_name = name.to_string();
        self.tool_input.clear();
        self.tool_labeled = false;
        self.enter(Phase::Preparing, &format!("Preparing {name}…"));
    }

    fn on_tool_input_delta(&mut self, json: &str) {
        self.streamed(json.len());
        // Name the call once its main argument is complete. That comes
        // early, so only the start of the input is kept.
        if self.tool_labeled || self.tool_input.len() > 4096 {
            return;
        }
        self.tool_input.push_str(json);
        if let Some(arg) = partial_argument(&self.tool_input) {
            self.tool_labeled = true;
            let arg = text::printable(arg.lines().next().unwrap_or(""));
            self.out.set_status(Some(&format!("Preparing {}({arg})…", self.tool_name)));
        }
    }

    fn on_tool_call(&mut self, name: &str, input: &Value) {
        if self.results > 0 {
            (self.calls, self.results) = (0, 0);
        }
        self.calls += 1;
        let status = if self.calls == 1 { format!("Running {name}…") } else { format!("Running {} tools…", self.calls) };
        self.enter(Phase::Running, &status);
        self.separate(Block::Tools);
        let width = text::terminal_width();
        self.out.line(format!("{} {}", self.out.paint("32", "●"), call_label(name, input, width.saturating_sub(4))));
    }

    fn on_tool_result(&mut self, name: &str, output: &ToolOutput) {
        self.results += 1;
        // With several calls in flight, say which one each result is for.
        let prefix = if self.calls > 1 { format!("{name}: ") } else { String::new() };
        let width = text::terminal_width().saturating_sub(6);
        let shown: Vec<String> = match &output.change {
            Some(change) if !output.is_error => diff::render(change, width, |sgr, s| self.out.paint(sgr, s))
                .into_iter()
                .enumerate()
                .map(|(i, line)| if i == 0 { self.out.paint("2", &format!("  ⎿ {prefix}{line}")) } else { format!("    {line}") })
                .collect(),
            _ => {
                let color = if output.is_error { "31" } else { "2" };
                result_lines(name, &output.content, output.is_error, width)
                    .iter()
                    .enumerate()
                    .map(|(i, line)| {
                        let lead = if i == 0 { format!("  ⎿ {prefix}") } else { "    ".to_string() };
                        self.out.paint(color, &text::fit(&format!("{lead}{line}"), width + 4))
                    })
                    .collect()
            }
        };
        self.out.lines(&shown);
        if self.results == self.calls {
            self.enter(Phase::Waiting, WAITING);
        }
    }

    fn on_retry(&mut self, error: &anyhow::Error, attempt: u32, wait: Duration) {
        // What streamed stays on screen; the notice marks where it broke off
        // and the response starts over below it.
        self.end_text();
        self.enter(Phase::Waiting, WAITING);
        self.streamed = 0;
        let detail = format!("({}); retrying in {}s ({attempt}/{MAX_RETRIES})", retry_reason(error), wait.as_secs());
        if !self.out.styled() {
            eprintln!("warning: response broke off {detail}");
            return;
        }
        self.separate(Block::Tools);
        self.out.line(self.out.paint("33", &format!("↻ Response broke off {detail}")));
    }

    fn on_usage(&mut self, usage: &Usage) {
        self.requests += 1;
        self.output_tokens += usage.output_tokens;
        self.context_tokens = usage.input_tokens + usage.output_tokens;
        self.streamed = 0;
        self.out.set_status_tokens(self.output_tokens);
    }
}

/// The main argument of a tool call whose JSON input is still streaming
/// in, once its string value is complete.
fn partial_argument(json: &str) -> Option<String> {
    const KEYS: [&str; 7] = ["command", "path", "file_path", "pattern", "description", "url", "query"];
    KEYS.iter().find_map(|key| {
        let at = json.find(&format!("\"{key}\""))? + key.len() + 2;
        let value = json[at..].trim_start().strip_prefix(':')?.trim_start();
        if !value.starts_with('"') {
            return None;
        }
        let mut escaped = false;
        for (i, c) in value.char_indices().skip(1) {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => return serde_json::from_str(&value[..=i]).ok(),
                _ => {}
            }
        }
        None
    })
}

/// `12345` -> `12.3k`.
fn tokens(n: u64) -> String {
    if n < 1000 { n.to_string() } else { format!("{:.1}k", n as f64 / 1000.0) }
}

/// A one-line view of a tool call: `bash(cargo test)`, `grep(foo · src)`.
fn call_label(name: &str, input: &Value, max: usize) -> String {
    // The fields that say the most about a call, most telling first.
    const KEYS: [&str; 12] = [
        "agent_type", "description", "agent_id", "message", "command", "pattern", "path", "file_path", "glob", "url",
        "query", "name",
    ];
    let obj = input.as_object();
    let mut parts: Vec<String> = KEYS
        .iter()
        .filter_map(|k| obj?.get(*k)?.as_str())
        .filter(|v| !v.is_empty())
        .take(2)
        .map(|v| v.lines().next().unwrap_or("").to_string())
        .collect();
    if parts.is_empty() && obj.is_some_and(|o| !o.is_empty()) {
        parts.push(input.to_string());
    }
    let label = if parts.is_empty() { name.to_string() } else { format!("{name}({})", parts.join(" · ")) };
    let fitted = text::fit(&label, max);
    if fitted != label && label.ends_with(')') {
        // Keep the call visibly closed.
        format!("{})", text::fit(&label, max.saturating_sub(1)).trim_end_matches(')'))
    } else {
        fitted
    }
}

/// The first few lines of a tool result, or a summary of it.
fn result_lines(name: &str, content: &str, is_error: bool, width: usize) -> Vec<String> {
    let clean = text::strip_ansi(content).replace('\t', "  ");
    let lines: Vec<&str> = clean.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return vec!["(no output)".to_string()];
    }
    if name == "read" && !is_error {
        return vec![format!("Read {} lines", lines.len())];
    }
    let mut shown: Vec<String> = lines.iter().take(RESULT_LINES).map(|l| text::fit(l, width)).collect();
    if lines.len() > RESULT_LINES {
        shown.push(format!("… +{} lines", lines.len() - RESULT_LINES));
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn labels_calls_by_their_main_arguments() {
        assert_eq!(call_label("bash", &json!({"command": "cargo test\n--all", "timeout_ms": 5}), 80), "bash(cargo test)");
        assert_eq!(call_label("grep", &json!({"pattern": "fn main", "path": "src"}), 80), "grep(fn main · src)");
        assert_eq!(
            call_label("agent", &json!({"agent_type": "explore", "description": "find traits", "prompt": "..."}), 80),
            "agent(explore · find traits)"
        );
        assert_eq!(call_label("mcp__x__y", &json!({"id": 3}), 80), "mcp__x__y({\"id\":3})");
        assert_eq!(call_label("ping", &json!({}), 80), "ping");
        assert_eq!(call_label("bash", &json!({"command": "echo hello world"}), 12), "bash(echo …)");
    }

    #[test]
    fn finds_the_main_argument_of_a_streaming_call() {
        assert_eq!(partial_argument(r#"{"path": "src/a.rs", "content": "fn ma"#), Some("src/a.rs".into()));
        assert_eq!(partial_argument(r#"{"command":"echo \"hi\""#), None);
        assert_eq!(partial_argument(r#"{"command":"echo \"hi\"","#), Some(r#"echo "hi""#.into()));
        assert_eq!(partial_argument(r#"{"content": "x", "pa"#), None);
    }

    #[test]
    fn draws_the_spinner_status() {
        assert_eq!(elapsed(75), "1m 15s");
        assert_eq!(text::strip_ansi(&shimmer("Thinking…", 3)), "Thinking…");
        assert_eq!(clear_sequence(2), "\r\x1b[2A\x1b[J");
    }

    #[test]
    fn summarizes_results() {
        assert_eq!(result_lines("read", "     1\ta\n     2\tb\n", false, 80), vec!["Read 2 lines"]);
        assert_eq!(result_lines("bash", "", false, 80), vec!["(no output)"]);
        assert_eq!(
            result_lines("bash", "1\n2\n\n3\n4\n5\n", false, 80),
            vec!["1", "2", "3", "… +2 lines"]
        );
        assert_eq!(result_lines("read", "No such file", true, 80), vec!["No such file"]);
    }
}
