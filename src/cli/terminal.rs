//! The observer that shows an agent turn in the terminal as it runs:
//! Markdown text, tool calls and results, and how long it took.

use std::time::{Duration, Instant};

use serde_json::Value;

use super::calls::{call_label, call_lines, result_block};
use super::markdown::Markdown;
use super::output::{elapsed, tokens};
use super::{Output, diff, text};
use super::style::{DIM, YELLOW};
use crate::agent::Observer;
use crate::llm::{Usage, retry_reason};
use crate::tools::ToolOutput;

/// Reasoning time worth a "Thought for" note.
const NOTABLE_THINKING: Duration = Duration::from_secs(2);
const WAITING: &str = "Waiting for the model…";

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
    /// Call IDs and retained output indexes awaiting results.
    pending: Vec<(String, usize)>,
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
            pending: Vec::new(),
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
        for (_, index) in std::mem::take(&mut self.pending) {
            let (name, input) = self.out.record_result(index, None);
            let mut lines = call_lines(&self.out, &name, &input, text::terminal_width());
            lines.push(self.out.paint(YELLOW, "  ⎿ Interrupted · no result received"));
            self.out.lines(&lines);
        }
        self.out.set_status(None);
        // Piped output gets just the text; problems go to stderr.
        if !self.out.presented() {
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
            Err(TurnEnd::Interrupted) => self.out.warn("■ Interrupted"),
            Err(TurnEnd::Failed(e)) => self.out.error(format!("✗ {e:#}")),
        }
        self.out.note(format!("✻ {}", stats.join(" · ")));
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
        if took >= NOTABLE_THINKING && self.out.presented() {
            self.separate(Block::Tools);
            self.out.note(format!("∴ Thought for {}", elapsed(took.as_secs())));
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
            if self.last != Block::None || self.out.presented() {
                self.out.line("");
            }
            self.last = block;
        }
    }

    fn end_text(&mut self) {
        if self.out.presented() {
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

/// Shows a compaction of a conversation waiting at the prompt: a line when
/// it is done, and the hook notices it brings.
pub struct IdleCompaction<'a> {
    out: &'a Output,
}

impl<'a> IdleCompaction<'a> {
    pub fn new(out: &'a Output) -> Self {
        Self { out }
    }
}

impl Observer for IdleCompaction<'_> {
    fn on_text_delta(&mut self, _: &str) {}
    fn on_text_end(&mut self) {}
    fn on_tool_call(&mut self, _: &str, _: &Value) {}
    fn on_tool_result(&mut self, _: &str, _: &ToolOutput) {}

    fn on_compact_start(&mut self) {
        self.out.set_status(Some("Compacting the idle conversation before its cache expires…"));
    }

    fn on_compact_end(&mut self, before: u64, after: u64) {
        self.out.set_status(None);
        let note = format!("✻ Compacted the idle conversation: context {} → about {}", tokens(before), tokens(after));
        self.out.compaction_note(&note);
        self.out.note(&note);
    }

    fn on_hook(&mut self, message: &str) {
        self.out.warn(format!("⚑ {message}"));
    }
}

impl Observer for Terminal {
    fn on_waiting(&mut self, tasks: &[String]) {
        if tasks.is_empty() {
            if self.pending.is_empty() { self.enter(Phase::Waiting, WAITING); }
            else { self.enter(Phase::Running, "Running tools…"); }
        } else {
            self.enter(Phase::Running, &format!("Waiting for tasks: {}…", tasks.join(", ")));
        }
    }

    fn on_request_start(&mut self) {
        self.streamed = 0;
        // Messages sent from the input box go with this request.
        for queued in self.out.take_delivered() {
            self.out.transcript_text("❯ You", &queued.text, false);
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
        self.out.transcript_text("Thinking", delta, true);
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
        self.out.transcript_text("● Assistant", delta, true);
        self.streamed(delta.len());
        if self.last != Block::Text {
            if delta.trim().is_empty() {
                return;
            }
            self.enter(Phase::Responding, "Responding…");
            self.separate(Block::Text);
        }
        if self.out.presented() {
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
        self.on_tool_call_id("", name, input);
    }

    fn on_tool_result(&mut self, name: &str, output: &ToolOutput) {
        let id = self.pending.first().map(|(id, _)| id.clone()).unwrap_or_default();
        self.on_tool_result_id(&id, name, output);
    }

    fn on_tool_call_id(&mut self, id: &str, name: &str, input: &Value) {
        if self.results > 0 {
            (self.calls, self.results) = (0, 0);
        }
        self.calls += 1;
        let status = if self.calls == 1 { format!("Running {name}…") } else { format!("Running {} tools…", self.calls) };
        self.enter(Phase::Running, &status);
        self.separate(Block::Tools);
        let index = self.out.record_call(id, name, input);
        self.pending.push((id.to_string(), index));
        self.out.set_status_detail(&call_label(name, input, text::terminal_width().saturating_sub(6)));
    }

    fn on_tool_result_id(&mut self, id: &str, name: &str, output: &ToolOutput) {
        self.results += 1;
        let width = text::terminal_width();
        let mut shown = Vec::new();
        if let Some(position) = self.pending.iter().position(|(call_id, _)| call_id == id) {
            let (_, index) = self.pending.remove(position);
            let (name, input) = self.out.record_result(index, Some(output));
            shown.extend(call_lines(&self.out, &name, &input, width));
        }
        let result: Vec<String> = match &output.change {
            Some(change) if !output.is_error => diff::render(change, width.saturating_sub(6), |sgr, s| self.out.paint(sgr, s))
                .into_iter()
                .enumerate()
                .map(|(i, line)| if i == 0 { self.out.paint(DIM, &format!("  ⎿ {line}")) } else { format!("    {line}") })
                .collect(),
            _ => result_block(&self.out, name, &output.content, output.is_error, width),
        };
        shown.extend(result);
        self.out.lines(&shown);
        if self.results == self.calls {
            self.enter(Phase::Waiting, WAITING);
        }
    }

    fn on_retry(&mut self, error: &anyhow::Error, attempt: u32, max: u32, wait: Duration) {
        // What streamed stays on screen; the notice marks where it broke off
        // and the response starts over below it.
        self.end_text();
        self.enter(Phase::Waiting, WAITING);
        let what = if self.streamed > 0 { "response broke off" } else { "request failed" };
        self.streamed = 0;
        let detail = format!("({}); retrying in {}s ({attempt}/{max})", retry_reason(error), wait.as_secs());
        if !self.out.presented() {
            eprintln!("warning: {what} {detail}");
            return;
        }
        self.separate(Block::Tools);
        let mut what = what.to_string();
        what[..1].make_ascii_uppercase();
        self.out.warn(format!("↻ {what} {detail}"));
    }

    fn on_compact_start(&mut self) {
        self.end_text();
        self.enter(Phase::Waiting, "Compacting the conversation…");
    }

    fn on_hook(&mut self, message: &str) {
        if !self.out.presented() {
            eprintln!("hook: {message}");
            return;
        }
        self.end_text();
        self.separate(Block::Tools);
        for line in message.lines() {
            self.out.warn(format!("⚑ {}", text::printable(line)));
        }
    }

    fn on_compact_end(&mut self, before: u64, after: u64) {
        self.context_tokens = after;
        let note = format!("✻ Compacted the conversation: context {} → about {}", tokens(before), tokens(after));
        self.out.compaction_note(&note);
        if !self.out.presented() {
            eprintln!("{note}");
        } else {
            self.separate(Block::Tools);
            self.out.note(&note);
        }
        self.enter(Phase::Waiting, WAITING);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::tool_output;
    use serde_json::json;

    #[test]
    fn same_name_results_stay_adjacent_to_their_call_even_out_of_order() {
        let out = Output::capture();
        let mut terminal = Terminal::new(out.clone());
        terminal.on_tool_call_id("a", "bash", &json!({"command":"df -h"}));
        terminal.on_tool_call_id("b", "bash", &json!({"command":"du -sh"}));
        assert!(out.captured().is_empty());
        terminal.on_tool_result_id("b", "bash", &ToolOutput::error("DU result"));
        terminal.on_tool_result_id("a", "bash", &ToolOutput::ok("DF result"));
        assert_eq!(out.captured(), ["● bash(du -sh)", "  ⎿ DU result", "● bash(df -h)", "  ⎿ DF result"]);
        let items = out.with_tool_records(tool_output::items);
        assert_eq!(items[0].details.last().unwrap(), "DF result");
        assert_eq!(items[1].details.last().unwrap(), "DU result");
        assert_eq!(items[1].description, "Error");
    }

    #[test]
    fn interrupted_calls_remain_inspectable_and_do_not_leak_into_next_turn() {
        let out = Output::capture();
        let mut terminal = Terminal::new(out.clone());
        terminal.on_tool_call_id("a", "bash", &json!({"command":"sleep 10"}));
        terminal.end_turn(&Err(TurnEnd::Interrupted));
        assert!(terminal.pending.is_empty());
        assert!(out.captured().iter().any(|line| line.contains("sleep 10")));
        assert!(out.with_tool_records(|records| records[0].item(1).description.starts_with("Interrupted")));
        terminal.begin_turn();
        assert!(terminal.pending.is_empty());
        assert_eq!(out.with_tool_records(|records| records.len()), 1);
    }

    #[test]
    fn finds_the_main_argument_of_a_streaming_call() {
        assert_eq!(partial_argument(r#"{"path": "src/a.rs", "content": "fn ma"#), Some("src/a.rs".into()));
        assert_eq!(partial_argument(r#"{"command":"echo \"hi\""#), None);
        assert_eq!(partial_argument(r#"{"command":"echo \"hi\"","#), Some(r#"echo "hi""#.into()));
        assert_eq!(partial_argument(r#"{"content": "x", "pa"#), None);
    }
}
