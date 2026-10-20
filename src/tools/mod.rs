//! Tool abstraction and registry.
//!
//! A tool exposes a name, a description and a JSON schema to the model, and
//! executes a JSON input into a result. Adding a built-in tool means
//! implementing [`Tool`] and registering it in [`ToolRegistry::with_defaults`];
//! MCP and skill tools are registered at startup from configuration.

pub mod bash;
mod edit;
mod glob;
mod grep;
mod read;
pub mod shells;
pub mod snapshot;
pub(crate) mod spill;
pub mod workdir;
pub mod scheduling;
pub(crate) mod progress;
mod wait;
#[allow(dead_code)] // `todo_write` isn't registered; see `with_defaults`
pub mod todo;
mod write;

pub use bash::BashTool;
pub(crate) use bash::KillGroup;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub(crate) use read::number_lines;
pub use shells::{BashOutputTool, KillShellTool};
pub use wait::WaitTasksTool;
#[allow(unused_imports)]
pub use todo::TodoTool;
pub use write::WriteTool;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;

use crate::llm::ToolDef;
use serde_json::{Value, json};

/// Result of running a tool, sent back to the model as a `tool_result` block.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// Anthropic `image` or `document` content blocks sent alongside the
    /// text.
    pub images: Vec<Value>,
    /// A file the tool changed, for the frontend to show as a diff; not
    /// sent to the model.
    pub change: Option<Box<FileChange>>,
}

/// A file's contents before and after a tool changed it.
#[derive(Debug, Clone, PartialEq)]
pub struct FileChange {
    pub path: String,
    /// `None` for a file the tool created.
    pub before: Option<String>,
    pub after: String,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false, images: Vec::new(), change: None }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true, images: Vec::new(), change: None }
    }

    pub fn with_change(self, change: FileChange) -> Self {
        Self { change: Some(Box::new(change)), ..self }
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
    /// Read-only tools may overlap. Unknown mutations own the workspace.
    /// Orchestrators acquire guards through the tools they invoke instead.
    fn access(&self, _input: &Value) -> scheduling::Access {
        if self.read_only() { scheduling::Access::Concurrent } else { scheduling::Access::Workspace }
    }
    async fn run(&self, input: Value) -> ToolOutput;
}

/// Name-keyed set of tools. A `BTreeMap` keeps the tool list order stable
/// across requests, which keeps the prompt-cache prefix stable.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    /// The tracker the file tools share, if they are registered.
    files: Option<FileTracker>,
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
        registry.register(BashOutputTool);
        registry.register(KillShellTool);
        registry.register(WaitTasksTool);
        registry.register(GrepTool);
        registry.register(GlobTool);
        // `todo_write` is off: current models plan well enough without it. The tool, its
        // rendering and its compaction handling stay, so re-enabling it is this one line.
        // registry.register(TodoTool);
        registry.register(ReadTool::new(files.clone()));
        registry.register(EditTool::new(files.clone()));
        registry.register(WriteTool::new(files.clone()));
        registry.files = Some(files);
        registry
    }

    /// Gives the file tools present (`read`, `edit`, `write`) the tracker
    /// `files`, e.g. a subagent's own.
    pub fn track_files(&mut self, files: &FileTracker) {
        if self.get("read").is_some() {
            self.register(ReadTool::new(files.clone()));
        }
        if self.get("edit").is_some() {
            self.register(EditTool::new(files.clone()));
        }
        if self.get("write").is_some() {
            self.register(WriteTool::new(files.clone()));
        }
        self.files = Some(files.clone());
    }

    /// The tracker of the file tools, e.g. to re-attach the files read
    /// most recently after compacting the conversation.
    pub fn files(&self) -> Option<&FileTracker> {
        self.files.as_ref()
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.register_arc(Arc::new(tool));
    }

    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

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

    /// Tool definitions for the model.
    ///
    /// `eager_input_streaming` lets an Anthropic API stream large inputs
    /// (e.g. file contents for `write`) as they are generated. The API then
    /// no longer validates the input, so built-in tools deserialize into a
    /// strict struct before running; MCP tools, whose schemas are arbitrary,
    /// leave it off so the API validates their input.
    pub fn definitions(&self) -> Vec<ToolDef> {
        self.tools
            .values()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
                eager_input_streaming: t.eager_input_streaming(),
            })
            .collect()
    }

    pub async fn run(&self, name: &str, input: Value) -> ToolOutput {
        match self.tools.get(name) {
            Some(tool) => {
                let _guard = scheduling::acquire(tool.access(&input)).await;
                tool.run(input).await
            }
            None => ToolOutput::error(format!("Unknown tool: {name}")),
        }
    }
}

/// Keep at most `max` bytes of `s`, cutting on a char boundary and noting
/// how much was dropped.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = s.len() - end;
    if dropped == 0 {
        return s.to_string();
    }
    format!("{}\n... [truncated {dropped} bytes]", &s[..end])
}

/// Parses tool input into its typed form; unknown fields are rejected by
/// `#[serde(deny_unknown_fields)]` on each input struct.
pub(crate) fn parse_input<T: serde::de::DeserializeOwned>(input: Value) -> Result<T, ToolOutput> {
    serde_json::from_value(input).map_err(|e| ToolOutput::error(format!("Invalid input: {e}")))
}

/// Remembers which files the model has read, and their modification time at
/// that point, so `edit` and `write` refuse to change a file the model has
/// not seen or that changed since it was read.
#[derive(Clone, Default)]
pub struct FileTracker {
    /// Each file's modification time when it was recorded, and a counter
    /// value saying how recently that was.
    seen: Arc<Mutex<Seen>>,
}

/// Held while a tool checks, changes and records a file, so changes to the
/// same file apply one after another: calls from one response, and those of
/// agents running at once (subagents, web tasks), each with its own tracker.
/// Without it, an agent could write a file over another's change that
/// landed after its check.
static CHANGING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Default)]
struct Seen {
    files: HashMap<PathBuf, (Option<SystemTime>, u64)>,
    counter: u64,
    /// The `AGENTS.md` files of subdirectories the model was shown.
    notes: HashSet<PathBuf>,
}

impl FileTracker {
    fn key(path: &Path) -> PathBuf {
        std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
    }

    fn mtime(path: &Path) -> Option<SystemTime> {
        std::fs::metadata(path).and_then(|m| m.modified()).ok()
    }

    pub fn record(&self, path: &Path) {
        let mut seen = self.seen.lock().unwrap();
        seen.counter += 1;
        let entry = (Self::mtime(path), seen.counter);
        seen.files.insert(Self::key(path), entry);
    }

    /// The `n` files recorded most recently, most recent first, as
    /// absolute paths.
    pub fn recent(&self, n: usize) -> Vec<PathBuf> {
        let seen = self.seen.lock().unwrap();
        let mut files: Vec<(&PathBuf, u64)> = seen.files.iter().map(|(path, (_, at))| (path, *at)).collect();
        files.sort_by_key(|(_, at)| std::cmp::Reverse(*at));
        files.into_iter().take(n).map(|(path, _)| path.clone()).collect()
    }

    /// Forgets every file but `keep`, e.g. once the model no longer sees
    /// the others' contents; it no longer sees subdirectories' notes either.
    pub fn retain(&self, keep: &[PathBuf]) {
        let mut seen = self.seen.lock().unwrap();
        seen.files.retain(|path, _| keep.contains(path));
        seen.notes.clear();
    }

    /// The `AGENTS.md` files of the directories between `root` (whose own
    /// is in the system prompt) and `file` that the model was not shown
    /// yet, outermost first; they count as shown from now on.
    pub fn new_notes(&self, root: &Path, file: &Path) -> Vec<PathBuf> {
        let file = Self::key(file);
        let Ok(inside) = file.strip_prefix(root) else { return Vec::new() };
        let dirs: Vec<PathBuf> =
            inside.parent().into_iter().flat_map(Path::ancestors).filter(|d| !d.as_os_str().is_empty()).map(|d| root.join(d)).collect();
        let mut seen = self.seen.lock().unwrap();
        let mut notes = Vec::new();
        for dir in dirs.into_iter().rev() {
            let note = dir.join("AGENTS.md");
            if note.is_file() && seen.notes.insert(note.clone()) {
                notes.push(note);
            }
        }
        notes
    }

    /// Waits for any other agent's or call's file change to finish; hold the
    /// guard from checking the file until recording the change.
    pub async fn changing(&self) -> tokio::sync::MutexGuard<'static, ()> {
        CHANGING.lock().await
    }

    /// Ok for a new file, or for an existing file read since it last changed.
    pub fn check_writable(&self, path: &Path) -> Result<(), String> {
        if !path.exists() {
            return Ok(());
        }
        match self.seen.lock().unwrap().files.get(&Self::key(path)) {
            None => Err(format!(
                "{} already exists; read it with the `read` tool before changing it",
                path.display()
            )),
            Some((seen, _)) if *seen != Self::mtime(path) => Err(format!(
                "{} changed since it was last read (another agent, a command or the user may have \
                 changed it); read it again before changing it",
                path.display()
            )),
            Some(_) => Ok(()),
        }
    }
}
