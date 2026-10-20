//! One-shot mode for scripts: `--output-format json` prints one JSON object
//! with the result when the run is done; `stream-json` prints an event per
//! line as it happens, then the same result. Nothing else goes to stdout.

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::agent::{Agent, Observer};
use crate::llm::Usage;
use crate::tools::ToolOutput;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Format {
    Text,
    Json,
    StreamJson,
}

/// Takes `--output-format FORMAT` (or `=FORMAT`) out of the command line.
pub fn parse_flag(args: Vec<String>) -> Result<(Format, Vec<String>)> {
    let mut format = Format::Text;
    let mut rest = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let value = match arg.strip_prefix("--output-format") {
            Some("") => args.next(),
            Some(value) if value.starts_with('=') => Some(value[1..].to_string()),
            _ => {
                rest.push(arg);
                continue;
            }
        };
        format = match value.as_deref() {
            Some("text") => Format::Text,
            Some("json") => Format::Json,
            Some("stream-json") => Format::StreamJson,
            Some(other) => bail!("unknown --output-format `{other}` (text, json or stream-json)"),
            None => bail!("--output-format needs a format (text, json or stream-json)"),
        };
    }
    Ok((format, rest))
}

/// Prints `event` as one line of JSON.
pub fn print(event: &Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{event}");
    let _ = stdout.flush();
}

/// The run so far, for the result; with `stream`, each event is printed
/// as it comes.
pub struct Events {
    stream: bool,
    started: Instant,
    pub requests: u64,
    usage: Usage,
    text: String,
}

impl Events {
    pub fn new(format: Format) -> Self {
        Self { stream: format == Format::StreamJson, started: Instant::now(), requests: 0, usage: Usage::default(), text: String::new() }
    }

    /// Prints the event that starts a stream: the session, model and tools.
    pub fn init(&self, agent: &Agent, session_id: Option<&str>) {
        if self.stream {
            let tools: Vec<&str> = agent.tools().names().collect();
            let cwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
            print(&json!({"type": "init", "session_id": session_id, "model": agent.model(), "cwd": cwd, "tools": tools}));
        }
    }

    fn emit(&self, event: Value) {
        if self.stream {
            print(&event);
        }
    }

    /// The object that ends the run: what the model answered last, or why
    /// it failed.
    pub fn result(&self, agent: &Agent, session_id: Option<&str>, error: Option<String>, start: usize) -> Value {
        let usage = &self.usage;
        json!({
            "type": "result",
            "is_error": error.is_some(),
            "result": agent.last_text(start),
            "error": error,
            "session_id": session_id,
            "model": agent.model(),
            "requests": self.requests,
            "duration_ms": self.started.elapsed().as_millis() as u64,
            "usage": {
                "input_tokens": usage.input_tokens,
                "cache_read_tokens": usage.cache_read_tokens,
                "cache_write_tokens": usage.cache_write_tokens,
                "output_tokens": usage.output_tokens,
            },
        })
    }
}

impl Observer for Events {
    fn on_text_delta(&mut self, text: &str) {
        self.text.push_str(text);
    }

    fn on_text_end(&mut self) {
        let text = std::mem::take(&mut self.text);
        if !text.is_empty() {
            self.emit(json!({"type": "text", "text": text}));
        }
    }

    fn on_tool_call(&mut self, name: &str, input: &Value) {
        self.on_tool_call_id("", name, input);
    }

    fn on_tool_call_id(&mut self, id: &str, name: &str, input: &Value) {
        self.emit(json!({"type": "tool_call", "id": id, "name": name, "input": input}));
    }

    fn on_tool_result(&mut self, name: &str, output: &ToolOutput) {
        self.on_tool_result_id("", name, output);
    }

    fn on_tool_result_id(&mut self, id: &str, name: &str, output: &ToolOutput) {
        self.emit(json!({"type": "tool_result", "id": id, "name": name, "content": output.content, "is_error": output.is_error}));
    }

    fn on_usage(&mut self, usage: &Usage) {
        self.requests += 1;
        self.usage.input_tokens += usage.input_tokens;
        self.usage.cache_read_tokens += usage.cache_read_tokens;
        self.usage.cache_write_tokens += usage.cache_write_tokens;
        self.usage.output_tokens += usage.output_tokens;
        self.emit(json!({"type": "usage", "input_tokens": usage.input_tokens, "output_tokens": usage.output_tokens,
            "cache_read_tokens": usage.cache_read_tokens, "cache_write_tokens": usage.cache_write_tokens}));
    }

    fn on_retry(&mut self, error: &anyhow::Error, attempt: u32, max: u32, wait: Duration) {
        // What streamed before is void.
        self.text.clear();
        let message = crate::llm::retry_reason(error);
        self.emit(json!({"type": "retry", "message": message, "attempt": attempt, "max": max, "wait_ms": wait.as_millis() as u64}));
    }

    fn on_compact_end(&mut self, before: u64, after: u64) {
        self.emit(json!({"type": "compact", "before": before, "after": after}));
    }

    fn on_hook(&mut self, message: &str) {
        self.emit(json!({"type": "hook", "message": message}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn parses_the_format() {
        let (format, rest) = parse_flag(args(&["--output-format", "json", "list", "files"])).unwrap();
        assert_eq!((format, rest), (Format::Json, args(&["list", "files"])));
        assert_eq!(parse_flag(args(&["--output-format=stream-json", "x"])).unwrap().0, Format::StreamJson);
        assert_eq!(parse_flag(args(&["x"])).unwrap().0, Format::Text);
        assert!(parse_flag(args(&["--output-format", "xml"])).is_err());
        assert!(parse_flag(args(&["--output-format"])).is_err());
    }

    #[test]
    fn counts_usage_and_collects_text() {
        let mut events = Events::new(Format::Json);
        events.on_usage(&Usage { input_tokens: 10, cache_read_tokens: 4, cache_write_tokens: 0, output_tokens: 3 });
        events.on_usage(&Usage { input_tokens: 20, cache_read_tokens: 0, cache_write_tokens: 2, output_tokens: 5 });
        assert_eq!(events.requests, 2);
        assert_eq!(events.usage, Usage { input_tokens: 30, cache_read_tokens: 4, cache_write_tokens: 2, output_tokens: 8 });
        events.on_text_delta("par");
        events.on_text_end();
        assert!(events.text.is_empty());
    }
}
