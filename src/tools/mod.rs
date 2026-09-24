//! Tool abstraction and registry.
//!
//! A tool exposes a name, a description and a JSON schema to the model, and
//! executes a JSON input into a text result. Adding a tool means implementing
//! [`Tool`] and registering it in [`ToolRegistry::with_defaults`].

mod bash;
mod grep;
mod read;
mod write;

pub use bash::BashTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub use write::WriteTool;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use serde_json::{Value, json};

/// Result of running a tool, sent back to the model as a `tool_result` block.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> Value;
    async fn run(&self, input: Value) -> ToolOutput;
}

/// Name-keyed set of tools. A `BTreeMap` keeps the tool list order stable
/// across requests, which keeps the prompt-cache prefix stable.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<&'static str, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_defaults() -> Self {
        let files = FileTracker::default();
        let mut registry = Self::new();
        registry.register(BashTool);
        registry.register(GrepTool);
        registry.register(ReadTool::new(files.clone()));
        registry.register(WriteTool::new(files));
        registry
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.tools.insert(tool.name(), Arc::new(tool));
    }

    /// Tool definitions in the shape the Messages API expects.
    ///
    /// `eager_input_streaming` streams large inputs (e.g. file contents for
    /// `write`) as they are generated. The API then no longer validates the
    /// input, so every tool deserializes into a strict struct before running.
    pub fn definitions(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(|t| {
                json!({
                    "name": t.name(),
                    "description": t.description(),
                    "input_schema": t.input_schema(),
                    "eager_input_streaming": true,
                })
            })
            .collect()
    }

    pub async fn run(&self, name: &str, input: Value) -> ToolOutput {
        match self.tools.get(name) {
            Some(tool) => tool.run(input).await,
            None => ToolOutput::error(format!("Unknown tool: {name}")),
        }
    }
}

/// Keep at most `max` bytes of `s`, cutting on a char boundary and noting
/// how much was dropped.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... [truncated {} bytes]", &s[..end], s.len() - end)
}

/// Parses tool input into its typed form; unknown fields are rejected by
/// `#[serde(deny_unknown_fields)]` on each input struct.
pub(crate) fn parse_input<T: serde::de::DeserializeOwned>(input: Value) -> Result<T, ToolOutput> {
    serde_json::from_value(input).map_err(|e| ToolOutput::error(format!("Invalid input: {e}")))
}

/// Remembers which files the model has read, and their modification time at
/// that point, so `write` refuses to clobber a file the model has not seen
/// or that changed since it was read.
#[derive(Clone, Default)]
pub struct FileTracker(Arc<Mutex<HashMap<PathBuf, Option<SystemTime>>>>);

impl FileTracker {
    fn key(path: &Path) -> PathBuf {
        std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
    }

    fn mtime(path: &Path) -> Option<SystemTime> {
        std::fs::metadata(path).and_then(|m| m.modified()).ok()
    }

    pub fn record(&self, path: &Path) {
        self.0.lock().unwrap().insert(Self::key(path), Self::mtime(path));
    }

    /// Ok for a new file, or for an existing file read since it last changed.
    pub fn check_writable(&self, path: &Path) -> Result<(), String> {
        if !path.exists() {
            return Ok(());
        }
        match self.0.lock().unwrap().get(&Self::key(path)) {
            None => Err(format!(
                "{} already exists; read it with the `read` tool before overwriting it",
                path.display()
            )),
            Some(seen) if *seen != Self::mtime(path) => Err(format!(
                "{} changed since it was last read; read it again before overwriting it",
                path.display()
            )),
            Some(_) => Ok(()),
        }
    }
}
