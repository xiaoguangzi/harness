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
//! trailing `*` any name it starts. A compound command (`a && b`, `a | b`,
//! `a; b`) is denied or asked about if any of its commands is, and allowed
//! only if every one of them is; a command substitution (`$(…)`, backticks)
//! is never allowed by a `Bash(…)` pattern. Deny rules win, then ask, then allow,
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
    /// Whether the rule is for every call of `bash`, whatever it runs.
    fn is_all_commands(&self) -> bool {
        matches!(&self.matcher, Matcher::Tool(c) if c.is_whole_tool() && c.matches_command(&hooks::tool_names("bash"), ""))
    }

    /// Whether the rule allows `command`, one command of a compound one.
    fn matches_command(&self, command: &str) -> bool {
        matches!(&self.matcher, Matcher::Tool(c) if c.matches_command(&hooks::tool_names("bash"), command))
    }

    /// Whether the allow rule allows `command`, as written or without its
    /// wrappers (`timeout 30 npm test` for `Bash(npm test:*)`).
    fn allows_command(&self, command: &str) -> bool {
        self.matches_command(command) || self.matches_command(strip_wrappers(command, false))
    }

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
/// are what is checked; `code` checks each call its script makes.
const UNCHECKED: [&str; 7] = ["agent", "message_agent", "stop_agent", "wait_tasks", "bash_output", "kill_shell", "code"];

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
    /// The rules that would allow calls like it, for "don't ask again":
    /// none when they could not cover all of it.
    pub suggestions: Vec<String>,
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

    /// A model's question, independent of permission rules or mode.
    async fn question(&self, _request: &crate::tools::question::Request, _agent: Option<&str>) -> Option<crate::tools::question::Answer> {
        None
    }
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

    pub fn asker(&self) -> Option<Arc<dyn Asker>> {
        self.asker.read().unwrap().clone()
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
        if UNCHECKED.contains(&name) || self.allowed(name, input) {
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

    /// Whether the allow rules or the session's rules allow the call. Each
    /// command of a compound `bash` command must be allowed, by one rule or
    /// another; a command the parts of which cannot be told apart is not.
    fn allowed(&self, name: &str, input: &Value) -> bool {
        let session = self.session.lock().unwrap();
        let rules: Vec<&Rule> = self.config.allow.iter().chain(session.iter()).collect();
        if name != "bash" {
            return rules.iter().any(|r| r.matches(name, input));
        }
        if rules.iter().any(|r| r.is_all_commands()) {
            return true;
        }
        let Some(commands) = input["command"].as_str().and_then(commands) else { return false };
        !commands.is_empty() && commands.iter().all(|c| rules.iter().any(|r| r.allows_command(c)))
    }

    /// The rules that would allow calls like this one from now on, for
    /// "don't ask again": a rule for each command of a compound `bash` one
    /// that is not allowed yet (at most [`MAX_SUGGESTIONS`]), the tool
    /// otherwise. None when they would not cover the whole command.
    pub fn suggestions(&self, tool: &str, input: &Value) -> Vec<String> {
        let name = hooks::hook_tool_name(tool);
        if tool != "bash" {
            return vec![name.to_string()];
        }
        let Some(commands) = input["command"].as_str().and_then(commands) else { return Vec::new() };
        let session = self.session.lock().unwrap();
        let rules: Vec<&Rule> = self.config.allow.iter().chain(session.iter()).collect();
        let mut suggested: Vec<String> = Vec::new();
        for command in commands.iter().filter(|c| !rules.iter().any(|r| r.allows_command(c))) {
            let Some(rule) = command_rule(name, strip_wrappers(command, false)) else { return Vec::new() };
            if !suggested.contains(&rule) {
                suggested.push(rule);
            }
        }
        if suggested.len() > MAX_SUGGESTIONS { Vec::new() } else { suggested }
    }

    /// What the rules say about a call a tool makes on its own while its
    /// call runs (a skill's `` !`command` ``): the approval of the tool's
    /// call covers it, except where a deny rule refuses it or an ask rule
    /// has the user decide.
    fn decide_nested(&self, name: &str, input: &Value) -> Decision {
        let matching = |rules: &[Rule]| rules.iter().find(|r| r.matches(name, input)).map(|r| r.text.clone());
        if let Some(rule) = matching(&self.config.deny) {
            return Decision::Deny(format!("The permission rule `{rule}` denies this call."));
        }
        match matching(&self.config.ask) {
            Some(rule) => Decision::Ask(format!("the rule `{rule}` asks for it")),
            None => Decision::Allow,
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
                let rules = request.suggestions.iter().filter_map(|s| Rule::try_from(s.clone()).ok());
                self.session.lock().unwrap().extend(rules);
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

tokio::task_local! {
    /// The permissions of the conversation whose tool call is running, and
    /// the subagent making it, if it is one.
    static CALLER: (Arc<Permissions>, Option<String>);
}

/// Runs a tool's call (`future`) for the conversation with `permissions`,
/// so that what the tool runs on its own is checked by them ([`nested`]).
pub async fn scope<F: std::future::Future>(permissions: Arc<Permissions>, agent: Option<String>, future: F) -> F::Output {
    CALLER.scope((permissions, agent), future).await
}

/// Whether the tool whose call is running may make a call of `name` with
/// `input` on its own, as a skill does for each `` !`command` `` in its
/// text: what fills such a command comes from the model, so the rules that
/// deny or ask about the call it could not make itself hold here too.
/// `Err` is why it does not run. Outside a tool's call (a skill the user
/// typed) there is nothing to check.
pub async fn nested(name: &str, input: &Value) -> Result<(), String> {
    let Ok((permissions, agent)) = CALLER.try_with(Clone::clone) else { return Ok(()) };
    let reason = match permissions.decide_nested(name, input) {
        Decision::Allow => return Ok(()),
        Decision::Deny(reason) => return Err(reason),
        Decision::Ask(reason) => reason,
    };
    let suggestions = permissions.suggestions(name, input);
    let request = Request { tool: name.to_string(), input: input.clone(), reason, suggestions, agent };
    permissions.ask(request).await
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

/// Rules offered for one compound command, at most: Claude Code's limit.
const MAX_SUGGESTIONS: usize = 5;

/// The commands of a compound `bash` command, split where the shell runs one
/// after another or pipes one into the next (`&&`, `||`, `;`, `|`, `|&`, `&`,
/// newlines), outside quotes; `None` if it cannot be split with certainty (a
/// comment, a heredoc, `$'…'`), or runs a command substitution (`$(…)`,
/// backticks, `<(…)`) that no rule can be said to allow. Redirections (`2>&1`,
/// `&>`) do not split, nor newlines in quotes or after a backslash.
fn commands(command: &str) -> Option<Vec<&str>> {
    let bytes = command.as_bytes();
    let mut parts = Vec::new();
    let (mut start, mut i) = (0, 0);
    // Whether a command must follow the last operator (`&&`, `||`, `|`).
    let mut wants_command = false;
    // An empty part is fine after `;` or `&`, not where a command must follow.
    fn push<'a>(parts: &mut Vec<&'a str>, part: &'a str, wants: bool) -> Option<()> {
        let part = part.trim();
        if !part.is_empty() {
            parts.push(part);
        }
        (!part.is_empty() || !wants).then_some(())
    }
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'\'' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    i += 1;
                }
                if i == bytes.len() {
                    return None;
                }
            }
            b'"' => {
                i += 1;
                loop {
                    match bytes.get(i)? {
                        b'"' => break,
                        b'\\' => i += 1,
                        b'`' => return None,
                        b'$' if bytes.get(i + 1) == Some(&b'(') => return None,
                        _ => {}
                    }
                    i += 1;
                }
            }
            b'`' => return None,
            // ANSI-C quoting and heredocs: not followed here.
            b'$' if matches!(bytes.get(i + 1), Some(b'(' | b'\'')) => return None,
            c @ (b'<' | b'>') => match bytes.get(i + 1) {
                Some(b'(') => return None,
                Some(b'<') if c == b'<' => return None,
                // `>&`, `<&`: the `&` is the redirection's.
                Some(b'&') => i += 1,
                _ => {}
            },
            // `&>`: a redirection.
            b'&' if bytes.get(i + 1) == Some(&b'>') => {}
            // A comment, which may hide a quote or a line's end.
            b'#' if i == 0 || bytes[i - 1].is_ascii_whitespace() || b";&|()<>".contains(&bytes[i - 1]) => return None,
            c @ (b'&' | b'|' | b';' | b'\n') => {
                push(&mut parts, &command[start..i], wants_command)?;
                let doubled = matches!(c, b'&' | b'|') && bytes.get(i + 1) == Some(&c);
                let piped_stderr = c == b'|' && bytes.get(i + 1) == Some(&b'&');
                if doubled || piped_stderr {
                    i += 1;
                }
                wants_command = doubled || c == b'|';
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    push(&mut parts, &command[start..], wants_command)?;
    Some(parts)
}

/// Variables a leading assignment of which an allow rule looks past
/// (`NODE_ENV=test npm test`): they change what a program prints or how it
/// logs, not what it runs. `PATH`, `NODE_OPTIONS`, `GIT_*`, `LD_*` and the
/// like are not here: they can make a command run something else.
const SAFE_VARS: &[&str] = &[
    "CARGO_TERM_COLOR", "CI", "CLICOLOR", "CLICOLOR_FORCE", "COLUMNS", "DEBUG", "FORCE_COLOR", "LANG", "LANGUAGE",
    "LC_ALL", "LC_CTYPE", "NO_COLOR", "NODE_ENV", "PYTHONDONTWRITEBYTECODE", "PYTHONUNBUFFERED", "RUST_BACKTRACE",
    "RUST_LOG", "RUST_TEST_THREADS", "TERM", "TZ",
];

/// One command (a part of a compound one) without the wrappers that run
/// their arguments as the command, as Claude Code strips them before
/// matching rules: `timeout`, `time`, `nice`, `nohup`, `stdbuf`, `command`
/// (not `command -v`), `builtin`, zsh's `noglob`, `xargs` without options,
/// and leading assignments: of [`SAFE_VARS`] only for an allow rule, of any
/// variable (`any_assignment`) for a deny or ask rule. A wrapper whose
/// options are not plain is left on, so the command matches only rules for
/// it as written.
pub(crate) fn strip_wrappers(command: &str, any_assignment: bool) -> &str {
    let mut rest = command.trim();
    while let Some(inner) = strip_one(rest, any_assignment).map(str::trim_start) {
        if inner.is_empty() {
            break;
        }
        rest = inner;
    }
    rest
}

/// What follows the first word of `command`, if it is a wrapper (with its
/// options) or an assignment that [`strip_wrappers`] strips.
fn strip_one(command: &str, any_assignment: bool) -> Option<&str> {
    let (word, mut rest) = split_word(command);
    if let Some((name, value)) = word.split_once('=') {
        let name_ok = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        let safe = any_assignment || (SAFE_VARS.contains(&name) && !value.contains(['`', '$']));
        return (name_ok && safe).then_some(rest);
    }
    // Takes an option's value, attached (`-n5`, `--signal=KILL`) or as the next word.
    let value = |attached: &str, rest: &mut &str, ok: fn(&str) -> bool| -> bool {
        let value = if attached.is_empty() {
            let (next, after) = split_word(rest);
            *rest = after;
            next
        } else {
            attached
        };
        ok(value)
    };
    let number = |v: &str| v.strip_prefix(['-', '+']).unwrap_or(v).parse::<u32>().is_ok();
    let duration = |v: &str| {
        let digits = v.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(v);
        !digits.is_empty() && digits.parse::<f64>().is_ok_and(|d| d >= 0.0) && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
    };
    let plain = |v: &str| !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric());
    let next_is_option = |rest: &str| split_word(rest).0.starts_with('-');
    match word {
        "nohup" | "builtin" | "noglob" => Some(rest),
        "command" | "xargs" => (!next_is_option(rest)).then_some(rest),
        "time" => match split_word(rest) {
            ("-p", after) => Some(after),
            _ => Some(rest),
        },
        "nice" => {
            let (option, after) = split_word(rest);
            if let Some(attached) = option.strip_prefix("--adjustment=") {
                return number(attached).then_some(after);
            }
            match option.strip_prefix("-n") {
                Some(attached) => {
                    rest = after;
                    value(attached, &mut rest, number).then_some(rest)
                }
                None if option.starts_with('-') => number(option).then_some(after),
                None => Some(rest),
            }
        }
        "timeout" => {
            loop {
                let (option, after) = split_word(rest);
                if !option.starts_with('-') {
                    break;
                }
                rest = after;
                let ok = match option {
                    "--preserve-status" | "--foreground" | "-v" | "--verbose" => true,
                    _ if option.starts_with("--signal=") => plain(&option[9..]),
                    _ if option.starts_with("--kill-after=") => duration(&option[13..]),
                    _ if option.starts_with("-s") => value(&option[2..], &mut rest, plain),
                    _ if option.starts_with("-k") => value(&option[2..], &mut rest, duration),
                    _ => false,
                };
                if !ok {
                    return None;
                }
            }
            let (limit, after) = split_word(rest);
            duration(limit).then_some(after)
        }
        "stdbuf" => {
            let mode = |v: &str| v == "L" || v.parse::<u64>().is_ok() || v.get(..v.len().saturating_sub(1)).is_some_and(|n| n.parse::<u64>().is_ok());
            loop {
                let (option, after) = split_word(rest);
                if !option.starts_with('-') {
                    return Some(rest);
                }
                rest = after;
                let ok = match option.split_once('=') {
                    Some(("--input" | "--output" | "--error", v)) => mode(v),
                    _ if matches!(option.get(..2), Some("-i" | "-o" | "-e")) => value(&option[2..], &mut rest, mode),
                    _ => false,
                };
                if !ok {
                    return None;
                }
            }
        }
        _ => None,
    }
}

/// The first word of `command` and what follows it; a word runs to
/// whitespace outside quotes.
fn split_word(command: &str) -> (&str, &str) {
    let command = command.trim_start();
    let bytes = command.as_bytes();
    let mut i = 0;
    let mut quote = None;
    while i < bytes.len() {
        match (quote, bytes[i]) {
            (None, b'\\') => i += 1,
            (None, b'\'' | b'"') => quote = Some(bytes[i]),
            (Some(b'"'), b'\\') => i += 1,
            (Some(q), c) if c == q => quote = None,
            (None, c) if c.is_ascii_whitespace() => break,
            _ => {}
        }
        i += 1;
    }
    let i = i.min(bytes.len());
    (&command[..i], &command[i..])
}

/// The rule that allows `command` (one command, not compound) from now on:
/// its first word and subcommand, `Bash(git commit:*)`. `None` when no rule
/// can say that plainly (a subshell, a group).
fn command_rule(name: &str, command: &str) -> Option<String> {
    let mut words = command.split_whitespace();
    let first = words.next()?;
    if first.contains(['(', ')', '{', '}', '<', '>', '=']) || matches!(first, "!" | "[[" | "if" | "for" | "while" | "do" | "then" | "case") {
        return None;
    }
    let sub = words.next().filter(|w| w.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') && !w.starts_with('-'));
    let rule = match sub {
        Some(sub) => format!("{name}({first} {sub}:*)"),
        None => format!("{name}({first}:*)"),
    };
    Rule::try_from(rule.clone()).ok().filter(|r| r.matches_command(command)).map(|_| rule)
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

    #[test]
    fn every_command_of_a_compound_one_must_be_allowed() {
        let p = permissions(json!({"defaultMode": "default", "allow": ["Bash(cargo test:*)", "Bash(git status)"]}));
        let decide = |command: &str| p.decide("bash", None, &json!({"command": command}), None);
        for allowed in [
            "cargo test",
            "cargo test --lib 2>&1",
            "cargo test &> log",
            "cargo test |& cargo test",
            "cargo test && git status",
            "git status; cargo test;",
            "cargo test -- 'a && b' \"c | d\"",
            "cargo test \\; rm",
            "cargo test 'it $(is) quoted'",
            "cargo test -- \"a\nb\"",
            "cargo test \\\n  --lib",
            "cargo test x\"#\" y#z",
        ] {
            assert_eq!(decide(allowed), Decision::Allow, "{allowed}");
        }
        for asks in [
            "cargo test && make",
            "make || cargo test",
            "cargo test | sh",
            "cargo test & rm -rf x",
            "cargo test\nrm -rf x",
            "cargo test $(rm -rf x)",
            "cargo test `rm -rf x`",
            "cargo test \"$(rm -rf x)\"",
            "cargo test <(rm -rf x)",
            "cargo test $'\\'' ; rm -rf x #'",
            "cargo test 'unclosed ; rm -rf x",
            "cargo test <<EOF\nx\nEOF",
            "cargo test \\>& rm -rf x",
            "cargo test \\<& rm -rf x",
            "cargo test #'\nrm -rf x #'",
            "cargo test # \\\nrm -rf x",
            "cargo test >#'\nrm -rf x #'",
            "cargo test &&",
            "cargo test | ",
            "",
        ] {
            assert!(matches!(decide(asks), Decision::Ask(_)), "{asks:?}");
        }
        let p = permissions(json!({"defaultMode": "default", "allow": ["Bash"]}));
        assert_eq!(p.decide("bash", None, &json!({"command": "make $(x) && y"}), None), Decision::Allow, "all of bash");
    }

    #[test]
    fn rules_look_past_wrappers() {
        let p = permissions(json!({
            "defaultMode": "default",
            "allow": ["Bash(npm test:*)"],
            "ask": ["Bash(git push:*)"],
            "deny": ["Bash(rm:*)"]
        }));
        let decide = |command: &str| p.decide("bash", None, &json!({"command": command}), None);
        for allowed in [
            "timeout 30 npm test",
            "timeout -s KILL -k 5s --preserve-status 2m npm test",
            "timeout --signal=TERM 1.5 npm test",
            "time -p npm test",
            "nice npm test",
            "nice -n 10 npm test",
            "nice -5 npm test",
            "nohup nice -n5 npm test --watch=false",
            "stdbuf -oL -e 0 npm test",
            "stdbuf --output=L npm test",
            "command npm test",
            "builtin npm test",
            "noglob npm test",
            "NODE_ENV=test CI=1 npm test",
            "FORCE_COLOR='1' timeout 30 npm test",
            "xargs npm test",
        ] {
            assert_eq!(decide(allowed), Decision::Allow, "{allowed}");
        }
        for asks in [
            "timeout npm test",
            "timeout -x 30 npm test",
            "timeout 30",
            "nice -n x npm test",
            "stdbuf -oX1 npm test",
            "command -v npm test",
            "xargs -n1 npm test",
            "PATH=. npm test",
            "NODE_OPTIONS=--require=x npm test",
            "NODE_ENV=$(x) npm test",
            "watch npm test",
            "npx npm test",
        ] {
            assert!(matches!(decide(asks), Decision::Ask(_)), "{asks:?}");
        }
        for denied in ["timeout 5 rm -rf x", "FOO=bar rm -rf x", "PATH=/bin nohup rm x", "echo && nice -n 1 rm x"] {
            assert!(matches!(decide(denied), Decision::Deny(_)), "{denied:?}");
        }
        assert!(matches!(decide("timeout 60 git push"), Decision::Ask(_)));
        assert_eq!(p.suggestions("bash", &json!({"command": "timeout 30 cargo build --release"})), ["Bash(cargo build:*)"]);
        assert_eq!(strip_wrappers("  nohup  ", false), "nohup", "a wrapper alone is the command");
        assert_eq!(strip_wrappers("stdbuf -é x", false), "stdbuf -é x");
        assert_eq!(strip_wrappers("stdbuf -oé x", false), "stdbuf -oé x");
    }

    #[tokio::test]
    async fn asks_the_user_and_remembers_for_the_session() {
        let p = permissions(json!({"defaultMode": "default"}));
        let request = |command: &str| Request {
            tool: "bash".into(),
            input: json!({"command": command}),
            reason: "it changes things".into(),
            suggestions: p.suggestions("bash", &json!({"command": command})),
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
        p.set_asker(Some(Arc::new(Answers(Mutex::new(vec![Answer::AllowAlways])))));
        assert_eq!(p.ask(request("cargo build && make check")).await, Ok(()));
        assert_eq!(p.rules()[3].1, ["Bash(cargo build:*)", "Bash(make check:*)"], "only the command not allowed yet");
        assert_eq!(p.decide("bash", None, &json!({"command": "make check && cargo build -q"}), None), Decision::Allow);
    }

    #[tokio::test]
    async fn what_a_tool_runs_on_its_own_keeps_to_the_rules() {
        let p = Arc::new(permissions(json!({"defaultMode": "default", "ask": ["Bash(git push:*)"], "deny": ["Bash(rm -rf:*)"]})));
        let check = |command: &str| {
            let (p, input) = (p.clone(), json!({"command": command}));
            async move { scope(p, None, nested("bash", &input)).await }
        };
        assert_eq!(check("touch a").await, Ok(()), "the tool's own call was approved");
        assert!(check("echo hi; rm -rf /").await.unwrap_err().contains("denies this call"));
        assert!(check("git push origin").await.unwrap_err().contains("cannot be asked for here"));
        p.set_asker(Some(Arc::new(Answers(Mutex::new(vec![Answer::Allow])))));
        assert_eq!(check("git push origin").await, Ok(()));
        assert_eq!(nested("bash", &json!({"command": "rm -rf /"})).await, Ok(()), "the user's own command");
    }

    #[test]
    fn suggests_rules_and_parses_modes() {
        let p = permissions(json!({"allow": ["Bash(cd:*)"]}));
        let suggestion = |tool: &str, command: &str| p.suggestions(tool, &json!({"command": command, "path": "a"})).join(", ");
        assert_eq!(suggestion("bash", "git commit -m x"), "Bash(git commit:*)");
        assert_eq!(suggestion("bash", "ls -la"), "Bash(ls:*)");
        assert_eq!(suggestion("bash", "./run.sh x"), "Bash(./run.sh x:*)");
        assert_eq!(suggestion("bash", "cd app && make && make install | tee log"), "Bash(make:*), Bash(make install:*), Bash(tee log:*)");
        assert_eq!(suggestion("bash", "make $(cat target)"), "", "no rule would allow it");
        assert_eq!(suggestion("bash", "(cd app; make)"), "", "nor a subshell");
        assert_eq!(suggestion("bash", "a; b; c; d; e; f"), "", "more than five");
        assert_eq!(suggestion("edit", ""), "Edit");
        assert_eq!(suggestion("mcp__gh__issues", ""), "mcp__gh__issues");
        assert_eq!(Mode::parse("acceptedits").unwrap(), Mode::AcceptEdits);
        assert!(Mode::parse("yolo").is_err());
        assert_eq!(Mode::BypassPermissions.next(), Mode::Default);
        assert!(serde_json::from_value::<Config>(json!({"allow": ["Bash(git"]})).is_err());
        assert!(serde_json::from_value::<Config>(json!({"defaultMode": "auto"})).is_err());
    }
}
