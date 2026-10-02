//! Hooks: shell commands from the settings file (and the plugins it
//! enables) that run at points of the agent loop, in the format of Claude Code's (which Codex shares), so
//! scripts written for those work here too.
//!
//! ```json
//! "hooks": {
//!   "PreToolUse": [
//!     { "matcher": "Bash", "hooks": [{ "type": "command", "command": "~/bin/guard.sh", "timeout": 30 }] }
//!   ]
//! }
//! ```
//!
//! A hook gets the event as JSON on stdin and answers with its exit code (2
//! blocks, with stderr as the reason) or JSON on stdout. Tools are matched
//! by their Claude Code names as well (`Bash`, `Edit|Write`), hooks see them
//! under those names, and the file tools' `path` also comes as `file_path`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::future::join_all;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::tools::{KillGroup, ToolOutput};

/// How long a hook may run unless it sets `timeout`.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
/// The same for SessionEnd hooks, which hold up quitting, and the most they
/// may set.
const SESSION_END_TIMEOUT: Duration = Duration::from_millis(1500);
const MAX_SESSION_END_TIMEOUT: Duration = Duration::from_secs(60);
/// Text a hook gives the model is cut to this many bytes.
const MAX_CONTEXT_BYTES: usize = 10_000;

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
#[serde(deny_unknown_fields)]
struct Handler {
    #[serde(rename = "type")]
    _kind: Kind,
    command: String,
    /// Seconds.
    timeout: Option<u64>,
    /// Set for a plugin's hooks: `CLAUDE_PLUGIN_ROOT` and the like.
    #[serde(skip)]
    env: Vec<(String, String)>,
}

/// Only command hooks are supported.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Command,
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
const ALIASES: &[(&str, &[&str])] = &[
    ("agent", &["Agent", "Task"]),
    ("bash", &["Bash"]),
    ("bash_output", &["BashOutput"]),
    ("edit", &["Edit"]),
    // `LS` lists a directory; agents that list it get `glob` for that.
    ("glob", &["Glob", "LS"]),
    ("grep", &["Grep"]),
    ("kill_shell", &["KillShell"]),
    ("message_agent", &["SendMessage"]),
    ("read", &["Read"]),
    ("skill", &["Skill"]),
    ("stop_agent", &["TaskStop"]),
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
fn tool_names(tool: &str) -> Vec<&str> {
    std::iter::once(tool).chain(aliases(tool).iter().copied()).collect()
}

/// The tool's name as hooks see it.
fn hook_tool_name(tool: &str) -> &str {
    aliases(tool).first().copied().unwrap_or(tool)
}

/// The tool input as hooks see it: the file tools' `path` also as
/// `file_path`.
fn input_for_hook(tool: &str, input: &Value) -> Value {
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
/// `events` with `vars` (`${CLAUDE_PLUGIN_ROOT}` and the like) replaced in
/// the commands and set in their environment. What the harness cannot run
/// (another event, another type of hook) is left out with a warning rather
/// than failing the plugin, which is not the user's to fix.
pub fn plugin_config(events: &Value, vars: &[(String, String)], warnings: &mut Vec<String>) -> Config {
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
                let kind = handler["type"].as_str().unwrap_or_default();
                let Some(command) = handler["command"].as_str().filter(|_| kind == "command") else {
                    warnings.push(format!("{name} hook of type `{kind}` is not supported; left out"));
                    continue;
                };
                let command = vars.iter().fold(command.to_string(), |c, (k, v)| c.replace(&format!("${{{k}}}"), v));
                let timeout = handler["timeout"].as_u64();
                hooks.push(Handler { _kind: Kind::Command, command, timeout, env: vars.to_vec() });
            }
            if !hooks.is_empty() {
                config.entry(event).or_default().push(Group { matcher, hooks });
            }
        }
    }
    config
}

static CONFIG: OnceLock<Arc<Config>> = OnceLock::new();

/// Sets the hooks of every conversation from here on, from the settings.
pub fn init(config: Config) {
    let _ = CONFIG.set(Arc::new(config));
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
}

impl Default for Hooks {
    /// The hooks from the settings.
    fn default() -> Self {
        Self::with_config(CONFIG.get().cloned().unwrap_or_default())
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
        Self { config, session: Arc::new(Mutex::new(session)), subagent: None }
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
        for group in groups.iter().filter(|g| !event.uses_matcher() || g.matcher.matches(names)) {
            for handler in &group.hooks {
                if !handlers.iter().any(|h| h.command == handler.command) {
                    handlers.push(handler);
                }
            }
        }
        let mut outcome = Outcome::default();
        if handlers.is_empty() {
            return outcome;
        }
        let input = self.input(event, fields, model).to_string();
        let replies = join_all(handlers.iter().map(|h| {
            let timeout = match event {
                Event::SessionEnd => {
                    h.timeout.map_or(SESSION_END_TIMEOUT, Duration::from_secs).min(MAX_SESSION_END_TIMEOUT)
                }
                _ => h.timeout.map_or(DEFAULT_TIMEOUT, Duration::from_secs),
            };
            run_command(&h.command, &h.env, &input, timeout)
        }))
        .await;
        for (handler, reply) in handlers.iter().zip(replies) {
            outcome.add(event, &handler.command, reply);
        }
        outcome
    }

    fn input(&self, event: Event, fields: Value, model: &str) -> Value {
        let session = self.session.lock().unwrap();
        let cwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
        let mut input = json!({
            "session_id": session.id,
            "transcript_path": session.transcript.as_ref().map(|p| p.display().to_string()),
            "cwd": cwd,
            "hook_event_name": event.as_str(),
            // The harness never asks before running a tool.
            "permission_mode": "bypassPermissions",
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
    /// Takes in what the hook `command` answered.
    fn add(&mut self, event: Event, command: &str, reply: Result<Reply, String>) {
        let name = format!("{} hook `{}`", event.as_str(), crate::util::ellipsize(command, 60));
        let reply = match reply {
            Ok(reply) => reply,
            Err(e) => return self.notices.push(format!("{name} failed: {e}")),
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
                return self.notices.push(format!("{name} failed ({status}){detail}"));
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
            Err(e) => return self.notices.push(format!("{name} printed invalid JSON: {e}")),
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
            // There is no one to ask: the call is refused.
            Some("ask") => self.block(&format!(
                "{} (the hook asked for the user's approval, which the harness cannot ask for)",
                why.unwrap_or("needs approval")
            )),
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

/// Runs `sh -c command` with `input` on stdin, in a process group of its
/// own that is killed if it times out or the call is dropped.
async fn run_command(
    command: &str,
    env: &[(String, String)],
    input: &str,
    timeout: Duration,
) -> Result<Reply, String> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).process_group(0).envs(env.iter().map(|(k, v)| (k, v)));
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
            json!({"Notification": []}),
            json!({"PreCompact": [{"hooks": [{"type": "command", "command": "y", "statusMessage": "z"}]}]}),
            json!({"Stop": [{"hooks": [{"type": "prompt", "command": "y"}]}]}),
            json!({"Stop": [{"hooks": [{"type": "command", "command": "y", "async": true}]}]}),
            json!({"PreToolUse": [{"matcher": "(", "hooks": []}]}),
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
        let vars = vec![("CLAUDE_PLUGIN_ROOT".to_string(), "/p/root".to_string())];
        let events = json!({
            "UserPromptSubmit": [{"hooks": [
                {"type": "command", "command": "echo \"${CLAUDE_PLUGIN_ROOT}/x $CLAUDE_PLUGIN_ROOT\"", "timeout": 5},
                {"type": "prompt", "prompt": "is this ok?"}
            ]}],
            "Notification": [{"hooks": [{"type": "command", "command": "true"}]}]
        });
        let mut warnings = Vec::new();
        let config = plugin_config(&events, &vars, &mut warnings);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("`Notification`")) && warnings.iter().any(|w| w.contains("`prompt`")));
        let outcome = Hooks::with_config(Arc::new(config)).user_prompt_submit("hi", "m").await;
        assert_eq!(outcome.context, ["/p/root/x /p/root"]);
        assert_eq!(tool_named("Bash"), Some("bash"));
        assert_eq!(tool_named("KillShell"), Some("kill_shell"));
        assert_eq!(tool_named("Glob"), Some("glob"));
        assert_eq!(tool_named("LS"), Some("glob"));
        assert_eq!(tool_named("WebFetch"), None);
    }
}
