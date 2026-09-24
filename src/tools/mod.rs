//! Tool abstraction and registry.
//!
//! A tool exposes a name, a description and a JSON schema to the model, and
//! executes a JSON input into a result. Adding a built-in tool means
//! implementing [`Tool`] and registering it in [`ToolRegistry::with_defaults`];
//! MCP and skill tools are registered at startup from configuration.

mod bash;
mod grep;
mod read;
mod write;

pub use bash::BashTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub(crate) use read::number_lines;
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
    /// Anthropic `image` content blocks sent alongside the text.
    pub images: Vec<Value>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false, images: Vec::new() }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true, images: Vec::new() }
    }

    /// The `content` field of the `tool_result` block.
    pub fn to_result_content(&self) -> Value {
        if self.images.is_empty() {
            return Value::String(self.content.clone());
        }
        let mut blocks = Vec::with_capacity(self.images.len() + 1);
        if !self.content.is_empty() {
            blocks.push(json!({"type": "text", "text": self.content}));
        }
        blocks.extend(self.images.iter().cloned());
        Value::Array(blocks)
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;
    /// Whether the API streams this tool's input unbuffered. When true the
    /// API skips input validation, so `run` must validate strictly.
    fn eager_input_streaming(&self) -> bool {
        true
    }
    /// Whether the tool never modifies anything. Read-only agents only get
    /// tools that return true; unknown tools default to false.
    fn read_only(&self) -> bool {
        false
    }
    async fn run(&self, input: Value) -> ToolOutput;
}

/// Name-keyed set of tools. A `BTreeMap` keeps the tool list order stable
/// across requests, which keeps the prompt-cache prefix stable.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The built-in tools. `files` is shared with `@file` mentions, so a file
    /// attached by the user counts as read.
    pub fn with_defaults(files: FileTracker) -> Self {
        let mut registry = Self::new();
        registry.register(BashTool);
        registry.register(GrepTool);
        registry.register(ReadTool::new(files.clone()));
        registry.register(WriteTool::new(files));
        registry
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.register_arc(Arc::new(tool));
    }

    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    #[cfg(test)]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Keeps only the tools for which `keep` returns true.
    pub fn retain(&mut self, mut keep: impl FnMut(&dyn Tool) -> bool) {
        self.tools.retain(|_, t| keep(t.as_ref()));
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    /// Tool definitions in the shape the Messages API expects.
    ///
    /// `eager_input_streaming` streams large inputs (e.g. file contents for
    /// `write`) as they are generated. The API then no longer validates the
    /// input, so built-in tools deserialize into a strict struct before
    /// running; MCP tools, whose schemas are arbitrary, leave it off so the
    /// API validates their input.
    pub fn definitions(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(|t| {
                let mut def = json!({
                    "name": t.name(),
                    "description": t.description(),
                    "input_schema": t.input_schema(),
                });
                if t.eager_input_streaming() {
                    def["eager_input_streaming"] = json!(true);
                }
                def
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
