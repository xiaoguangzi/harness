//! Hooks: commands from the settings file (and the plugins it enables) that
//! run at points of the agent loop, in the format of Claude Code's (which
//! Codex shares), so scripts written for those work here too.
//!
//! ```json
//! "hooks": {
//!   "PreToolUse": [
//!     { "matcher": "Bash", "hooks": [{ "type": "command", "command": "~/bin/guard.sh", "timeout": 30 }] }
//!   ],
//!   "Stop": [{ "hooks": [{ "type": "prompt", "prompt": "Did it run the tests? $ARGUMENTS" }] }]
//! }
//! ```
//!
//! A command hook gets the event as JSON on stdin and answers with its exit
//! code (2 blocks, with stderr as the reason) or JSON on stdout; it runs
//! through `sh -c`, or with `args` directly. A prompt hook asks the
//! conversation's model instead, which answers `{"ok": false, "reason": …}`
//! to block. Tools are matched by their Claude Code names as well (`Bash`,
//! `Edit|Write`), hooks see them under those names, and the file tools'
//! `path` also comes as `file_path`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures::StreamExt;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::agent::Inbox;
use crate::extensions::skills::PluginDirs;
use crate::llm::{Client, Message};
use crate::security::permissions::HookDecision;
use crate::tools::{KillGroup, ToolOutput};

/// How long a hook may run unless it sets `timeout`.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
/// The same for SessionEnd hooks, which hold up quitting, and the most they
/// may set.
const SESSION_END_TIMEOUT: Duration = Duration::from_millis(1500);
const MAX_SESSION_END_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a prompt hook may take unless it sets `timeout`.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(30);
/// The most hooks of one event that run at once; the rest wait their turn.
const MAX_CONCURRENT: usize = 12;
/// Text a hook gives the model is cut to this many bytes.
const MAX_CONTEXT_BYTES: usize = 10_000;
/// What a prompt hook's model is told.
const PROMPT_SYSTEM: &str = "You evaluate a hook of a coding agent: you get an instruction and the input of \
    the event the hook runs for, and decide. Answer with only a JSON object: {\"ok\": true} to let the agent \
    go ahead, or {\"ok\": false, \"reason\": \"...\"} to stop it, where the reason tells the agent what to do \
    instead.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
pub enum Event {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    UserPromptSubmit,
    Stop,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
    SessionStart,
    SessionEnd,
    Notification,
    PermissionRequest,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Event::PreToolUse => "PreToolUse",
            Event::PostToolUse => "PostToolUse",
            Event::PostToolUseFailure => "PostToolUseFailure",
            Event::UserPromptSubmit => "UserPromptSubmit",
            Event::Stop => "Stop",
            Event::SubagentStart => "SubagentStart",
            Event::SubagentStop => "SubagentStop",
            Event::PreCompact => "PreCompact",
            Event::PostCompact => "PostCompact",
            Event::SessionStart => "SessionStart",
            Event::SessionEnd => "SessionEnd",
            Event::Notification => "Notification",
            Event::PermissionRequest => "PermissionRequest",
        }
    }

    /// Whether plain text a hook prints goes to the model; for the other
    /// events only JSON output counts.
    fn text_is_context(self) -> bool {
        matches!(self, Event::SessionStart | Event::SubagentStart | Event::UserPromptSubmit)
    }

    /// Whether groups are filtered by their `matcher`.
    fn uses_matcher(self) -> bool {
        !matches!(self, Event::UserPromptSubmit | Event::Stop)
    }
}

/// The `hooks` section of the settings: each event's matcher groups.
pub type Config = BTreeMap<Event, Vec<Group>>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    #[serde(default)]
    matcher: Matcher,
    hooks: Vec<Handler>,
}

#[derive(Debug, Deserialize)]
#[serde(try_from = "RawHandler")]
struct Handler {
    action: Action,
    /// `if`: the tool calls it runs for, past the group's matcher.
    condition: Option<Condition>,
    mode: Mode,
    /// Seconds.
    timeout: Option<u64>,
    /// `onFailure: "block"`: a hook that fails blocks, rather than let
    /// things go on as if it had not run.
    block_on_failure: bool,
    /// Set for a plugin's hooks: `CLAUDE_PLUGIN_ROOT` and the like.
    env: Vec<(String, String)>,
}

/// `onFailure` in a hook's settings: `continue` (the default) or `block`.
fn block_on_failure(value: Option<&str>, action: &Action) -> Result<bool, String> {
    match (value, action) {
        (None | Some("continue"), _) => Ok(false),
        (Some("block"), Action::Command { .. }) => Ok(true),
        (Some("block"), Action::Prompt { .. }) => Err("`onFailure` is for command hooks".into()),
        (Some(other), _) => Err(format!("`onFailure` is `continue` or `block`, not `{other}`")),
    }
}

/// Whether the agent waits for a hook.
#[derive(Debug, Clone, PartialEq)]
enum Mode {
    /// It waits, and the hook's answer counts.
    Wait,
    /// `async`: the hook runs in the background; what it has for the model
    /// goes with the next request.
    Background,
    /// `asyncRewake`: the same, and exit code 2 wakes the model with
    /// `rewakeMessage` and the hook's reason.
    Rewake(Option<String>),
}

impl Mode {
    fn new(background: bool, rewake: bool, message: Option<String>) -> Self {
        match (background, rewake) {
            (_, true) => Mode::Rewake(message),
            (true, false) => Mode::Background,
            (false, false) => Mode::Wait,
        }
    }
}

/// A handler as the settings give it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHandler {
    #[serde(rename = "type")]
    kind: String,
    command: Option<String>,
    args: Option<Vec<String>>,
    prompt: Option<String>,
    model: Option<String>,
    timeout: Option<u64>,
    #[serde(rename = "if")]
    condition: Option<String>,
    #[serde(default, rename = "async")]
    background: bool,
    #[serde(default, rename = "asyncRewake")]
    rewake: bool,
    #[serde(rename = "rewakeMessage")]
    rewake_message: Option<String>,
    #[serde(rename = "onFailure")]
    on_failure: Option<String>,
    /// Claude Code's line for its UI; unused.
    #[serde(rename = "rewakeSummary")]
    _rewake_summary: Option<String>,
}

/// An `if` rule, in Claude Code's permission syntax: `Bash(git commit:*)`,
/// `Edit(src/**/*.ts)`, or a tool's name alone.
#[derive(Debug, Clone)]
pub(crate) struct Condition {
    tool: String,
    /// What the command (for `Bash`) or the file path (for the file tools)
    /// must match.
    pattern: Option<Regex>,
}

impl Condition {
    pub(crate) fn parse(rule: &str) -> Result<Self, String> {
        let rule = rule.trim();
        let (tool, pattern) = match rule.split_once('(') {
            None => (rule, None),
            Some((tool, rest)) => match rest.strip_suffix(')') {
                Some(pattern) => (tool.trim(), Some(pattern.trim())),
                None => return Err(format!("`if` rule `{rule}` lacks its closing parenthesis")),
            },
        };
        if tool.is_empty() || !tool.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("`if` rule `{rule}` names no tool"));
        }
        let is_command = tool_named(tool) == Some("bash");
        let pattern = pattern.filter(|p| !p.is_empty() && *p != "*").map(|p| rule_regex(p, is_command)).transpose()?;
        Ok(Self { tool: tool.to_string(), pattern })
    }

    /// Whether the call of the tool named `names` (with its aliases) with
    /// `input` (as hooks see it) is one the rule is for.
    pub(crate) fn matches(&self, names: &[&str], input: &Value) -> bool {
        if !names.iter().any(|n| *n == self.tool) {
            return false;
        }
        let Some(pattern) = &self.pattern else { return true };
        match names.first().copied() {
            // Each part of a compound command counts.
            Some("bash") => input["command"]
                .as_str()
                .unwrap_or_default()
                .split(['\n', ';', '|', '&'])
                .map(str::trim)
                .any(|part| !part.is_empty() && pattern.is_match(part)),
            Some("read" | "edit" | "write" | "grep" | "glob") => {
                let Some(path) = input["file_path"].as_str().or(input["path"].as_str()) else { return false };
                let cwd = std::env::current_dir().unwrap_or_default();
                let relative = std::path::Path::new(path).strip_prefix(&cwd).map(|p| p.display().to_string());
                pattern.is_match(path) || relative.is_ok_and(|r| pattern.is_match(&r))
            }
            _ => false,
        }
    }
}

/// A rule's pattern as a regular expression for a whole command (`*` is
/// anything; `prefix:*` and a trailing ` *` take any arguments, or none) or
/// a whole path (`*` stays within a directory, `**` does not).
fn rule_regex(pattern: &str, command: bool) -> Result<Regex, String> {
    let (body, args) = match pattern.strip_suffix(":*").or_else(|| pattern.strip_suffix(" *")) {
        Some(body) if command => (body, r"(\s.*)?"),
        _ => (pattern, ""),
    };
    let mut regex = String::from("^");
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if command => regex.push_str(".*"),
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                regex.push_str(".*");
            }
            '*' => regex.push_str("[^/]*"),
            c => regex.push_str(&regex::escape(&c.to_string())),
        }
    }
    regex.push_str(args);
    regex.push('$');
    Regex::new(&regex).map_err(|e| format!("`if` pattern `{pattern}`: {e}"))
}

#[derive(Debug, Clone, PartialEq)]
enum Action {
    /// `sh -c command`, or with `args`, `command` started with them
    /// directly.
    Command { command: String, args: Option<Vec<String>> },
    /// A question for the model (`model`, else the conversation's), with
    /// the event's input in place of `$ARGUMENTS`.
    Prompt { prompt: String, model: Option<String> },
}

impl TryFrom<RawHandler> for Handler {
    type Error = String;

    fn try_from(raw: RawHandler) -> Result<Self, String> {
        let action = Action::new(&raw.kind, raw.command, raw.args, raw.prompt, raw.model)?;
        let condition = raw.condition.as_deref().map(Condition::parse).transpose()?;
        let mode = Mode::new(raw.background, raw.rewake, raw.rewake_message);
        let block_on_failure = block_on_failure(raw.on_failure.as_deref(), &action)?;
        Ok(Handler { action, condition, mode, timeout: raw.timeout, block_on_failure, env: Vec::new() })
    }
}

impl Action {
    fn new(
        kind: &str,
        command: Option<String>,
        args: Option<Vec<String>>,
        prompt: Option<String>,
        model: Option<String>,
    ) -> Result<Self, String> {
        match kind {
            "command" if prompt.is_some() || model.is_some() => Err("`prompt` and `model` are for prompt hooks".into()),
            "command" => Ok(Action::Command { command: command.ok_or("a command hook needs `command`")?, args }),
            "prompt" if command.is_some() || args.is_some() => Err("`command` and `args` are for command hooks".into()),
            "prompt" => Ok(Action::Prompt { prompt: prompt.ok_or("a prompt hook needs `prompt`")?, model }),
            other => Err(format!("hook type `{other}` is not supported (only `command` and `prompt`)")),
        }
    }

    /// The hook, for messages.
    fn label(&self) -> String {
        match self {
            Action::Command { command, args: None } => command.clone(),
            Action::Command { command, args: Some(args) } => format!("{command} {}", args.join(" ")),
            Action::Prompt { prompt, .. } => format!("prompt: {prompt}"),
        }
    }
}

/// Which tools (or session sources, agent types) a group applies to: all
/// for `""`, `"*"` or none; a list of exact names for `"Edit|Write"`;
/// otherwise a regular expression, found anywhere in the name.
#[derive(Debug, Default, Deserialize)]
#[serde(try_from = "String")]
enum Matcher {
    #[default]
    All,
    Names(Vec<String>),
    Pattern(Regex),
}

impl TryFrom<String> for Matcher {
    type Error = regex::Error;

    fn try_from(matcher: String) -> Result<Self, regex::Error> {
        let matcher = matcher.trim();
        if matcher.is_empty() || matcher == "*" {
            return Ok(Matcher::All);
        }
        if matcher.chars().all(|c| c.is_ascii_alphanumeric() || "_-|, ".contains(c)) {
            let names = matcher.split(['|', ',']).map(str::trim).filter(|n| !n.is_empty());
            return Ok(Matcher::Names(names.map(String::from).collect()));
        }
        Regex::new(matcher).map(Matcher::Pattern)
    }
}

impl Matcher {
    /// Whether any of `names` (a tool's own name and its aliases) matches.
    fn matches(&self, names: &[&str]) -> bool {
        match self {
            Matcher::All => true,
            Matcher::Names(list) => names.iter().any(|n| list.iter().any(|m| m == n)),
            Matcher::Pattern(re) => names.iter().any(|n| re.is_match(n)),
        }
    }
}

/// Claude Code's names for the built-in tools; the first is the one hooks
/// see in `tool_name`.
pub(crate) const ALIASES: &[(&str, &[&str])] = &[
    ("agent", &["Agent", "Task"]),
    ("bash", &["Bash"]),
    ("bash_output", &["BashOutput"]),
    ("edit", &["Edit"]),
    // `LS` lists a directory; agents that list it get `glob` for that.
    ("glob", &["Glob", "LS"]),
    ("grep", &["Grep"]),
    ("kill_shell", &["KillShell"]),
    ("lsp", &["LSP"]),
    ("message_agent", &["SendMessage"]),
    ("read", &["Read"]),
    ("skill", &["Skill"]),
    ("stop_agent", &["TaskStop"]),
    ("tool_search", &["ToolSearch"]),
    ("write", &["Write"]),
];

/// Tools whose `path` is Claude Code's `file_path`.
const FILE_TOOLS: &[&str] = &["read", "edit", "write"];

/// The tool Claude Code calls `alias` (`Bash` is `bash`).
pub fn tool_named(alias: &str) -> Option<&'static str> {
    ALIASES.iter().find(|(_, aliases)| aliases.contains(&alias)).map(|(name, _)| *name)
}

fn aliases(tool: &str) -> &'static [&'static str] {
    ALIASES.iter().find(|(name, _)| *name == tool).map_or(&[], |(_, aliases)| aliases)
}

/// The tool's name and aliases, for matching.
pub(crate) fn tool_names(tool: &str) -> Vec<&str> {
    std::iter::once(tool).chain(aliases(tool).iter().copied()).collect()
}

/// The tool's name as hooks see it.
pub(crate) fn hook_tool_name(tool: &str) -> &str {
    aliases(tool).first().copied().unwrap_or(tool)
}

/// The tool input as hooks see it: the file tools' `path` also as
/// `file_path`.
pub(crate) fn input_for_hook(tool: &str, input: &Value) -> Value {
    let mut input = input.clone();
    if FILE_TOOLS.contains(&tool)
        && let Some(object) = input.as_object_mut()
        && let Some(path) = object.get("path").cloned()
    {
        object.insert("file_path".into(), path);
    }
    input
}

/// An `updatedInput` from a hook as the tool takes it: `file_path` becomes
/// `path` again.
fn input_from_hook(tool: &str, mut input: Value) -> Value {
    if FILE_TOOLS.contains(&tool)
        && let Some(object) = input.as_object_mut()
        && let Some(path) = object.remove("file_path")
    {
        object.insert("path".into(), path);
    }
    input
}

/// Hook output for the model, marked as such.
pub fn feedback(event: Event, text: &str) -> String {
    format!("<hook-feedback event=\"{}\">\n{text}\n</hook-feedback>", event.as_str())
}

/// A plugin's hooks, from its `hooks/hooks.json` or manifest: the event map
/// `events` with the plugin's variables (`${CLAUDE_PLUGIN_ROOT}`,
/// `${user_config.<key>}` and the like) replaced in the commands and
/// prompts, and set in the commands' environment. What the harness cannot
/// run (another event, another type of hook) is left out with a warning
/// rather than failing the plugin, which is not the user's to fix; fields
/// it does not know are ignored.
pub fn plugin_config(events: &Value, plugin: &PluginDirs, warnings: &mut Vec<String>) -> Config {
    let mut config = Config::new();
    let Some(events) = events.as_object() else {
        warnings.push("hooks: expected an object of events".into());
        return config;
    };
    for (name, groups) in events {
        let Ok(event) = serde_json::from_value::<Event>(Value::String(name.clone())) else {
            warnings.push(format!("hook event `{name}` is not supported; its hooks are left out"));
            continue;
        };
        for group in groups.as_array().into_iter().flatten() {
            let matcher = match Matcher::try_from(group["matcher"].as_str().unwrap_or_default().to_string()) {
                Ok(matcher) => matcher,
                Err(e) => {
                    warnings.push(format!("{name} hook matcher: {e}"));
                    continue;
                }
            };
            let mut hooks = Vec::new();
            for handler in group["hooks"].as_array().into_iter().flatten() {
                let text = |key: &str| handler[key].as_str().map(String::from);
                let args = handler["args"].as_array().map(|args| {
                    args.iter().map(|a| plugin.expand(a.as_str().map_or(&a.to_string(), |a| a))).collect()
                });
                let kind = handler["type"].as_str().unwrap_or_default();
                let action = match Action::new(kind, text("command"), args, text("prompt"), text("model")) {
                    Ok(Action::Command { command, args }) => Action::Command { command: plugin.expand(&command), args },
                    Ok(Action::Prompt { prompt, model }) => Action::Prompt { prompt: plugin.expand_prompt(&prompt), model },
                    Err(e) => {
                        warnings.push(format!("{name} hook: {e}; left out"));
                        continue;
                    }
                };
                // A hook whose `if` cannot be read would run for more than it
                // should; it is left out.
                let condition = match text("if").as_deref().map(Condition::parse).transpose() {
                    Ok(condition) => condition,
                    Err(e) => {
                        warnings.push(format!("{name} hook: {e}; left out"));
                        continue;
                    }
                };
                let mode = Mode::new(handler["async"] == true, handler["asyncRewake"] == true, text("rewakeMessage"));
                let block_on_failure = match block_on_failure(text("onFailure").as_deref(), &action) {
                    Ok(block) => block,
                    Err(e) => {
                        warnings.push(format!("{name} hook: {e}; left out"));
                        continue;
                    }
                };
                let timeout = handler["timeout"].as_u64();
                hooks.push(Handler { action, condition, mode, timeout, block_on_failure, env: plugin.env() });
            }
            if !hooks.is_empty() {
                config.entry(event).or_default().push(Group { matcher, hooks });
            }
        }
    }
    config
}

static CONFIG: RwLock<Option<Arc<Config>>> = RwLock::new(None);

/// Sets the hooks of every conversation started from here on, from the
/// settings and plugins.
pub fn set(config: Arc<Config>) {
    *CONFIG.write().unwrap() = Some(config);
}

struct Session {
    id: String,
    transcript: Option<PathBuf>,
}

/// The hooks one conversation runs, and what they are told about it.
/// Subagents share their parent's session.
#[derive(Clone)]
pub struct Hooks {
    config: Arc<Config>,
    session: Arc<Mutex<Session>>,
    /// The subagent's id and type, for a subagent's hooks.
    subagent: Option<(String, String)>,
    /// The conversation's model, which prompt hooks ask.
    client: Option<Client>,
    /// The conversation's inbox, where background hooks' answers go.
    inbox: Option<Arc<Inbox>>,
    /// The conversation's permissions, whose mode hooks are told.
    permissions: Option<Arc<crate::security::permissions::Permissions>>,
}

impl Default for Hooks {
    /// The hooks from the settings.
    fn default() -> Self {
        Self::with_config(CONFIG.read().unwrap().clone().unwrap_or_default())
    }
}

/// What the hooks of one event decided, all together.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Why the action is blocked: exit code 2's stderr, `decision: "block"`
    /// or a PreToolUse `deny`.
    pub block: Option<String>,
    /// `continue: false`: the agent stops, with this `stopReason`.
    pub halt: Option<String>,
    /// Text for the model: `additionalContext`, or plain output where that
    /// counts.
    pub context: Vec<String>,
    /// For the user: `systemMessage`s and hooks that failed.
    pub notices: Vec<String>,
    /// A PreToolUse hook's `updatedInput`.
    pub updated_input: Option<Value>,
    /// A PostToolUse hook's `updatedToolOutput`.
    pub updated_output: Option<String>,
    /// A PreToolUse hook's `permissionDecision` of `allow` or `ask`, or a
    /// PermissionRequest hook's `allow` (its `deny` blocks).
    pub permission: Option<crate::security::permissions::HookDecision>,
}

/// How a hook process ended.
struct Reply {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Hooks {
    pub fn with_config(config: Arc<Config>) -> Self {
        let session = Session { id: crate::session::new_id(), transcript: None };
        Self { config, session: Arc::new(Mutex::new(session)), subagent: None, client: None, inbox: None, permissions: None }
    }

    /// Tells hooks the permission mode of `permissions`, the conversation's.
    pub fn set_permissions(&mut self, permissions: Arc<crate::security::permissions::Permissions>) {
        self.permissions = Some(permissions);
    }

    /// Has prompt hooks ask `client`'s model.
    pub fn set_client(&mut self, client: Client) {
        self.client = Some(client);
    }

    /// Sends what background hooks have for the model to `inbox`.
    pub fn set_inbox(&mut self, inbox: Arc<Inbox>) {
        self.inbox = Some(inbox);
    }

    /// Tells the hooks which saved conversation they run for.
    pub fn set_session(&self, id: &str, transcript: Option<PathBuf>) {
        *self.session.lock().unwrap() = Session { id: id.to_string(), transcript };
    }

    /// The hooks of subagent `id` of type `agent_type`, in this session.
    pub fn for_subagent(&self, id: &str, agent_type: &str) -> Self {
        Self { subagent: Some((id.to_string(), agent_type.to_string())), ..self.clone() }
    }

    pub fn is_subagent(&self) -> bool {
        self.subagent.is_some()
    }

    /// The subagent these hooks are for, as `type agent_id`.
    pub fn subagent_label(&self) -> Option<String> {
        self.subagent.as_ref().map(|(id, agent_type)| format!("{agent_type} {id}"))
    }

    pub fn permissions(&self) -> Option<&Arc<crate::security::permissions::Permissions>> {
        self.permissions.as_ref()
    }

    /// `Stop`, or `SubagentStop` for a subagent.
    pub fn stop_event(&self) -> Event {
        if self.is_subagent() { Event::SubagentStop } else { Event::Stop }
    }

    pub async fn pre_tool_use(&self, tool: &str, id: &str, input: &Value, model: &str) -> Outcome {
        let fields = json!({
            "tool_name": hook_tool_name(tool),
            "tool_use_id": id,
            "tool_input": input_for_hook(tool, input),
        });
        let mut outcome = self.run(Event::PreToolUse, &tool_names(tool), fields, model).await;
        outcome.updated_input = outcome.updated_input.map(|input| input_from_hook(tool, input));
        outcome
    }

    /// `PermissionRequest` before the user is asked about a call: a hook
    /// may allow it (`permission`) or deny it (`block`) instead.
    pub async fn permission_request(&self, tool: &str, input: &Value, suggestion: &str, model: &str) -> Outcome {
        let fields = json!({
            "tool_name": hook_tool_name(tool),
            "tool_input": input_for_hook(tool, input),
            "permission_suggestions": [{"type": "addRules", "rules": [suggestion], "behavior": "allow", "destination": "session"}],
        });
        self.run(Event::PermissionRequest, &tool_names(tool), fields, model).await
    }

    /// `PostToolUse` after a call that succeeded, `PostToolUseFailure`
    /// after one that failed.
    pub async fn post_tool_use(
        &self,
        tool: &str,
        id: &str,
        input: &Value,
        output: &ToolOutput,
        model: &str,
    ) -> Outcome {
        let (event, key) =
            if output.is_error { (Event::PostToolUseFailure, "error") } else { (Event::PostToolUse, "tool_response") };
        let fields = json!({
            "tool_name": hook_tool_name(tool),
            "tool_use_id": id,
            "tool_input": input_for_hook(tool, input),
            key: output.content,
        });
        self.run(event, &tool_names(tool), fields, model).await
    }

    pub async fn user_prompt_submit(&self, prompt: &str, model: &str) -> Outcome {
        self.run(Event::UserPromptSubmit, &[], json!({"prompt": prompt}), model).await
    }

    /// The model ended its turn; `active` says whether a Stop hook already
    /// made it go on in this turn.
    pub async fn stop(&self, active: bool, last_message: &str, model: &str) -> Outcome {
        let mut fields = json!({"stop_hook_active": active, "last_assistant_message": last_message});
        let types = match &self.subagent {
            Some((_, agent_type)) => {
                fields["agent_transcript_path"] = Value::Null;
                vec![agent_type.as_str()]
            }
            None => Vec::new(),
        };
        self.run(self.stop_event(), &types, fields, model).await
    }

    /// A conversation starts: `startup`, `clear`, `resume` or `compact`.
    pub async fn session_start(&self, source: &str, model: &str) -> Outcome {
        self.run(Event::SessionStart, &[source], json!({"source": source}), model).await
    }

    /// A conversation ends: `clear`, `resume`, `prompt_input_exit` or
    /// `other`.
    pub async fn session_end(&self, reason: &str, model: &str) -> Outcome {
        self.run(Event::SessionEnd, &[reason], json!({"reason": reason}), model).await
    }

    /// Something for the user to notice, such as the prompt waiting for
    /// them (`idle_prompt`).
    pub async fn notification(&self, kind: &str, message: &str, model: &str) -> Outcome {
        let fields = json!({"notification_type": kind, "message": message, "title": "harness"});
        self.run(Event::Notification, &[kind], fields, model).await
    }

    /// Whether any hook runs for `event`.
    pub fn has(&self, event: Event) -> bool {
        self.config.get(&event).is_some_and(|groups| !groups.is_empty())
    }

    /// A subagent starts its first run.
    pub async fn subagent_start(&self, model: &str) -> Outcome {
        let agent_type = self.subagent.as_ref().map_or("", |(_, t)| t.as_str());
        self.run(Event::SubagentStart, &[agent_type], json!({}), model).await
    }

    /// The conversation is about to be compacted, `manual`ly (`/compact`,
    /// with `instructions` saying what to focus on) or `auto`matically.
    pub async fn pre_compact(&self, trigger: &str, instructions: &str, model: &str) -> Outcome {
        let fields = json!({"trigger": trigger, "custom_instructions": instructions});
        self.run(Event::PreCompact, &[trigger], fields, model).await
    }

    /// The conversation was compacted into `summary`.
    pub async fn post_compact(&self, trigger: &str, summary: &str, model: &str) -> Outcome {
        let fields = json!({"trigger": trigger, "compact_summary": summary});
        self.run(Event::PostCompact, &[trigger], fields, model).await
    }

    /// Runs the hooks of `event` whose matcher matches one of `names`, all
    /// at once (a command listed twice runs once), with the common fields
    /// and `fields` as input.
    async fn run(&self, event: Event, names: &[&str], fields: Value, model: &str) -> Outcome {
        let mut handlers: Vec<&Handler> = Vec::new();
        let groups = self.config.get(&event).map_or(&[][..], Vec::as_slice);
        // `if` rules only apply to tool calls.
        let tool_input = fields.get("tool_input");
        for group in groups.iter().filter(|g| !event.uses_matcher() || g.matcher.matches(names)) {
            for handler in &group.hooks {
                let applies = match (&handler.condition, tool_input) {
                    (Some(condition), Some(input)) => condition.matches(names, input),
                    _ => true,
                };
                if applies && !handlers.iter().any(|h| h.action == handler.action) {
                    handlers.push(handler);
                }
            }
        }
        let mut outcome = Outcome::default();
        if handlers.is_empty() {
            return outcome;
        }
        let input = self.input(event, fields, model).to_string();
        // Background hooks report on their own.
        let (background, handlers): (Vec<&Handler>, Vec<&Handler>) =
            handlers.into_iter().partition(|h| h.mode != Mode::Wait);
        for handler in background {
            self.run_in_background(event, handler, input.clone());
        }
        let calls: Vec<_> = handlers.iter().map(|h| self.call(event, h, &input)).collect();
        let replies: Vec<_> = futures::stream::iter(calls).buffered(MAX_CONCURRENT).collect().await;
        for (handler, reply) in handlers.iter().zip(replies) {
            outcome.add(event, handler, reply);
        }
        outcome
    }

    /// Runs one hook with `input` and returns its answer.
    async fn call(&self, event: Event, h: &Handler, input: &str) -> Result<Reply, String> {
        let default = match &h.action {
            Action::Prompt { .. } => PROMPT_TIMEOUT,
            Action::Command { .. } => DEFAULT_TIMEOUT,
        };
        let timeout = match event {
            Event::SessionEnd => h.timeout.map_or(SESSION_END_TIMEOUT, Duration::from_secs).min(MAX_SESSION_END_TIMEOUT),
            _ => h.timeout.map_or(default, Duration::from_secs),
        };
        match &h.action {
            Action::Command { command, args } => run_command(command, args.as_deref(), &h.env, input, timeout).await,
            Action::Prompt { prompt, model } => match &self.client {
                Some(client) => {
                    let asked = ask_model(client, prompt, model.as_deref(), input);
                    let timed_out = || format!("timed out after {:.1}s", timeout.as_secs_f64());
                    tokio::time::timeout(timeout, asked).await.unwrap_or_else(|_| Err(timed_out()))
                }
                None => Err("there is no model to ask here".into()),
            },
        }
    }

    /// Runs `handler` without waiting for it. When it ends, what it has for
    /// the model goes to the inbox: with the next request, or, for an
    /// `asyncRewake` hook that blocked (exit code 2), in a turn of its own.
    fn run_in_background(&self, event: Event, handler: &Handler, input: String) {
        let hooks = self.clone();
        let handler = Handler {
            action: handler.action.clone(),
            condition: None,
            mode: handler.mode.clone(),
            timeout: handler.timeout,
            block_on_failure: handler.block_on_failure,
            env: handler.env.clone(),
        };
        tokio::spawn(async move {
            let reply = hooks.call(event, &handler, &input).await;
            let mut outcome = Outcome::default();
            outcome.add(event, &handler, reply);
            let Some(inbox) = &hooks.inbox else { return };
            let mut texts: Vec<String> = outcome.context;
            let wake = match (&handler.mode, outcome.block) {
                (Mode::Rewake(message), Some(reason)) => {
                    texts.push(match message {
                        Some(message) => format!("{message}\n\n{reason}"),
                        None => reason,
                    });
                    true
                }
                (_, Some(reason)) => {
                    texts.push(reason);
                    false
                }
                _ => false,
            };
            for text in texts {
                inbox.push(feedback(event, &text), wake);
            }
        });
    }

    fn input(&self, event: Event, fields: Value, model: &str) -> Value {
        let session = self.session.lock().unwrap();
        let cwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
        let mut input = json!({
            "session_id": session.id,
            "transcript_path": session.transcript.as_ref().map(|p| p.display().to_string()),
            "cwd": cwd,
            "hook_event_name": event.as_str(),
            "permission_mode": self.permissions.as_ref().map_or("bypassPermissions", |p| p.mode().as_str()),
            "model": model,
        });
        if let Some((id, agent_type)) = &self.subagent {
            input["agent_id"] = json!(id);
            input["agent_type"] = json!(agent_type);
        }
        if let (Some(input), Value::Object(fields)) = (input.as_object_mut(), fields) {
            input.extend(fields);
        }
        input
    }
}

impl Outcome {
    /// Takes in what the hook `handler` answered.
    fn add(&mut self, event: Event, handler: &Handler, reply: Result<Reply, String>) {
        let name = format!("{} hook `{}`", event.as_str(), crate::util::ellipsize(&handler.action.label(), 60));
        let failed = |outcome: &mut Self, failure: String| {
            if handler.block_on_failure {
                outcome.block(&format!("{failure}, and it blocks when it fails"));
            }
            outcome.notices.push(failure);
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(e) => return failed(self, format!("{name} failed: {e}")),
        };
        match reply.code {
            Some(0) => {}
            Some(2) => {
                let reason = reply.stderr.trim();
                return self.block(if reason.is_empty() { "blocked by a hook" } else { reason });
            }
            code => {
                let status = code.map_or("killed by a signal".to_string(), |c| format!("exit code {c}"));
                let detail = reply.stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
                let detail = if detail.is_empty() { String::new() } else { format!(": {detail}") };
                return failed(self, format!("{name} failed ({status}){detail}"));
            }
        }
        let stdout = reply.stdout.trim();
        if stdout.is_empty() {
            return;
        }
        if !stdout.starts_with('{') {
            if event.text_is_context() {
                self.context.push(crate::tools::truncate(stdout, MAX_CONTEXT_BYTES));
            }
            return;
        }
        let output: Value = match serde_json::from_str(stdout) {
            Ok(output) => output,
            Err(e) => return failed(self, format!("{name} printed invalid JSON: {e}")),
        };
        if output["continue"] == false && self.halt.is_none() {
            self.halt = Some(output["stopReason"].as_str().unwrap_or_default().to_string());
        }
        if let Some(message) = output["systemMessage"].as_str() {
            self.notices.push(message.to_string());
        }
        let reason = output["reason"].as_str().filter(|r| !r.trim().is_empty());
        if output["decision"] == "block" {
            self.block(reason.unwrap_or("blocked by a hook"));
        }
        let specific = &output["hookSpecificOutput"];
        let why = specific["permissionDecisionReason"].as_str().filter(|r| !r.trim().is_empty()).or(reason);
        match specific["permissionDecision"].as_str() {
            Some("deny") => self.block(why.unwrap_or("denied by a hook")),
            Some("ask") => {
                let reason = format!("a {} hook asks for it: {}", event.as_str(), why.unwrap_or("it needs approval"));
                self.permission.get_or_insert(HookDecision::Ask(reason));
            }
            Some("allow") => {
                self.permission.get_or_insert(HookDecision::Allow);
            }
            _ => {}
        }
        // A PermissionRequest hook's answer.
        let decision = &specific["decision"];
        match decision["behavior"].as_str() {
            Some("allow") => {
                self.permission.get_or_insert(HookDecision::Allow);
            }
            Some("deny") => self.block(decision["message"].as_str().filter(|m| !m.trim().is_empty()).unwrap_or("denied by a hook")),
            _ => {}
        }
        if let Some(context) = specific["additionalContext"].as_str().filter(|c| !c.trim().is_empty()) {
            self.context.push(crate::tools::truncate(context, MAX_CONTEXT_BYTES));
        }
        if let Some(input) = specific.get("updatedInput").filter(|i| i.is_object())
            && self.updated_input.is_none()
        {
            self.updated_input = Some(input.clone());
        }
        if let Some(output) = specific["updatedToolOutput"].as_str()
            && self.updated_output.is_none()
        {
            self.updated_output = Some(output.to_string());
        }
    }

    fn block(&mut self, reason: &str) {
        match &mut self.block {
            Some(block) => {
                block.push('\n');
                block.push_str(reason);
            }
            None => self.block = Some(reason.to_string()),
        }
    }
}

/// Asks `client`'s model (or `model`, if it resolves) whether to go ahead,
/// with the hook's `input` in place of `$ARGUMENTS` in `prompt`; its answer
/// as a command hook's: `ok: false` as exit code 2, other JSON as output.
async fn ask_model(client: &Client, prompt: &str, model: Option<&str>, input: &str) -> Result<Reply, String> {
    let client = model.and_then(|m| client.switch(m).ok()).unwrap_or_else(|| client.clone());
    let text = match prompt.contains("$ARGUMENTS") {
        true => prompt.replace("$ARGUMENTS", input),
        false => format!("{prompt}\n\nThe input of the event:\n{input}"),
    };
    let messages = [Message::user(vec![json!({"type": "text", "text": text})])];
    let response = client.create(PROMPT_SYSTEM, &[], &messages, &mut |_| {}).await.map_err(|e| format!("{e:#}"))?;
    let answer: String = response.content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
    let json = match (answer.find('{'), answer.rfind('}')) {
        (Some(start), Some(end)) if start < end => serde_json::from_str::<Value>(&answer[start..=end]).ok(),
        _ => None,
    };
    let reply = |code, stdout: String, stderr: &str| Ok(Reply { code: Some(code), stdout, stderr: stderr.to_string() });
    match json {
        Some(json) if json["ok"] == true => reply(0, String::new(), ""),
        Some(json) if json["ok"] == false => reply(2, String::new(), json["reason"].as_str().unwrap_or_default()),
        Some(json) if json.is_object() => reply(0, json.to_string(), ""),
        _ => Err(format!("the model answered without the JSON asked for: {}", crate::util::ellipsize(answer.trim(), 200))),
    }
}

/// Runs `sh -c command`, or with `args` `command` itself, with `input` on
/// stdin, in a process group of its own that is killed if it times out or
/// the call is dropped.
async fn run_command(
    command: &str,
    args: Option<&[String]>,
    env: &[(String, String)],
    input: &str,
    timeout: Duration,
) -> Result<Reply, String> {
    let mut cmd = match args {
        Some(args) => {
            let mut cmd = Command::new(command);
            cmd.args(args);
            cmd
        }
        None => {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(command);
            cmd
        }
    };
    cmd.process_group(0).envs(env.iter().map(|(k, v)| (k, v)));
    if let Ok(dir) = std::env::current_dir() {
        cmd.env("CLAUDE_PROJECT_DIR", &dir).env("HARNESS_PROJECT_DIR", &dir);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot start it: {e}"))?;
    let mut group = KillGroup(child.id());
    let stdin = child.stdin.take();
    let write = async move {
        // A hook that doesn't read its input closes the pipe early.
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(input.as_bytes()).await;
        }
    };
    let output = match tokio::time::timeout(timeout, async { tokio::join!(write, child.wait_with_output()).1 }).await {
        Err(_) => return Err(format!("timed out after {:.1}s", timeout.as_secs_f64())),
        Ok(Err(e)) => return Err(e.to_string()),
        Ok(Ok(output)) => output,
    };
    // It exited; what it left running in the background may go on.
    group.0 = None;
    Ok(Reply {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Hooks from a JSON `hooks` section. Tests that run them hold
    /// [`crate::util::lock_cwd`]: the hook processes inherit the working
    /// directory, which a test elsewhere moves into a temporary directory
    /// and deletes, making `sh` complain on stderr.
    pub(crate) fn hooks(config: Value) -> Hooks {
        Hooks::with_config(Arc::new(serde_json::from_value(config).unwrap()))
    }

    fn command(event: &str, matcher: &str, command: &str) -> Hooks {
        hooks(json!({event: [{"matcher": matcher, "hooks": [{"type": "command", "command": command}]}]}))
    }

    #[test]
    fn parses_the_config_strictly() {
        let config: Config = serde_json::from_value(json!({
            "PreToolUse": [{"matcher": "Edit|Write", "hooks": [{"type": "command", "command": "x", "timeout": 5}]}],
            "Stop": [{"hooks": [{"type": "command", "command": "y"}]}]
        }))
        .unwrap();
        assert_eq!(config.len(), 2);
        let bad = [
            json!({"PreCompact": [{"hooks": [{"type": "command", "command": "y", "statusMessage": "z"}]}]}),
            json!({"Stop": [{"hooks": [{"type": "prompt", "command": "y"}]}]}),
            json!({"Stop": [{"hooks": [{"type": "http", "url": "y"}]}]}),
            json!({"Stop": [{"hooks": [{"type": "command", "command": "y", "once": true}]}]}),
            json!({"PreToolUse": [{"hooks": [{"type": "command", "command": "y", "if": "Bash(git"}]}]}),
            json!({"PreToolUse": [{"matcher": "(", "hooks": []}]}),
            json!({"Stop": [{"hooks": [{"type": "command", "command": "y", "onFailure": "stop"}]}]}),
            json!({"Stop": [{"hooks": [{"type": "prompt", "prompt": "y", "onFailure": "block"}]}]}),
        ];
        for config in bad {
            assert!(serde_json::from_value::<Config>(config.clone()).is_err(), "{config}");
        }
    }

    #[test]
    fn matchers() {
        let m = |s: &str| Matcher::try_from(s.to_string()).unwrap();
        assert!(m("").matches(&["bash"]) && m("*").matches(&["bash"]));
        assert!(m("Bash").matches(&tool_names("bash")));
        assert!(m("bash").matches(&tool_names("bash")));
        assert!(m("Edit|Write").matches(&tool_names("write")));
        assert!(m("Edit, Write").matches(&tool_names("edit")));
        assert!(!m("Edit|Write").matches(&tool_names("read")));
        assert!(!m("Edit").matches(&["NotebookEdit"]), "a plain name is matched exactly");
        assert!(m("mcp__fs__.*").matches(&tool_names("mcp__fs__read")));
        assert!(m("^Ta").matches(&tool_names("agent")));
        assert!(!m("^Bash$").matches(&tool_names("bash_output")));
    }

    #[test]
    fn file_tools_get_file_path() {
        let input = input_for_hook("edit", &json!({"path": "a.rs", "old_string": "x"}));
        assert_eq!(input, json!({"path": "a.rs", "file_path": "a.rs", "old_string": "x"}));
        assert_eq!(input_for_hook("bash", &json!({"path": "a"})), json!({"path": "a"}));
        let back = input_from_hook("write", json!({"path": "a.rs", "file_path": "b.rs", "content": ""}));
        assert_eq!(back, json!({"path": "b.rs", "content": ""}));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn hooks_get_the_event_on_stdin() {
        let _guard = crate::util::lock_cwd();
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("seen.json");
        let h = command("PreToolUse", "Write", &format!("cat > {}", seen.display()));
        h.set_session("s1", Some(PathBuf::from("/t/s1.jsonl")));
        let outcome = h.pre_tool_use("write", "toolu_1", &json!({"path": "a.rs", "content": "x"}), "gw/m").await;
        assert!(outcome.block.is_none() && outcome.notices.is_empty(), "{outcome:?}");
        let input: Value = serde_json::from_str(&std::fs::read_to_string(&seen).unwrap()).unwrap();
        assert_eq!(input["hook_event_name"], "PreToolUse");
        assert_eq!(input["session_id"], "s1");
        assert_eq!(input["transcript_path"], "/t/s1.jsonl");
        assert_eq!(input["tool_name"], "Write");
        assert_eq!(input["tool_use_id"], "toolu_1");
        assert_eq!(input["tool_input"]["file_path"], "a.rs");
        assert_eq!(input["model"], "gw/m");
        assert!(input.get("agent_id").is_none());

        // Other tools don't run it.
        std::fs::remove_file(&seen).unwrap();
        h.pre_tool_use("read", "toolu_2", &json!({"path": "a.rs"}), "gw/m").await;
        assert!(!seen.exists());

        let sub = h.for_subagent("a1", "explore");
        sub.pre_tool_use("write", "toolu_3", &json!({"path": "a.rs", "content": ""}), "gw/m").await;
        let input: Value = serde_json::from_str(&std::fs::read_to_string(&seen).unwrap()).unwrap();
        assert_eq!((input["agent_id"].as_str(), input["agent_type"].as_str()), (Some("a1"), Some("explore")));
        assert_eq!(input["session_id"], "s1", "subagents share the session");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn exit_code_2_blocks_with_stderr() {
        let _guard = crate::util::lock_cwd();
        let h = command("PreToolUse", "Bash", "echo 'no rm -rf' >&2; exit 2");
        let outcome = h.pre_tool_use("bash", "t", &json!({"command": "rm -rf /"}), "m").await;
        assert_eq!(outcome.block.as_deref(), Some("no rm -rf"));

        let h = command("PreToolUse", "Bash", "echo oops >&2; exit 1");
        let outcome = h.pre_tool_use("bash", "t", &json!({"command": "ls"}), "m").await;
        assert!(outcome.block.is_none());
        assert_eq!(outcome.notices.len(), 1);
        assert!(outcome.notices[0].contains("(exit code 1): oops"), "{:?}", outcome.notices);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn json_output() {
        let _guard = crate::util::lock_cwd();
        let deny = r#"echo '{"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": "not here"}}'"#;
        let outcome = command("PreToolUse", "*", deny).pre_tool_use("bash", "t", &json!({}), "m").await;
        assert_eq!(outcome.block.as_deref(), Some("not here"));

        let rewrite = r#"echo '{"hookSpecificOutput": {"permissionDecision": "allow", "updatedInput": {"file_path": "b.rs"}, "additionalContext": "moved"}, "systemMessage": "hi"}'"#;
        let outcome = command("PreToolUse", "*", rewrite).pre_tool_use("read", "t", &json!({"path": "a.rs"}), "m").await;
        assert!(outcome.block.is_none());
        assert_eq!(outcome.updated_input, Some(json!({"path": "b.rs"})));
        assert_eq!(outcome.context, ["moved"]);
        assert_eq!(outcome.notices, ["hi"]);

        let halt = r#"echo '{"continue": false, "stopReason": "enough"}'"#;
        let outcome = command("Stop", "", halt).stop(false, "done", "m").await;
        assert_eq!(outcome.halt.as_deref(), Some("enough"));

        let outcome = command("PostToolUse", "*", "echo '{not json'").post_tool_use("bash", "t", &json!({}), &ToolOutput::ok("x"), "m").await;
        assert!(outcome.notices[0].contains("invalid JSON"), "{:?}", outcome.notices);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn plain_output_is_context_only_for_some_events() {
        let _guard = crate::util::lock_cwd();
        let outcome = command("SessionStart", "startup", "echo on main").session_start("startup", "m").await;
        assert_eq!(outcome.context, ["on main"]);
        let outcome = command("SessionStart", "resume", "echo on main").session_start("startup", "m").await;
        assert!(outcome.context.is_empty(), "the matcher picks the source");
        let outcome = command("PostToolUse", "*", "echo ignored").post_tool_use("bash", "t", &json!({}), &ToolOutput::ok("x"), "m").await;
        assert!(outcome.context.is_empty());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn failures_run_the_failure_event() {
        let _guard = crate::util::lock_cwd();
        let h = hooks(json!({
            "PostToolUse": [{"hooks": [{"type": "command", "command": "echo '{\"decision\": \"block\", \"reason\": \"ok\"}'"}]}],
            "PostToolUseFailure": [{"hooks": [{"type": "command", "command": "echo '{\"hookSpecificOutput\": {\"additionalContext\": \"failed\"}}'"}]}]
        }));
        let outcome = h.post_tool_use("bash", "t", &json!({}), &ToolOutput::ok("x"), "m").await;
        assert_eq!((outcome.block.as_deref(), outcome.context.len()), (Some("ok"), 0));
        let outcome = h.post_tool_use("bash", "t", &json!({}), &ToolOutput::error("x"), "m").await;
        assert_eq!((outcome.block, outcome.context), (None, vec!["failed".to_string()]));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn subagents_run_subagent_stop() {
        let _guard = crate::util::lock_cwd();
        let h = hooks(json!({
            "Stop": [{"hooks": [{"type": "command", "command": "echo main >&2; exit 2"}]}],
            "SubagentStop": [{"matcher": "explore", "hooks": [{"type": "command", "command": "cat >&2; exit 2"}]}]
        }));
        assert_eq!(h.stop(false, "", "m").await.block.as_deref(), Some("main"));
        let outcome = h.for_subagent("a1", "explore").stop(true, "found it", "m").await;
        let input: Value = serde_json::from_str(outcome.block.as_deref().unwrap()).unwrap();
        assert_eq!(input["hook_event_name"], "SubagentStop");
        assert_eq!((input["stop_hook_active"].as_bool(), input["last_assistant_message"].as_str()), (Some(true), Some("found it")));
        assert!(h.for_subagent("a2", "general-purpose").stop(false, "", "m").await.block.is_none());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn subagent_start_matches_the_agent_type() {
        let _guard = crate::util::lock_cwd();
        let h = command("SubagentStart", "explore", "echo look in src/ first");
        assert_eq!(h.for_subagent("a1", "explore").subagent_start("m").await.context, ["look in src/ first"]);
        assert!(h.for_subagent("a2", "general-purpose").subagent_start("m").await.context.is_empty());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn session_end_hooks_get_little_time() {
        let _guard = crate::util::lock_cwd();
        let h = command("SessionEnd", "clear", "sleep 30");
        let started = std::time::Instant::now();
        let outcome = h.session_end("clear", "m").await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(outcome.notices[0].contains("timed out after 1.5s"), "{:?}", outcome.notices);
        assert!(h.session_end("prompt_input_exit", "m").await.notices.is_empty(), "the matcher picks the reason");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn hooks_that_block_on_failure() {
        let _guard = crate::util::lock_cwd();
        let failing = |on_failure: &str| {
            hooks(json!({"PreToolUse": [{"hooks": [
                {"type": "command", "command": "echo broken >&2; exit 1", "onFailure": on_failure}]}]}))
        };
        let outcome = failing("continue").pre_tool_use("bash", "t1", &json!({}), "m").await;
        assert!(outcome.block.is_none() && outcome.notices[0].contains("failed (exit code 1): broken"));
        let outcome = failing("block").pre_tool_use("bash", "t1", &json!({}), "m").await;
        assert!(outcome.block.unwrap().contains("broken, and it blocks when it fails"));
        let missing = hooks(json!({"Stop": [{"hooks": [
            {"type": "command", "command": "/nonexistent/hook", "args": [], "onFailure": "block"}]}]}));
        assert!(missing.stop(false, "", "m").await.block.is_some(), "a hook that cannot start fails too");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_hook_that_runs_too_long_is_killed() {
        let _guard = crate::util::lock_cwd();
        let h = hooks(json!({"Stop": [{"hooks": [{"type": "command", "command": "sleep 30", "timeout": 1}]}]}));
        let started = std::time::Instant::now();
        let outcome = h.stop(false, "", "m").await;
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(outcome.notices[0].contains("timed out"), "{:?}", outcome.notices);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn plugin_hooks() {
        let _guard = crate::util::lock_cwd();
        let mut plugin = PluginDirs::new("/p/root".into(), "/p/data".into());
        plugin.options.push(crate::extensions::skills::PluginOption { key: "token".into(), value: "t0k".into(), sensitive: true });
        let events = json!({
            "UserPromptSubmit": [{"hooks": [
                {"type": "command", "command": "echo \"${CLAUDE_PLUGIN_ROOT}/x $CLAUDE_PLUGIN_ROOT ${user_config.token}\"", "timeout": 5},
                {"type": "command", "command": "printf", "args": ["%s|%s", "${CLAUDE_PLUGIN_ROOT}", "$CLAUDE_PLUGIN_OPTION_TOKEN"]},
                {"type": "agent", "prompt": "is this ok?"}
            ]}],
            "Elicitation": [{"hooks": [{"type": "command", "command": "true"}]}],
            "Notification": [{"matcher": "idle_prompt", "hooks": [{"type": "command", "command": "cat >&2; exit 1"}]}]
        });
        let mut warnings = Vec::new();
        let config = plugin_config(&events, &plugin, &mut warnings);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("`Elicitation`")) && warnings.iter().any(|w| w.contains("`agent`")));
        let hooks = Hooks::with_config(Arc::new(config));
        let mut context = hooks.user_prompt_submit("hi", "m").await.context;
        context.sort();
        // The exec form passes `$…` on as it is.
        assert_eq!(context, ["/p/root/x /p/root t0k", "/p/root|$CLAUDE_PLUGIN_OPTION_TOKEN"]);
        let outcome = hooks.notification("idle_prompt", "waiting", "m").await;
        let input: Value = serde_json::from_str(outcome.notices[0].split_once(": ").unwrap().1).unwrap();
        assert_eq!((input["notification_type"].as_str(), input["message"].as_str()), (Some("idle_prompt"), Some("waiting")));
        assert!(hooks.notification("other", "x", "m").await.notices.is_empty(), "the matcher picks the type");
        assert!(hooks.has(Event::Notification) && !hooks.has(Event::Stop));
        assert_eq!(tool_named("Bash"), Some("bash"));
        assert_eq!(tool_named("KillShell"), Some("kill_shell"));
        assert_eq!(tool_named("Glob"), Some("glob"));
        assert_eq!(tool_named("LS"), Some("glob"));
        assert_eq!(tool_named("WebFetch"), None);
    }

    #[test]
    fn if_rules() {
        let rule = |r: &str| Condition::parse(r).unwrap();
        let bash = |c: &str| json!({"command": c});
        let names = tool_names("bash");
        assert!(rule("Bash(git commit:*)").matches(&names, &bash("git commit -m x")));
        assert!(rule("Bash(git commit:*)").matches(&names, &bash("git add . && git commit")));
        assert!(!rule("Bash(git commit:*)").matches(&names, &bash("git commits")));
        assert!(rule("Bash(git push *)").matches(&names, &bash("git push")));
        assert!(rule("Bash(git push *)").matches(&names, &bash("cd x; git push origin main")));
        assert!(!rule("Bash(git push *)").matches(&names, &bash("echo git push")));
        assert!(rule("Bash(git -C * commit *)").matches(&names, &bash("git -C /r commit -m x")));
        assert!(rule("Bash(py* *claude-security*scripts/*.py *)").matches(&names, &bash("python3 /p/claude-security/scripts/a.py --x")));
        assert!(rule("Bash").matches(&names, &bash("ls")) && !rule("Bash").matches(&tool_names("read"), &json!({})));
        let file = |p: &str| json!({"file_path": p});
        assert!(rule("Edit(src/**/*.rs)").matches(&tool_names("edit"), &file("src/a/b.rs")));
        assert!(!rule("Edit(src/*.rs)").matches(&tool_names("edit"), &file("src/a/b.rs")));
        assert!(!rule("Edit(src/*.rs)").matches(&tool_names("write"), &file("src/a.rs")), "another tool");
        assert!(Condition::parse("Bash(x").is_err() && Condition::parse("(x)").is_err());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn if_rules_pick_the_calls() {
        let _guard = crate::util::lock_cwd();
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("seen");
        let h = hooks(json!({"PreToolUse": [{"matcher": "Bash", "hooks": [
            {"type": "command", "command": format!("echo commit >> {}", seen.display()), "if": "Bash(git commit:*)"},
            {"type": "command", "command": format!("echo commit >> {}", seen.display()), "if": "Bash(git push:*)"}
        ]}]}));
        h.pre_tool_use("bash", "t", &json!({"command": "ls"}), "m").await;
        assert!(!seen.exists());
        h.pre_tool_use("bash", "t", &json!({"command": "git push"}), "m").await;
        assert_eq!(std::fs::read_to_string(&seen).unwrap(), "commit\n", "the second, though its command is the first's");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn background_hooks_report_through_the_inbox() {
        let _guard = crate::util::lock_cwd();
        let mut h = hooks(json!({"PostToolUse": [{"hooks": [
            {"type": "command", "command": "sleep 0.2; echo 'found a leak' >&2; exit 2", "asyncRewake": true, "rewakeMessage": "Review:"},
            {"type": "command", "command": "echo '{\"hookSpecificOutput\": {\"additionalContext\": \"noted\"}}'", "async": true}
        ]}]}));
        let inbox = Arc::new(Inbox::default());
        h.set_inbox(inbox.clone());
        let started = std::time::Instant::now();
        let outcome = h.post_tool_use("bash", "t", &json!({}), &ToolOutput::ok("x"), "m").await;
        assert!(started.elapsed() < Duration::from_millis(150), "not waited for");
        assert!(outcome.block.is_none() && outcome.context.is_empty());
        tokio::time::timeout(Duration::from_secs(5), inbox.wanted()).await.unwrap();
        let texts: Vec<String> = inbox.take().iter().map(|b| b["text"].as_str().unwrap().to_string()).collect();
        assert_eq!(texts.len(), 2, "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("noted")));
        assert!(texts.iter().any(|t| t.contains("Review:\n\nfound a leak") && t.contains("PostToolUse")), "{texts:?}");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn exec_form_in_the_settings() {
        let _guard = crate::util::lock_cwd();
        let h = hooks(json!({"UserPromptSubmit": [{"hooks": [{"type": "command", "command": "echo", "args": ["a b", "$HOME"]}]}]}));
        assert_eq!(h.user_prompt_submit("hi", "m").await.context, ["a b $HOME"]);
        let bad = json!({"Stop": [{"hooks": [{"type": "command", "args": ["x"]}]}]});
        assert!(serde_json::from_value::<Config>(bad).is_err());
    }

    #[tokio::test]
    async fn prompt_hooks_ask_the_model() {
        use crate::agent::mock::{gateway, texts};
        let (client, mut requests) = gateway(|n| {
            let answer = match n {
                0 => r#"{"ok": false, "reason": "the tests were not run"}"#,
                1 => "Sure: {\"ok\": true}",
                _ => "no idea",
            };
            (vec![json!({"type": "text", "text": answer})], "end_turn", 10)
        })
        .await;
        let mut h = hooks(json!({"Stop": [{"hooks": [{"type": "prompt", "prompt": "Check this: $ARGUMENTS", "model": "haiku"}]}]}));
        assert!(h.stop(false, "done", "m").await.notices[0].contains("no model to ask"));
        h.set_client(client);
        let outcome = h.stop(false, "done", "m").await;
        assert_eq!(outcome.block.as_deref(), Some("the tests were not run"));
        let request = requests.recv().await.unwrap();
        let asked = &texts(&request["messages"][0])[0];
        assert!(asked.starts_with("Check this: {") && asked.contains(r#""last_assistant_message":"done""#), "{asked}");
        assert!(request["system"].to_string().contains("evaluate a hook"));
        assert!(h.stop(false, "done", "m").await.block.is_none());
        let outcome = h.stop(false, "done", "m").await;
        assert!(outcome.notices[0].contains("without the JSON asked for: no idea"), "{:?}", outcome.notices);
    }
}
