//! Permissions: whether a tool call runs, asks the user first, or is
//! refused. The settings' `permissions` section, in Claude Code's format:
//!
//! ```json
//! {
//!   "permissions": {
//!     "defaultMode": "default",
//!     "allow": ["Bash(cargo test:*)", "mcp__exa"],
//!     "ask": ["Bash(git push:*)"],
//!     "deny": ["Read(.env)", "Bash(rm -rf:*)"]
//!   }
//! }
//! ```
//!
//! Rules take Claude Code's permission syntax, as hooks' `if` does: a tool
//! (`Bash`, `edit`), with what its command starts with or what its path
//! matches in parentheses; `mcp__<server>` is all of a server's tools and a
//! trailing `*` any name it starts. Deny rules win, then ask, then allow,
//! then the mode: `default` runs what only reads (and, in the sandbox,
//! `bash`) and asks for the rest; `acceptEdits` also edits files in the
//! working directory; `plan` refuses anything that changes things;
//! `bypassPermissions`, the default, asks for nothing. A conversation's
//! subagents share its permissions and mode.

use std::sync::{Arc, Mutex, OnceLock, RwLock};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::extensions::hooks::{self, Condition};
use crate::tools::Tool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
pub enum Mode {
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    #[serde(rename = "plan")]
    Plan,
    #[default]
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Default, Mode::AcceptEdits, Mode::Plan, Mode::BypassPermissions];

    /// The name the settings and hooks use.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Default => "default",
            Mode::AcceptEdits => "acceptEdits",
            Mode::Plan => "plan",
            Mode::BypassPermissions => "bypassPermissions",
        }
    }

    pub fn parse(name: &str) -> Result<Self, String> {
        Self::ALL.into_iter().find(|m| m.as_str().eq_ignore_ascii_case(name.trim())).ok_or_else(|| {
            let names: Vec<&str> = Self::ALL.iter().map(|m| m.as_str()).collect();
            format!("unknown permission mode `{name}` (one of {})", names.join(", "))
        })
    }

    /// The mode Shift+Tab goes to from this one.
    pub fn next(self) -> Self {
        Self::ALL[(Self::ALL.iter().position(|m| *m == self).unwrap_or(0) + 1) % Self::ALL.len()]
    }

    /// For the user: what it does.
    pub fn describe(self) -> &'static str {
        match self {
            Mode::Default => "asks before tools that change things",
            Mode::AcceptEdits => "edits files without asking; asks before other changes",
            Mode::Plan => "only reads and plans; changes are refused",
            Mode::BypassPermissions => "runs every tool without asking",
        }
    }
}

/// The `permissions` section of the settings.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(rename = "defaultMode")]
    pub default_mode: Option<Mode>,
    #[serde(default)]
    allow: Vec<Rule>,
    #[serde(default)]
    ask: Vec<Rule>,
    #[serde(default)]
    deny: Vec<Rule>,
}

/// A permission rule, as the settings give it.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "String")]
pub struct Rule {
    text: String,
    matcher: Matcher,
}

#[derive(Debug, Clone)]
enum Matcher {
    /// Built-in tools, as hooks' `if` rules match them.
    Tool(Condition),
    /// MCP tools: a name, all of a server's (`mcp__server`) or those a
    /// prefix starts (`mcp__server__get*`).
    Mcp { name: String, prefix: bool },
}

impl TryFrom<String> for Rule {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        let trimmed = text.trim();
        let matcher = if trimmed.starts_with("mcp__") {
            match trimmed.strip_suffix('*') {
                Some(prefix) => Matcher::Mcp { name: prefix.to_string(), prefix: true },
                None => Matcher::Mcp { name: trimmed.to_string(), prefix: false },
            }
        } else {
            Matcher::Tool(Condition::parse(trimmed).map_err(|e| e.replace("`if` rule", "permission rule"))?)
        };
        Ok(Self { text: trimmed.to_string(), matcher })
    }
}

impl Rule {
    fn matches(&self, tool: &str, input: &Value) -> bool {
        match &self.matcher {
            Matcher::Tool(condition) => condition.matches(&hooks::tool_names(tool), &hooks::input_for_hook(tool, input)),
            Matcher::Mcp { name, prefix: true } => tool.starts_with(name.as_str()),
            // `mcp__server` is all of the server's tools.
            Matcher::Mcp { name, prefix: false } => {
                tool == name || (name.matches("__").count() == 1 && tool.strip_prefix(name.as_str()).is_some_and(|rest| rest.starts_with("__")))
            }
        }
    }
}

static CONFIG: OnceLock<Arc<Config>> = OnceLock::new();

/// Takes `--permission-mode MODE` (or `=MODE`) out of the command line.
pub fn parse_flag(args: Vec<String>) -> anyhow::Result<(Option<Mode>, Vec<String>)> {
    let mut mode = None;
    let mut rest = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let value = match arg.strip_prefix("--permission-mode") {
            Some("") => args.next(),
            Some(value) if value.starts_with('=') => Some(value[1..].to_string()),
            _ => {
                rest.push(arg);
                continue;
            }
        };
        let value = value.ok_or_else(|| anyhow::anyhow!("--permission-mode needs a mode"))?;
        mode = Some(Mode::parse(&value).map_err(anyhow::Error::msg)?);
    }
    Ok((mode, rest))
}

/// Sets the rules and default mode of every conversation, from the
/// settings.
pub fn set(config: Config) {
    let _ = CONFIG.set(Arc::new(config));
}

/// The tools that start, steer or wait for other work, whose own calls
/// are what is checked.
const UNCHECKED: [&str; 6] = ["agent", "message_agent", "stop_agent", "wait_tasks", "bash_output", "kill_shell"];

/// What happens to a call, before anyone is asked.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    /// The user decides; why it needs asking.
    Ask(String),
    /// Refused, with what the model is told.
    Deny(String),
}

/// A call waiting for the user's decision.
#[derive(Debug, Clone)]
pub struct Request {
    pub tool: String,
    pub input: Value,
    /// Why it needs asking.
    pub reason: String,
    /// The rule that would allow calls like it, for "don't ask again".
    pub suggestion: String,
    /// The subagent asking, if it is one.
    pub agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Allow,
    /// Allow it, and calls the rule matches for the rest of the session.
    AllowAlways,
    /// Refuse it, with what the model is told, if the user said.
    Deny(Option<String>),
}

/// Asks the user about calls: the frontend.
#[async_trait]
pub trait Asker: Send + Sync {
    /// The user's answer about `request`; `None` if no one can be asked
    /// now.
    async fn ask(&self, request: &Request) -> Option<Answer>;
}

/// One conversation's permissions: the settings' rules, the mode, what the
/// user allowed for the session, and who asks; shared with its subagents.
pub struct Permissions {
    config: Arc<Config>,
    mode: RwLock<Mode>,
    session: Mutex<Vec<Rule>>,
    asker: RwLock<Option<Arc<dyn Asker>>>,
}

impl Default for Permissions {
    fn default() -> Self {
        let config = CONFIG.get().cloned().unwrap_or_default();
        Self::with_config(config)
    }
}

impl Permissions {
    pub fn with_config(config: Arc<Config>) -> Self {
        let mode = RwLock::new(config.default_mode.unwrap_or_default());
        Self { config, mode, session: Mutex::new(Vec::new()), asker: RwLock::new(None) }
    }

    pub fn mode(&self) -> Mode {
        *self.mode.read().unwrap()
    }

    pub fn set_mode(&self, mode: Mode) {
        *self.mode.write().unwrap() = mode;
    }

    /// The mode the settings start conversations in.
    pub fn default_mode(&self) -> Mode {
        self.config.default_mode.unwrap_or_default()
    }

    pub fn set_asker(&self, asker: Option<Arc<dyn Asker>>) {
        *self.asker.write().unwrap() = asker;
    }

    /// The rules, for `/permissions`: each kind with its rules, the session's
    /// as allow rules.
    pub fn rules(&self) -> Vec<(&'static str, Vec<String>)> {
        let texts = |rules: &[Rule]| rules.iter().map(|r| r.text.clone()).collect::<Vec<_>>();
        let session = texts(&self.session.lock().unwrap());
        vec![
            ("deny", texts(&self.config.deny)),
            ("ask", texts(&self.config.ask)),
            ("allow", texts(&self.config.allow)),
            ("allowed for this session", session),
        ]
    }

    /// What happens to a call of `tool` (named `name`) with `input`, before
    /// the user is asked; `hook` is what PreToolUse hooks decided.
    pub fn decide(&self, name: &str, tool: Option<&dyn Tool>, input: &Value, hook: Option<&HookDecision>) -> Decision {
        let matching = |rules: &[Rule]| rules.iter().find(|r| r.matches(name, input)).map(|r| r.text.clone());
        if let Some(rule) = matching(&self.config.deny) {
            return Decision::Deny(format!("The permission rule `{rule}` denies this call."));
        }
        match hook {
            Some(HookDecision::Allow) => return Decision::Allow,
            Some(HookDecision::Ask(reason)) => return Decision::Ask(reason.clone()),
            None => {}
        }
        if let Some(rule) = matching(&self.config.ask) {
            return Decision::Ask(format!("the rule `{rule}` asks for it"));
        }
        if UNCHECKED.contains(&name) || matching(&self.config.allow).is_some() || matching(&self.session.lock().unwrap()).is_some() {
            return Decision::Allow;
        }
        let reads = tool.is_some_and(|t| t.read_only());
        match self.mode() {
            Mode::BypassPermissions => Decision::Allow,
            _ if reads => Decision::Allow,
            Mode::Plan => Decision::Deny(
                "Plan mode is on: only tools that read run. Finish researching and present your plan; the user \
                 turns plan mode off to have you carry it out."
                    .into(),
            ),
            Mode::AcceptEdits if matches!(name, "edit" | "write") && in_working_dir(input) => Decision::Allow,
            _ if name == "bash" && crate::security::sandbox::active().is_some_and(|s| s.auto_allows_bash()) => Decision::Allow,
            _ => Decision::Ask("it changes things".into()),
        }
    }

    /// Settles a call [`Permissions::decide`] said to ask about: the user
    /// decides, if they can be asked. `Err` is what the model is told
    /// when it does not run.
    pub async fn ask(&self, request: Request) -> Result<(), String> {
        let asker = self.asker.read().unwrap().clone();
        let answer = match asker {
            Some(asker) => asker.ask(&request).await,
            None => None,
        };
        match answer {
            Some(Answer::Allow) => Ok(()),
            Some(Answer::AllowAlways) => {
                if let Ok(rule) = Rule::try_from(request.suggestion.clone()) {
                    self.session.lock().unwrap().push(rule);
                }
                Ok(())
            }
            Some(Answer::Deny(message)) => Err(match message {
                Some(message) => format!("The user declined this call, saying: {message}"),
                None => "The user declined this call. Ask them what to do instead, or do something else.".into(),
            }),
            None => Err(format!(
                "This call needs the user's approval ({}), which cannot be asked for here. Say what you would \
                 run, so they can allow it (`permissions.allow` in the settings, or another permission mode).",
                request.reason
            )),
        }
    }
}

/// What PreToolUse hooks decided about a call's permission.
#[derive(Debug, Clone, PartialEq)]
pub enum HookDecision {
    /// `permissionDecision: "allow"`: it runs without asking, unless a deny
    /// rule refuses it.
    Allow,
    /// `permissionDecision: "ask"`, with the hook's reason.
    Ask(String),
}

/// Whether the file a call of `edit` or `write` changes is in the working
/// directory.
fn in_working_dir(input: &Value) -> bool {
    let Some(path) = input["path"].as_str() else { return false };
    // A subagent's worktree, if it has one.
    let cwd = crate::tools::workdir::get();
    let path = cwd.join(path);
    path.starts_with(&cwd) && !path.components().any(|c| c == std::path::Component::ParentDir)
}

/// The rule that would allow calls like this one: a command's first word
/// (and its subcommand), or the tool.
pub fn suggestion(tool: &str, input: &Value) -> String {
    let name = hooks::hook_tool_name(tool);
    if tool != "bash" {
        return name.to_string();
    }
    let command = input["command"].as_str().unwrap_or_default();
    let mut words = command.split_whitespace();
    let Some(first) = words.next() else { return name.to_string() };
    let sub = words.next().filter(|w| w.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') && !w.starts_with('-'));
    match sub {
        Some(sub) => format!("{name}({first} {sub}:*)"),
        None => format!("{name}({first}:*)"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::tools::{FileTracker, ToolRegistry};

    fn permissions(config: Value) -> Permissions {
        Permissions::with_config(Arc::new(serde_json::from_value(config).unwrap()))
    }

    struct Answers(Mutex<Vec<Answer>>);

    #[async_trait]
    impl Asker for Answers {
        async fn ask(&self, _: &Request) -> Option<Answer> {
            self.0.lock().unwrap().pop()
        }
    }

    #[test]
    fn rules_then_the_mode_decide() {
        let tools = ToolRegistry::with_defaults(FileTracker::default());
        let p = permissions(json!({
            "defaultMode": "default",
            "allow": ["Bash(cargo test:*)", "mcp__exa"],
            "ask": ["Bash(git push:*)"],
            "deny": ["Read(.env)", "Bash(rm -rf:*)", "mcp__gh__delete*"]
        }));
        let decide = |name: &str, input: Value| p.decide(name, tools.get(name).map(|t| t.as_ref()), &input, None);
        assert_eq!(decide("bash", json!({"command": "cargo test --lib"})), Decision::Allow);
        assert!(matches!(decide("bash", json!({"command": "ls; rm -rf /"})), Decision::Deny(_)), "a part of it");
        assert!(matches!(decide("bash", json!({"command": "git push origin"})), Decision::Ask(_)));
        assert!(matches!(decide("bash", json!({"command": "make"})), Decision::Ask(_)), "default mode asks");
        assert_eq!(decide("read", json!({"path": "src/main.rs"})), Decision::Allow, "it only reads");
        assert!(matches!(decide("read", json!({"path": ".env"})), Decision::Deny(_)));
        assert_eq!(decide("mcp__exa__search", json!({})), Decision::Allow, "all of the server's tools");
        assert!(matches!(decide("mcp__exa2__search", json!({})), Decision::Ask(_)), "not another server's");
        assert!(matches!(decide("mcp__gh__delete_repo", json!({})), Decision::Deny(_)));
        assert_eq!(decide("agent", json!({})), Decision::Allow, "what subagents do is checked instead");
        let hook = HookDecision::Ask("the hook wants it".into());
        assert_eq!(p.decide("bash", None, &json!({"command": "cargo test"}), Some(&hook)), Decision::Ask("the hook wants it".into()));
        assert!(matches!(p.decide("bash", None, &json!({"command": "rm -rf x"}), Some(&HookDecision::Allow)), Decision::Deny(_)));

        p.set_mode(Mode::AcceptEdits);
        assert_eq!(decide("edit", json!({"path": "src/main.rs"})), Decision::Allow);
        assert!(matches!(decide("edit", json!({"path": "/etc/hosts"})), Decision::Ask(_)), "outside the working directory");
        assert!(matches!(decide("edit", json!({"path": "../x"})), Decision::Ask(_)));
        p.set_mode(Mode::Plan);
        assert!(matches!(decide("edit", json!({"path": "src/main.rs"})), Decision::Deny(_)));
        assert_eq!(decide("grep", json!({"pattern": "x"})), Decision::Allow);
        p.set_mode(Mode::BypassPermissions);
        assert_eq!(decide("bash", json!({"command": "make"})), Decision::Allow);
        assert!(matches!(decide("bash", json!({"command": "git push"})), Decision::Ask(_)), "ask rules still ask");
    }

    #[tokio::test]
    async fn asks_the_user_and_remembers_for_the_session() {
        let p = permissions(json!({"defaultMode": "default"}));
        let request = |command: &str| Request {
            tool: "bash".into(),
            input: json!({"command": command}),
            reason: "it changes things".into(),
            suggestion: suggestion("bash", &json!({"command": command})),
            agent: None,
        };
        assert!(p.ask(request("make")).await.unwrap_err().contains("cannot be asked for here"));
        let answers = vec![Answer::Deny(Some("use ninja".into())), Answer::AllowAlways];
        p.set_asker(Some(Arc::new(Answers(Mutex::new(answers)))));
        assert_eq!(p.ask(request("cargo build --release")).await, Ok(()));
        assert_eq!(p.decide("bash", None, &json!({"command": "cargo build"}), None), Decision::Allow, "remembered");
        assert!(matches!(p.decide("bash", None, &json!({"command": "cargo run"}), None), Decision::Ask(_)));
        assert!(p.ask(request("make")).await.unwrap_err().contains("use ninja"));
        assert_eq!(p.rules()[3].1, ["Bash(cargo build:*)"]);
    }

    #[test]
    fn suggests_rules_and_parses_modes() {
        assert_eq!(suggestion("bash", &json!({"command": "git commit -m x"})), "Bash(git commit:*)");
        assert_eq!(suggestion("bash", &json!({"command": "ls -la"})), "Bash(ls:*)");
        assert_eq!(suggestion("bash", &json!({"command": "./run.sh x"})), "Bash(./run.sh x:*)");
        assert_eq!(suggestion("edit", &json!({"path": "a"})), "Edit");
        assert_eq!(suggestion("mcp__gh__issues", &json!({})), "mcp__gh__issues");
        assert_eq!(Mode::parse("acceptedits").unwrap(), Mode::AcceptEdits);
        assert!(Mode::parse("yolo").is_err());
        assert_eq!(Mode::BypassPermissions.next(), Mode::Default);
        assert!(serde_json::from_value::<Config>(json!({"allow": ["Bash(git"]})).is_err());
        assert!(serde_json::from_value::<Config>(json!({"defaultMode": "auto"})).is_err());
    }
}
