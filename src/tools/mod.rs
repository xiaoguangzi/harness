//! Tool abstraction and registry.
//!
//! A tool exposes a name, a description and a JSON schema to the model, and
//! executes a JSON input into a text result. Adding a tool means implementing
//! [`Tool`] and registering it in [`ToolRegistry::with_defaults`].

mod bash;
mod read;

pub use bash::BashTool;
pub use read::ReadTool;

use std::collections::BTreeMap;
use std::sync::Arc;

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
        let mut registry = Self::new();
        registry.register(BashTool);
        registry.register(ReadTool);
        registry
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.tools.insert(tool.name(), Arc::new(tool));
    }

    /// Tool definitions in the shape the Messages API expects.
    pub fn definitions(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(|t| {
                json!({
                    "name": t.name(),
                    "description": t.description(),
                    "input_schema": t.input_schema(),
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
