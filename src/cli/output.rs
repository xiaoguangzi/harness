//! Terminal output: a status spinner, and the observer that shows an agent
//! turn as it runs (Markdown text, tool calls and results, token counts).

use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::markdown::Markdown;
use super::text;
use crate::agent::Observer;
use crate::llm::Usage;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Lines of a tool result shown under its call.
const RESULT_LINES: usize = 3;

/// Shared handle on stdout. Output printed while an agent runs goes through
/// it, so the spinner line is cleared first and redrawn after.
#[derive(Clone)]
pub struct Output {
    state: Arc<Mutex<State>>,
    /// Whether stdout is a terminal that gets colors and a spinner.
    styled: bool,
}

struct State {
    status: Option<(String, Instant)>,
    /// Whether the spinner is on screen, on an otherwise empty line.
    drawn: bool,
    frame: usize,
}

impl Output {
    pub fn stdout() -> Self {
        let styled = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        Self { state: Arc::new(Mutex::new(State { status: None, drawn: false, frame: 0 })), styled }
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
            let mut tick = tokio::time::interval(Duration::from_millis(80));
            loop {
                tick.tick().await;
                let mut state = this.state.lock().unwrap();
                if state.status.is_some() {
                    state.frame += 1;
                    draw(&mut state);
                }
            }
        });
    }

    /// Shows `label` next to the spinner; `None` hides it.
    pub fn set_status(&self, label: Option<&str>) {
        let mut state = self.state.lock().unwrap();
        match label {
            Some(label) => {
                let since = match state.status.take() {
                    Some((_, since)) => since,
                    None => Instant::now(),
                };
                state.status = Some((label.to_string(), since));
                if self.styled {
                    draw(&mut state);
                }
            }
            None => {
                state.status = None;
                clear(&mut state);
            }
        }
    }

    /// Prints `lines`, each followed by a newline.
    pub fn lines<S: AsRef<str>>(&self, lines: &[S]) {
        if lines.is_empty() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        clear(&mut state);
        let mut stdout = io::stdout().lock();
        for line in lines {
            let _ = writeln!(stdout, "{}", line.as_ref());
        }
        let _ = stdout.flush();
        drop(stdout);
        if self.styled && state.status.is_some() {
            draw(&mut state);
        }
    }

    pub fn line(&self, line: impl AsRef<str>) {
        self.lines(&[line]);
    }

    /// Shows a subagent's tool call, indented under its `agent` call.
    pub fn subagent_call(&self, label: &str, tool: &str, input: &Value) {
        let prefix = format!("    ◦ {} › ", text::fit(label, 32));
        let room = text::terminal_width().saturating_sub(text::width(&prefix) + 1);
        self.line(self.paint("2", &format!("{prefix}{}", call_label(tool, input, room))));
    }

    /// Prints text as is, without a newline; only when not styled, where
    /// there is no spinner to interleave with.
    fn raw(&self, s: &str) {
        let mut stdout = io::stdout().lock();
        let _ = write!(stdout, "{s}");
        let _ = stdout.flush();
    }
}

fn draw(state: &mut State) {
    let Some((label, since)) = &state.status else { return };
    let secs = since.elapsed().as_secs();
    let hint = format!(" ({secs}s · ctrl-c to interrupt)");
    let room = text::terminal_width().saturating_sub(text::width(&hint) + 4);
    let frame = SPINNER[state.frame % SPINNER.len()];
    let mut stdout = io::stdout().lock();
    let _ = write!(stdout, "\r\x1b[K\x1b[35m{frame}\x1b[0m {}\x1b[2m{hint}\x1b[0m", text::fit(label, room));
    let _ = stdout.flush();
    state.drawn = true;
}

fn clear(state: &mut State) {
    if std::mem::take(&mut state.drawn) {
        let mut stdout = io::stdout().lock();
        let _ = write!(stdout, "\r\x1b[K");
        let _ = stdout.flush();
    }
}

#[derive(PartialEq)]
enum Block {
    None,
    Text,
    Tools,
}

/// Shows one agent turn in the terminal.
pub struct Terminal {
    out: Output,
    markdown: Markdown,
    last: Block,
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
}

impl Terminal {
    pub fn new(out: Output) -> Self {
        Self {
            markdown: Markdown::new(text::terminal_width()),
            out,
            last: Block::None,
            raw_open: false,
            calls: 0,
            results: 0,
            started: Instant::now(),
            requests: 0,
            output_tokens: 0,
            context_tokens: 0,
        }
    }

    pub fn begin_turn(&mut self) {
        let out = self.out.clone();
        *self = Self::new(out);
        self.out.set_status(Some("Thinking…"));
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
    fn on_text_delta(&mut self, delta: &str) {
        if self.last != Block::Text {
            if delta.trim().is_empty() {
                return;
            }
            self.separate(Block::Text);
            self.out.set_status(Some("Responding…"));
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
        self.out.set_status(Some("Thinking…"));
    }

    fn on_tool_call(&mut self, name: &str, input: &Value) {
        if self.results > 0 {
            (self.calls, self.results) = (0, 0);
        }
        self.calls += 1;
        self.separate(Block::Tools);
        let width = text::terminal_width();
        self.out.line(format!("{} {}", self.out.paint("32", "●"), call_label(name, input, width.saturating_sub(4))));
        let status = if self.calls == 1 { format!("Running {name}…") } else { format!("Running {} tools…", self.calls) };
        self.out.set_status(Some(&status));
    }

    fn on_tool_result(&mut self, name: &str, content: &str, is_error: bool) {
        self.results += 1;
        // With several calls in flight, say which one each result is for.
        let prefix = if self.calls > 1 { format!("{name}: ") } else { String::new() };
        let width = text::terminal_width().saturating_sub(6);
        let lines = result_lines(name, content, is_error, width);
        let color = if is_error { "31" } else { "2" };
        let mut shown = Vec::with_capacity(lines.len());
        for (i, line) in lines.iter().enumerate() {
            let lead = if i == 0 { format!("  ⎿ {prefix}") } else { "    ".to_string() };
            shown.push(self.out.paint(color, &text::fit(&format!("{lead}{line}"), width + 4)));
        }
        self.out.lines(&shown);
        if self.results == self.calls {
            self.out.set_status(Some("Thinking…"));
        }
    }

    fn on_usage(&mut self, usage: &Usage) {
        self.requests += 1;
        self.output_tokens += usage.output_tokens;
        self.context_tokens = usage.input_tokens + usage.output_tokens;
    }
}

/// `12345` -> `12.3k`.
fn tokens(n: u64) -> String {
    if n < 1000 { n.to_string() } else { format!("{:.1}k", n as f64 / 1000.0) }
}

/// A one-line view of a tool call: `bash(cargo test)`, `grep(foo · src)`.
fn call_label(name: &str, input: &Value, max: usize) -> String {
    // The fields that say the most about a call, most telling first.
    const KEYS: [&str; 10] =
        ["agent_type", "description", "command", "pattern", "path", "file_path", "glob", "url", "query", "name"];
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
