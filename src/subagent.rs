//! Subagents: the main agent delegates a self-contained task to a fresh
//! agent with its own conversation, system prompt and tool set, and gets
//! back only its report. This keeps long searches out of the main context,
//! and several subagents can run in parallel.
//!
//! A subagent runs in the foreground (the `agent` call waits for its report)
//! or in the background (the call returns at once, and the report arrives in
//! the parent's inbox when it is done; see [`runtime`]). Either way its
//! conversation is kept, so `message_agent` can send it follow-ups, and
//! `stop_agent` stops it.
//!
//! The built-in `explore` agent only has `grep` and `glob`; `general-purpose`
//! has every tool but these three. Custom agents are Markdown files in
//! `.harness/agents/` or `.claude/agents/` (project or `~`):
//!
//! ```markdown
//! ---
//! name: reviewer
//! description: Reviews the current diff for bugs. Use after making changes.
//! tools: read, grep, bash        # optional; default: every tool allowed
//! permission: read-only          # default; `write` allows modifying tools
//! model: relay-b/gpt-5-mini       # optional; default: `subagent_model` or the main model
//! ---
//! You are a code reviewer...
//! ```
//!
//! Custom agents default to read-only: such an agent only gets tools marked
//! read-only (`read`, `grep`, `skill`, MCP tools with `readOnlyHint`), so no
//! `bash` or `write` unless it declares `permission: write`.

pub mod runtime;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, Weak};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::Agent;
use crate::hooks::{self, Hooks};
use crate::llm::Client;
use crate::skills::{Fork, PluginDirs, Skill, parse_frontmatter, split_frontmatter};
use crate::tools::{FileTracker, Tool, ToolOutput, ToolRegistry, parse_input};
pub use runtime::{Subagents, scope};

const MAX_RESULT_BYTES: usize = 50_000;

/// The tools that start and steer subagents. Subagents never get them, so
/// they cannot start subagents of their own.
pub const TOOLS: [&str; 3] = ["agent", "message_agent", "stop_agent"];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Permission {
    ReadOnly,
    Write,
}

impl Permission {
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::ReadOnly => "read-only",
            Permission::Write => "write",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub prompt: String,
    pub permission: Permission,
    /// Tool names (a trailing `*` matches a prefix); `None` allows all tools
    /// the permission grants.
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
    /// Where the definition came from; `None` for built-ins.
    pub path: Option<PathBuf>,
}

impl AgentDef {
    fn allows(&self, tool: &str) -> bool {
        match &self.tools {
            None => true,
            Some(names) => names.iter().any(|n| match n.strip_suffix('*') {
                Some(prefix) => tool.starts_with(prefix),
                None => n == tool,
            }),
        }
    }
}

pub fn explore() -> AgentDef {
    AgentDef {
        name: "explore".into(),
        description: "Codebase search agent with only the `grep` and `glob` tools. Use it for open-ended \
                      questions that need many searches (where is X implemented, which files use Y, how \
                      is Z wired up) when you only need the conclusion, not the search output. It cannot \
                      modify anything."
            .into(),
        prompt: "You are an exploration agent: a code search specialist working for another agent. You \
                 get one question about the codebase; answer it by searching.\n\n\
                 Your tools are `grep` and `glob`:\n\
                 - Find candidate files with `glob` when you know their names or paths (`**/*_test.go`), \
                 and with `grep`'s default `files_with_matches` mode when you know what they contain, \
                 narrowing with its `glob` and `path`.\n\
                 - Then look at the code with `output_mode: \"content\"` and `context` lines around a \
                 match (for example the definition of a function), rather than broad patterns that \
                 return everything.\n\
                 - Run independent searches in parallel, and stop once you can answer.\n\n\
                 Your final message is the only thing the caller sees. Make it a concise report: the \
                 answer first, then the key locations as `path:line` with a phrase on each, then anything \
                 you could not determine. Quote code only when a few lines are essential."
            .into(),
        permission: Permission::ReadOnly,
        tools: Some(vec!["grep".into(), "glob".into()]),
        model: None,
        path: None,
    }
}

/// A coding agent with every tool the main agent has, bar the subagent tools.
pub fn general_purpose() -> AgentDef {
    AgentDef {
        name: "general-purpose".into(),
        description: "General-purpose agent for multi-step work: researching a question that needs \
                      reading files and running commands, or making a well-scoped change and testing \
                      it. It has the same tools as you. When several change files at once, give each \
                      its own files."
            .into(),
        prompt: "You are a subagent: another agent delegated a task to you. Complete it with your tools. \
                 Read the code you change, keep unrelated work intact, and check your changes with \
                 relevant tests or builds. Other agents may be working in the same directory: change \
                 only the files your task is about, and never revert changes you did not make. Do not \
                 commit, push or publish anything unless the task says to.\n\n\
                 Your final message is the only thing the caller sees. Make it a concise report: what \
                 you found or did, the files you changed, how you verified it, and anything left open."
            .into(),
        permission: Permission::Write,
        tools: None,
        model: None,
        path: None,
    }
}

/// Default search roots, highest precedence first.
pub fn default_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from(".harness/agents"), PathBuf::from(".claude/agents")];
    if let Some(home) = std::env::var_os("HOME") {
        let home = Path::new(&home);
        roots.push(home.join(".harness/agents"));
        roots.push(home.join(".claude/agents"));
    }
    roots
}

/// The built-in agents plus `<root>/*.md` definitions. On a name clash the
/// earlier root wins, and any file overrides a built-in.
pub fn discover(roots: &[PathBuf]) -> (Vec<AgentDef>, Vec<String>) {
    let mut defs: BTreeMap<String, AgentDef> = BTreeMap::new();
    let mut warnings = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else { continue };
        let mut files: Vec<PathBuf> =
            entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "md")).collect();
        files.sort();
        for file in files {
            let parsed = std::fs::read_to_string(&file)
                .map_err(|e| e.to_string())
                .and_then(|t| parse_def(&t, &file, Permission::ReadOnly));
            match parsed {
                Ok(def) => {
                    defs.entry(def.name.clone()).or_insert(def);
                }
                Err(e) => warnings.push(format!("{}: {e}", file.display())),
            }
        }
    }
    for builtin in [explore(), general_purpose()] {
        defs.entry(builtin.name.clone()).or_insert(builtin);
    }
    (defs.into_values().collect(), warnings)
}

/// A plugin's agent from `file`, named `<prefix>:<name>`, with the
/// plugin's path variables replaced in its prompt. Unlike the user's own
/// agents it may write unless it says `permission: read-only`: plugins are
/// written for Claude Code, where `tools` is the whole list of what an
/// agent gets (all of them without it), and the user chose to enable it.
pub fn plugin_def(file: &Path, prefix: &str, plugin: &PluginDirs) -> Result<AgentDef, String> {
    let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    let mut def = parse_def(&text, file, Permission::Write)?;
    def.name = format!("{prefix}:{}", def.name);
    def.prompt = plugin.expand_prompt(&def.prompt);
    Ok(def)
}

/// The tools a `tools` or `allowed-tools` field names: `read, grep`,
/// `[read, grep]` or a YAML list (joined by the frontmatter parser into
/// `- read - grep`). Claude Code names such as `Read` or `KillShell` become
/// the harness's, and what follows a name in parentheses
/// (`Bash(git add:*)`) is dropped; MCP names keep their case.
pub(crate) fn tool_list(value: &str) -> Vec<String> {
    let mut items = Vec::new();
    let (mut item, mut depth) = (String::new(), 0);
    for c in value.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ if depth > 0 => {}
            ',' | '[' | ']' => items.push(std::mem::take(&mut item)),
            c if c.is_whitespace() => items.push(std::mem::take(&mut item)),
            c => item.push(c),
        }
    }
    items.push(item);
    let mut tools: Vec<String> = Vec::new();
    for name in items.iter().map(|n| n.trim_matches(['"', '\''])).filter(|n| !n.is_empty() && *n != "-") {
        let name = match hooks::tool_named(name) {
            Some(tool) => tool.to_string(),
            None if name.starts_with("mcp__") => name.to_string(),
            None => name.to_ascii_lowercase(),
        };
        // `Glob` and `LS` are both `glob`.
        if !tools.contains(&name) {
            tools.push(name);
        }
    }
    tools
}

/// A `model` field: Claude Code's model aliases name no model a gateway
/// knows, so they mean the default, as `inherit` does.
pub(crate) fn model_field(model: Option<&String>) -> Option<String> {
    let model = model?.trim();
    (!model.is_empty() && !matches!(model, "inherit" | "sonnet" | "opus" | "haiku")).then(|| model.to_string())
}

/// The agent defined in `text`, with `default` permission unless it gives
/// one.
fn parse_def(text: &str, file: &Path, default: Permission) -> Result<AgentDef, String> {
    let (front, body) = split_frontmatter(text).ok_or("missing `---` frontmatter")?;
    let fields = parse_frontmatter(front);
    let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let name = fields.get("name").cloned().unwrap_or(stem);
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(format!("invalid agent name `{name}`"));
    }
    let description = fields.get("description").cloned().filter(|d| !d.is_empty()).ok_or("missing `description`")?;
    let permission = match fields.get("permission").map(|p| p.to_ascii_lowercase()).as_deref() {
        None => default,
        Some("read-only" | "readonly" | "read_only") => Permission::ReadOnly,
        Some("write") => Permission::Write,
        Some(other) => return Err(format!("unknown permission `{other}` (use `read-only` or `write`)")),
    };
    let tools = fields.get("tools").map(|t| tool_list(t));
    let model = model_field(fields.get("model"));
    let prompt = body.trim().to_string();
    if prompt.is_empty() {
        return Err("empty system prompt".into());
    }
    let path = std::path::absolute(file).ok();
    Ok(AgentDef { name, description, prompt, permission, tools, model, path })
}

/// Runs `context: fork` skills for the `skill` tool and the frontends,
/// through the `agent` tool, which is built after the `skill` tool it
/// starts subagents with.
#[derive(Clone, Default)]
pub struct Forks(Arc<RwLock<Weak<AgentTool>>>);

impl Forks {
    /// Runs forks with `tool` from now on.
    pub fn set(&self, tool: &Arc<AgentTool>) {
        *self.0.write().unwrap() = Arc::downgrade(tool);
    }

    /// Runs skill `skill`, whose instructions are `prompt`, in a subagent,
    /// and returns its report. Where no subagent can start (in a subagent,
    /// or without the `agent` tool), the instructions come back to follow
    /// in place.
    pub async fn run(&self, skill: &str, fork: &Fork, prompt: String) -> ToolOutput {
        let tool = self.0.read().unwrap().upgrade();
        match tool {
            Some(tool) if !runtime::nested() => tool.fork(skill, fork, prompt).await,
            _ => ToolOutput::ok(format!(
                "(The `{skill}` skill would run in a subagent of its own, which cannot start here; follow it \
                 yourself.)\n\n{prompt}"
            )),
        }
    }
}

impl Forks {
    /// Runs the fork skill `skill` the user typed (`/<name> arguments`) in
    /// a subagent of the conversation that owns `subagents`, on `client`;
    /// returns the message that hands its report to that conversation's
    /// model.
    pub async fn run_for_user(
        &self,
        skill: &Skill,
        arguments: &str,
        subagents: Arc<Subagents>,
        client: Client,
    ) -> Result<String, String> {
        let fork = skill.fork.as_ref().ok_or_else(|| format!("{} does not run in a subagent", skill.name))?;
        let prompt = skill.render(arguments).await?;
        let output = scope(subagents, client, false, self.run(&skill.name, fork, prompt)).await;
        if output.is_error {
            return Err(output.content);
        }
        let arguments = arguments.trim();
        let with = if arguments.is_empty() { String::new() } else { format!(" ({arguments})") };
        Ok(format!(
            "The user ran the `{}` skill{with}, which ran in a subagent of its own. Its report:\n\n{}",
            skill.name, output.content
        ))
    }
}

/// Receives subagent progress as JSON events: a tool call (`subagent`) or a
/// change of status (`subagent_status`). The web UI logs them as they are.
pub type Reporter = Arc<dyn Fn(&Value) + Send + Sync>;

/// The `agent` tool.
pub struct AgentTool {
    defs: Arc<Vec<AgentDef>>,
    /// Tools subagents are chosen from. Must not contain the subagent
    /// tools, so subagents cannot spawn subagents.
    base: ToolRegistry,
    client: Client,
    /// Where progress goes unless the conversation set its own place.
    reporter: Reporter,
    description: String,
}

impl AgentTool {
    pub fn new(defs: Arc<Vec<AgentDef>>, base: ToolRegistry, client: Client, reporter: Reporter) -> Self {
        let mut description = String::from(
            "Delegate a task to a subagent that works in its own separate context and returns only its \
             final report. Use it to keep long searches and file reading out of this conversation. \
             The subagent cannot see this conversation, so `prompt` must be self-contained: state the \
             goal, what you already know, and what the report should contain. For independent \
             questions, launch several subagents in parallel in one response.\n\n\
             By default the call waits for the report. With `run_in_background: true` it returns at \
             once and you keep working; the report arrives by itself in a later message when the \
             subagent is done, even after your turn has ended. Do not poll or wait for it, and never \
             guess what it will find. Every subagent gets an `agent_id`: `message_agent` continues \
             it with its context intact, `stop_agent` stops it. The report comes to you, not the \
             user; relay what matters.\n\nAvailable agents:",
        );
        for d in defs.iter() {
            let access = match d.permission {
                Permission::ReadOnly => "read-only",
                Permission::Write => "can modify files",
            };
            description.push_str(&format!("\n- {} ({access}): {}", d.name, d.description));
        }
        Self { defs, base, client, reporter, description }
    }

    /// Tool names in `defs` that no available tool matches.
    /// Plugins' agents (`<plugin>:<name>`) are left out: they are written
    /// for Claude Code, some of whose tools the harness lacks, and the user
    /// has no lists to fix.
    pub fn unknown_tools(&self) -> Vec<(String, String)> {
        let mut unknown = Vec::new();
        for def in self.defs.iter().filter(|d| !d.name.contains(':')) {
            let registry = self.registry_for(&AgentDef { tools: None, permission: Permission::Write, ..def.clone() });
            for name in def.tools.iter().flatten() {
                let probe = AgentDef { tools: Some(vec![name.clone()]), ..def.clone() };
                if !registry.names().any(|t| probe.allows(t)) {
                    unknown.push((def.name.clone(), name.clone()));
                }
            }
        }
        unknown
    }

    /// Builds the tool set for one subagent. File tools get a fresh tracker,
    /// so a subagent's reads never count as reads by the main agent.
    pub fn registry_for(&self, def: &AgentDef) -> ToolRegistry {
        let mut registry = self.base.clone();
        registry.track_files(&FileTracker::default());
        if def.permission == Permission::ReadOnly {
            registry.retain(|t| t.read_only());
        }
        registry.retain(|t| !TOOLS.contains(&t.name()) && def.allows(t.name()));
        registry
    }

    /// The client for a subagent: the agent's own model, else the configured
    /// subagent model, else the caller's.
    pub fn client_for(&self, def: &AgentDef) -> anyhow::Result<Client> {
        client_from(def, &self.client)
    }
}

/// The client for `def` when the caller's model is `parent`.
fn client_from(def: &AgentDef, parent: &Client) -> anyhow::Result<Client> {
    match def.model.as_deref().or(parent.providers().subagent_model.as_deref()) {
        Some(spec) => parent.switch(spec),
        None => Ok(parent.clone()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    agent_type: String,
    description: String,
    prompt: String,
    #[serde(default)]
    run_in_background: bool,
}

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        "agent"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        let names: Vec<&str> = self.defs.iter().map(|d| d.name.as_str()).collect();
        json!({
            "type": "object",
            "properties": {
                "agent_type": {"type": "string", "enum": names, "description": "Which agent to run."},
                "description": {"type": "string", "description": "A 3-6 word summary of the task, shown to the user."},
                "prompt": {"type": "string", "description": "The complete, self-contained task for the subagent."},
                "run_in_background": {
                    "type": "boolean",
                    "description": "Return at once instead of waiting; the report arrives in a later message."
                }
            },
            "required": ["agent_type", "description", "prompt"],
            "additionalProperties": false
        })
    }

    /// Only as read-only as the agents it can start.
    fn read_only(&self) -> bool {
        self.defs.iter().all(|d| d.permission == Permission::ReadOnly)
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let Some(def) = self.defs.iter().find(|d| d.name == input.agent_type) else {
            return ToolOutput::error(format!("Unknown agent type: {}", input.agent_type));
        };
        let tools = self.registry_for(def);
        self.launch(def, tools, &input.description, input.prompt, input.run_in_background).await
    }
}

impl AgentTool {
    /// Starts a subagent of type `def` with `tools` on `prompt`; returns its
    /// report, or with `background` that it started.
    async fn launch(&self, def: &AgentDef, tools: ToolRegistry, description: &str, prompt: String, background: bool) -> ToolOutput {
        // Outside a conversation (the web UI's tool page) a subagent can
        // only run in the foreground, on the tool's own model.
        let (subagents, parent) = match runtime::current() {
            Some((subagents, client)) => (subagents, client),
            None if background => {
                return ToolOutput::error("run_in_background needs a conversation to report back to");
            }
            None => (Arc::new(Subagents::new(Arc::default(), Arc::default(), Hooks::default())), self.client.clone()),
        };
        let client = match client_from(def, &parent) {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("Agent `{}` has an unusable model: {e:#}", def.name)),
        };
        let agent = Agent::new(client, tools, crate::prompt::subagent(def));
        let run = match subagents.spawn(agent, &def.name, description, prompt, background, &self.reporter) {
            Ok(run) => run,
            Err(e) => return ToolOutput::error(format!("{e:#}")),
        };
        if !background {
            return run.report().await;
        }
        ToolOutput::ok(format!(
            "Started {} agent {} in the background. Its report will arrive in a later message when it \
             finishes; do not poll, sleep or wait for it. Carry on with other work, or end your turn if \
             there is nothing else to do.",
            def.name, run.child.id
        ))
    }

    /// Runs skill `skill` in a subagent of the type its `fork` names (with
    /// at most its `allowed-tools`), on the skill's instructions `prompt`.
    async fn fork(&self, skill: &str, fork: &Fork, prompt: String) -> ToolOutput {
        let name = fork.agent.as_deref().unwrap_or("general-purpose");
        let Some(def) = self.defs.iter().find(|d| d.name == name) else {
            return ToolOutput::error(format!("Skill `{skill}` runs in agent `{name}`, which does not exist"));
        };
        let mut def = def.clone();
        if fork.model.is_some() {
            def.model = fork.model.clone();
        }
        let mut tools = self.registry_for(&def);
        if let Some(allowed) = &fork.allowed_tools {
            let allowed = AgentDef { tools: Some(allowed.clone()), ..def.clone() };
            tools.retain(|t| allowed.allows(t.name()));
        }
        self.launch(&def, tools, &format!("/{skill}"), prompt, false).await
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageInput {
    agent_id: String,
    message: String,
    #[serde(default)]
    run_in_background: bool,
}

/// The `message_agent` tool: continues a subagent of this conversation.
pub struct MessageAgentTool;

#[async_trait]
impl Tool for MessageAgentTool {
    fn name(&self) -> &str {
        "message_agent"
    }

    fn description(&self) -> &str {
        "Send a message to a subagent you started in this conversation, by its agent_id. It keeps its \
         context, so this is the way to ask a follow-up, correct its course or give it more work. A \
         finished or stopped subagent runs again on the message: the call waits for its new report, or \
         with run_in_background returns at once and the report arrives in a later message. A running \
         one reads the message at its next step, and reports as it would have anyway."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": {"type": "string", "description": "The subagent's agent_id."},
                "message": {"type": "string", "description": "What to tell it; it sees nothing else of this conversation."},
                "run_in_background": {"type": "boolean", "description": "Don't wait for its report (when it runs again)."}
            },
            "required": ["agent_id", "message"],
            "additionalProperties": false
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let input: MessageInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let Some((subagents, _)) = runtime::current() else {
            return ToolOutput::error("message_agent only works within a conversation");
        };
        let id = &input.agent_id;
        match subagents.send(id, input.message, input.run_in_background) {
            Err(e) => ToolOutput::error(format!("{e:#}")),
            Ok(runtime::Sent::Queued) => {
                ToolOutput::ok(format!("Subagent {id} is running; it will read the message at its next step."))
            }
            Ok(runtime::Sent::Started(_)) if input.run_in_background => ToolOutput::ok(format!(
                "Subagent {id} is working on the message in the background; its report will arrive in a \
                 later message."
            )),
            Ok(runtime::Sent::Started(run)) => run.report().await,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StopInput {
    agent_id: String,
}

/// The `stop_agent` tool.
pub struct StopAgentTool;

#[async_trait]
impl Tool for StopAgentTool {
    fn name(&self) -> &str {
        "stop_agent"
    }

    fn description(&self) -> &str {
        "Stop a running subagent of this conversation, by its agent_id, when its work is no longer \
         needed or it is going the wrong way. Changes it already made remain. Its context is kept: \
         message_agent continues it."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"agent_id": {"type": "string", "description": "The subagent's agent_id."}},
            "required": ["agent_id"],
            "additionalProperties": false
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let input: StopInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let Some((subagents, _)) = runtime::current() else {
            return ToolOutput::error("stop_agent only works within a conversation");
        };
        let id = &input.agent_id;
        match subagents.stop(id, false) {
            Ok(true) => ToolOutput::ok(format!("Stopped subagent {id}. message_agent can continue it.")),
            Ok(false) => ToolOutput::ok(format!("Subagent {id} was not running.")),
            Err(e) => ToolOutput::error(format!("{e:#}")),
        }
    }
}

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Providers;
    use crate::llm::config::tests::{file, no_env};
    use crate::tools::{GrepTool, ReadTool};

    fn client() -> Client {
        let file = file(json!({
            "model": "gw/main-model",
            "providers": {"gw": {"protocol": "anthropic", "base_url": "http://127.0.0.1:9", "api_key": "test"}}
        }));
        Client::new(Providers::from_parts(&file, &no_env).unwrap()).unwrap()
    }

    fn agent_tool(defs: Vec<AgentDef>) -> AgentTool {
        let base = ToolRegistry::with_defaults(FileTracker::default());
        AgentTool::new(Arc::new(defs), base, client(), Arc::new(|_: &Value| {}))
    }

    fn names(r: &ToolRegistry) -> Vec<&str> {
        r.names().collect()
    }

    #[test]
    fn explore_only_gets_grep_and_glob() {
        let tool = agent_tool(vec![explore()]);
        assert_eq!(names(&tool.registry_for(&explore())), vec!["glob", "grep"]);
        assert!(tool.read_only());
    }

    #[test]
    fn read_only_is_the_default() {
        let mut def = explore();
        def.tools = None;
        assert_eq!(names(&agent_tool(vec![]).registry_for(&def)), vec!["glob", "grep", "read"]);
    }

    #[test]
    fn plugin_agents_get_the_tools_they_list() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = PluginDirs::new(dir.path().into(), dir.path().into());
        let def = |name: &str, front: &str| {
            let file = dir.path().join(format!("{name}.md"));
            std::fs::write(&file, format!("---\ndescription: d\n{front}---\nprompt\n")).unwrap();
            plugin_def(&file, "p", &plugin).unwrap()
        };
        let tool = agent_tool(vec![]);
        // Claude Code's names, some the harness lacks; `tools` is the whole list.
        let fixer = def("fixer", "tools: Read, Grep, Glob, LS, Edit, Bash, WebFetch\n");
        assert_eq!(fixer.permission, Permission::Write);
        assert_eq!(names(&tool.registry_for(&fixer)), vec!["bash", "edit", "glob", "grep", "read"]);
        // Without `tools`, every tool but the subagent ones.
        let all = tool.registry_for(&def("all", ""));
        assert!(all.get("write").is_some() && all.get("bash").is_some(), "{:?}", names(&all));
        // It can still ask for less.
        let reader = def("reader", "tools: Read, Bash\npermission: read-only\n");
        assert_eq!(names(&tool.registry_for(&reader)), vec!["read"]);
        // The user's own agents stay read-only by default.
        let own = "---\ndescription: d\ntools: Read, Bash\n---\np\n";
        let own = parse_def(own, Path::new("own.md"), Permission::ReadOnly);
        assert_eq!(names(&tool.registry_for(&own.unwrap())), vec!["read"]);
    }

    #[test]
    fn write_permission_and_tool_lists() {
        let mut def = explore();
        def.permission = Permission::Write;
        def.tools = Some(vec!["read".into(), "write".into(), "bash".into()]);
        let tool = agent_tool(vec![def.clone()]);
        let registry = tool.registry_for(&def);
        assert_eq!(names(&registry), vec!["bash", "read", "write"]);
        assert!(!tool.read_only());

        // A read-only agent cannot get write tools even by listing them.
        def.permission = Permission::ReadOnly;
        assert_eq!(names(&tool.registry_for(&def)), vec!["read"]);
    }

    #[test]
    fn subagent_reads_do_not_count_for_main_agent() {
        let files = FileTracker::default();
        let mut base = ToolRegistry::new();
        base.register(ReadTool::new(files.clone()));
        base.register(GrepTool);
        let tool = AgentTool::new(Arc::new(vec![explore()]), base, client(), Arc::new(|_: &Value| {}));
        let registry = tool.registry_for(&AgentDef { tools: None, ..explore() });
        let read = registry.get("read").unwrap().clone();

        let f = tempfile::NamedTempFile::new().unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(read.run(json!({"path": f.path()})));
        assert!(files.check_writable(f.path()).is_err(), "main tracker must not see the subagent's read");
    }

    #[test]
    fn parses_definitions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("reviewer.md"),
            "---\ndescription: Reviews diffs\ntools: Read, Grep, mcp__gh__*\nmodel: claude-sonnet-5\n---\nYou review code.\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("fixer.md"),
            "---\nname: fixer\ndescription: Fixes things\npermission: write\ntools:\n  - bash\n  - write\n---\nFix it.\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("explore.md"), "---\ndescription: custom explore\n---\nMine.\n").unwrap();
        std::fs::write(dir.path().join("bad.md"), "---\ndescription: x\npermission: root\n---\nbody\n").unwrap();

        let (defs, warnings) = discover(&[dir.path().into()]);
        let by_name: BTreeMap<&str, &AgentDef> = defs.iter().map(|d| (d.name.as_str(), d)).collect();
        assert_eq!(by_name.keys().copied().collect::<Vec<_>>(), vec!["explore", "fixer", "general-purpose", "reviewer"]);
        assert_eq!(by_name["explore"].description, "custom explore", "files override built-ins");
        let reviewer = by_name["reviewer"];
        assert_eq!(reviewer.permission, Permission::ReadOnly);
        assert_eq!(reviewer.tools.as_deref(), Some(&["read".to_string(), "grep".into(), "mcp__gh__*".into()][..]));
        assert!(reviewer.allows("mcp__gh__list_issues") && !reviewer.allows("bash"));
        assert_eq!(reviewer.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(by_name["fixer"].permission, Permission::Write);
        assert_eq!(by_name["fixer"].tools.as_deref(), Some(&["bash".to_string(), "write".into()][..]));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("unknown permission"));
    }

    #[test]
    fn tool_lists() {
        assert_eq!(tool_list("Bash(git add:*), Read Grep(*), mcp__gh__*"), ["bash", "read", "grep", "mcp__gh__*"]);
        assert_eq!(tool_list("[Glob, LS]"), ["glob"]);
        assert_eq!(tool_list("- read - 'write'"), ["read", "write"]);
        assert_eq!(model_field(Some(&"opus".to_string())), None);
        assert_eq!(model_field(Some(&" gw/m ".to_string())).as_deref(), Some("gw/m"));
    }

    #[tokio::test]
    async fn fork_skills_run_in_a_subagent() {
        use crate::agent::mock::{gateway, texts};
        let (client, mut requests) = gateway(|_| (vec![json!({"type": "text", "text": "all done"})], "end_turn", 10)).await;
        let base = ToolRegistry::with_defaults(FileTracker::default());
        let defs = Arc::new(vec![explore(), general_purpose()]);
        let tool = Arc::new(AgentTool::new(defs, base, client.clone(), Arc::new(|_: &Value| {})));
        let forks = Forks::default();
        let fork = Fork { allowed_tools: Some(vec!["read".into(), "bash".into(), "write".into()]), ..Fork::default() };
        let out = forks.run("p:check", &fork, "Check it".into()).await;
        assert!(out.content.contains("follow it yourself") && out.content.ends_with("Check it"), "no `agent` tool yet");

        forks.set(&tool);
        let out = forks.run("p:check", &fork, "Check it".into()).await;
        assert!(!out.is_error && out.content.starts_with("all done"), "{}", out.content);
        let request = requests.recv().await.unwrap();
        let tools: Vec<&str> = request["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(tools, ["bash", "read", "write"], "general-purpose's tools, as far as allowed-tools goes");
        assert_eq!(texts(&request["messages"][0]), ["Check it"]);

        // An explore fork gets only what both allow.
        let explore_fork = Fork { agent: Some("explore".into()), allowed_tools: Some(vec!["grep".into(), "bash".into()]), ..Fork::default() };
        forks.run("p:find", &explore_fork, "Find it".into()).await;
        let request = requests.recv().await.unwrap();
        assert_eq!(request["tools"].as_array().unwrap().len(), 1);
        let missing = Fork { agent: Some("nope".into()), ..Fork::default() };
        assert!(forks.run("p:x", &missing, "x".into()).await.is_error);

        // A subagent runs it in place.
        let subagents = Arc::new(Subagents::new(Arc::default(), Arc::default(), Hooks::default()));
        let out = scope(subagents, client, true, forks.run("p:check", &fork, "Check it".into())).await;
        assert!(out.content.contains("follow it yourself"), "{}", out.content);
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn reports_unknown_tool_names() {
        let mut def = explore();
        def.tools = Some(vec!["read".into(), "webfetch".into()]);
        let tool = agent_tool(vec![def]);
        assert_eq!(tool.unknown_tools(), vec![("explore".to_string(), "webfetch".to_string())]);
    }
}
