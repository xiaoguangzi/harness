//! The advisor: a second, usually stronger model the main model consults
//! at decision points, as Claude Code's `/advisor` does.
//!
//! It is the Anthropic API's server-side advisor tool: the request carries
//! `{"type": "advisor_20260301", "name": "advisor", "model": …}`, the model
//! calls it with a `server_tool_use` block when it wants guidance, and the
//! API runs the advisor on the whole transcript and puts its advice in an
//! `advisor_tool_result` block before the model goes on. Nothing runs
//! here: the blocks are kept in the history as they came.
//!
//! The advisor must be at least as capable as the model it advises; a pair
//! the API refuses is not attached (see [`accepts`]).

use serde_json::{Value, json};

/// The tool's type and name in a request.
pub const TOOL_TYPE: &str = "advisor_20260301";
pub const TOOL_NAME: &str = "advisor";
/// The beta the tool needs, also for requests whose history holds its blocks.
pub const BETA: &str = "advisor-tool-2026-03-01";

/// Short names for the advisor, as Claude Code takes them.
const ALIASES: [(&str, &str); 4] = [
    ("fable", "claude-fable-5-2"),
    ("opus", "claude-opus-5-5"),
    ("sonnet", "claude-sonnet-5-5"),
    ("haiku", "claude-haiku-5-5"),
];

/// Words that turn the advisor off.
const OFF: [&str; 4] = ["off", "none", "no", "false"];

/// What a setting, flag or `/advisor` argument names: `Some(model)` with
/// an alias expanded, `None` for `off`.
pub fn parse(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() || OFF.contains(&name.to_ascii_lowercase().as_str()) {
        return None;
    }
    let alias = ALIASES.iter().find(|(alias, _)| alias.eq_ignore_ascii_case(name));
    Some(alias.map_or(name, |(_, model)| model).to_string())
}

/// The aliases and what they stand for, for `/advisor` to list.
pub fn aliases() -> &'static [(&'static str, &'static str)] {
    &ALIASES
}

/// Takes `--advisor MODEL` (or `--advisor=MODEL`) out of the command line:
/// the advisor for this run, `off` included, in place of the settings'.
pub fn parse_flag(args: Vec<String>) -> anyhow::Result<(Option<String>, Vec<String>)> {
    let mut flag = None;
    let mut rest = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--advisor" {
            let Some(model) = args.next() else { anyhow::bail!("--advisor needs a model (or off)") };
            flag = Some(model);
        } else if let Some(model) = arg.strip_prefix("--advisor=") {
            flag = Some(model.to_string());
        } else {
            rest.push(arg);
        }
    }
    Ok((flag, rest))
}

/// The models the API knows for the advisor tool, a longer name before one
/// it starts with (`opus-5-5` before `opus-5`).
const MODELS: [&str; 16] = [
    "fable-5-2", "mythos-5-2", "fable-5-1", "mythos-5-1", "fable-5", "mythos-5",
    "opus-5-5", "opus-5", "opus-4-8", "opus-4-7", "opus-4-6",
    "sonnet-5-5", "sonnet-5", "sonnet-4-6",
    "haiku-5-5", "haiku-4-5",
];

/// The Claude model `name` is, without its `claude-` start, also behind a
/// gateway's prefix (`anthropic/claude-…`, `us.anthropic.claude-…`) or with
/// a date or tag after it; `None` for a model the advisor tool does not
/// know.
fn known(name: &str) -> Option<&'static str> {
    let name = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
    let rest = &name[name.find("claude-")? + "claude-".len()..];
    // What may follow the name: nothing, a tag, a date, or a word; not a
    // version number (`opus-5` in `opus-5-5`).
    let ends = |after: &str| match after.strip_prefix('-') {
        Some(tail) => {
            let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
            digits == 0 || digits >= 8
        }
        None => after.is_empty() || after.starts_with(['@', '[', ':']),
    };
    MODELS.into_iter().find(|m| rest.strip_prefix(m).is_some_and(ends))
}

/// Whether the API takes `advisor` as the advisor of `executor`: the
/// advisor must be at least as capable. Unknown models are not paired.
pub fn accepts(executor: &str, advisor: &str) -> bool {
    let (Some(executor), Some(advisor)) = (known(executor), known(advisor)) else { return false };
    const TOP: [&str; 6] = ["mythos-5-2", "fable-5-2", "mythos-5-1", "fable-5-1", "mythos-5", "fable-5"];
    let allowed: &[&str] = match executor {
        "haiku-4-5" | "sonnet-4-6" => &[
            "opus-5-5", "opus-5", "opus-4-8", "opus-4-7", "opus-4-6", "sonnet-5-5", "sonnet-5", "haiku-5-5", "sonnet-4-6",
        ],
        "sonnet-5" | "haiku-5-5" => &["opus-5-5", "opus-5", "opus-4-8", "opus-4-7", "sonnet-5-5", "sonnet-5", "haiku-5-5"],
        "opus-4-6" => &["opus-5-5", "opus-5", "opus-4-8", "opus-4-7", "opus-4-6", "sonnet-5-5", "sonnet-5", "haiku-5-5"],
        "opus-4-7" | "opus-4-8" => &["opus-5-5", "opus-5", "opus-4-8", "opus-4-7", "sonnet-5-5"],
        "sonnet-5-5" => &["opus-5-5", "opus-5", "sonnet-5-5"],
        "opus-5-5" | "opus-5" | "fable-5" | "mythos-5" => &["opus-5-5", "opus-5"],
        "fable-5-1" | "mythos-5-1" => return TOP[..4].contains(&advisor),
        "fable-5-2" | "mythos-5-2" => return TOP[..2].contains(&advisor),
        _ => return false,
    };
    TOP.contains(&advisor) || allowed.contains(&advisor)
}

/// The tool definition a request carries.
pub fn tool(model: &str) -> Value {
    json!({"type": TOOL_TYPE, "name": TOOL_NAME, "model": model})
}

/// Whether `block` is the model's call to the advisor.
pub fn is_call(block: &Value) -> bool {
    block["type"] == "server_tool_use" && block["name"] == TOOL_NAME
}

/// Whether `block` is the advisor's answer.
pub fn is_result(block: &Value) -> bool {
    block["type"] == "advisor_tool_result"
}

/// What the advisor answered, from an `advisor_tool_result` block.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Advice in plain text (from older advisor models).
    Advice(String),
    /// Advice the executor reads but which comes back encrypted.
    Redacted,
    /// The consultation failed; the model goes on without advice.
    Unavailable(String),
}

impl Outcome {
    pub fn of(block: &Value) -> Self {
        let content = &block["content"];
        match content["type"].as_str().unwrap_or("") {
            "advisor_result" => Outcome::Advice(content["text"].as_str().unwrap_or("").to_string()),
            "advisor_redacted_result" => Outcome::Redacted,
            _ => Outcome::Unavailable(content["error_code"].as_str().unwrap_or("unknown").to_string()),
        }
    }

    /// One line on it, for the transcript.
    pub fn summary(&self) -> String {
        match self {
            Outcome::Advice(_) | Outcome::Redacted => "Advisor reviewed the conversation".into(),
            Outcome::Unavailable(code) => format!("Advisor unavailable ({code})"),
        }
    }
}

/// An assistant message's blocks as a request for another model takes
/// them: the advisor's blocks belong with the model that called it, so its
/// plain-text advice becomes text and the rest is left out.
pub fn for_other_model(blocks: Vec<Value>) -> Vec<Value> {
    blocks
        .into_iter()
        .filter_map(|block| match () {
            _ if is_call(&block) => None,
            _ if is_result(&block) => match Outcome::of(&block) {
                Outcome::Advice(text) if !text.is_empty() => {
                    Some(json!({"type": "text", "text": format!("(The advisor's guidance: {text})")}))
                }
                _ => None,
            },
            _ => Some(block),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_the_flag() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let (flag, rest) = parse_flag(args(&["--advisor", "opus", "fix it"])).unwrap();
        assert_eq!((flag.as_deref(), rest), (Some("opus"), args(&["fix it"])));
        assert_eq!(parse_flag(args(&["--advisor=off"])).unwrap().0.as_deref(), Some("off"));
        assert!(parse_flag(args(&["--advisor"])).is_err());
    }

    #[test]
    fn parses_aliases_and_off() {
        assert_eq!(parse("opus").as_deref(), Some("claude-opus-5-5"));
        assert_eq!(parse(" Fable ").as_deref(), Some("claude-fable-5-2"));
        assert_eq!(parse("claude-opus-4-8").as_deref(), Some("claude-opus-4-8"));
        assert_eq!(parse("off"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn knows_models_behind_prefixes_and_dates() {
        for (name, model) in [
            ("claude-opus-5-5", Some("opus-5-5")),
            ("claude-opus-5", Some("opus-5")),
            ("anthropic/claude-sonnet-5-5", Some("sonnet-5-5")),
            ("us.anthropic.claude-opus-4-8", Some("opus-4-8")),
            ("claude-haiku-4-5-20251001", Some("haiku-4-5")),
            ("claude-opus-5-5[1m]", Some("opus-5-5")),
            ("claude-fable-5-1", Some("fable-5-1")),
            ("claude-sonnet-4-5", None),
            ("gpt-5", None),
        ] {
            assert_eq!(known(name), model, "{name}");
        }
    }

    #[test]
    fn pairs_as_the_api_does() {
        assert!(accepts("claude-sonnet-5-5", "claude-opus-5-5"));
        assert!(accepts("claude-opus-5-5", "claude-opus-5-5"), "a second opinion of its own kind");
        assert!(accepts("claude-haiku-5-5", "claude-opus-4-8"));
        assert!(!accepts("claude-sonnet-5-5", "claude-opus-4-8"), "Sonnet 5.5 refuses Opus 4.8");
        assert!(!accepts("claude-opus-5-5", "claude-sonnet-5-5"), "less capable");
        assert!(!accepts("claude-fable-5-2", "claude-fable-5-1"));
        assert!(accepts("claude-fable-5-2", "claude-fable-5-2"));
        assert!(accepts("claude-opus-5", "claude-fable-5-2"));
        assert!(!accepts("claude-opus-5", "claude-haiku-4-5"), "Haiku 4.5 is never an advisor");
        assert!(!accepts("gpt-5", "claude-opus-5-5"), "an unknown model");
    }

    #[test]
    fn reads_outcomes() {
        let result = |content: Value| json!({"type": "advisor_tool_result", "tool_use_id": "s", "content": content});
        assert_eq!(Outcome::of(&result(json!({"type": "advisor_result", "text": "do X"}))), Outcome::Advice("do X".into()));
        assert_eq!(Outcome::of(&result(json!({"type": "advisor_redacted_result", "encrypted_content": "e"}))), Outcome::Redacted);
        let failed = Outcome::of(&result(json!({"type": "advisor_tool_result_error", "error_code": "overloaded"})));
        assert_eq!(failed.summary(), "Advisor unavailable (overloaded)");
    }

    #[test]
    fn other_models_get_plain_advice_as_text() {
        let blocks = vec![
            json!({"type": "text", "text": "a"}),
            json!({"type": "server_tool_use", "id": "s1", "name": "advisor", "input": {}}),
            json!({"type": "advisor_tool_result", "tool_use_id": "s1", "content": {"type": "advisor_result", "text": "check the lock"}}),
            json!({"type": "server_tool_use", "id": "s2", "name": "advisor", "input": {}}),
            json!({"type": "advisor_tool_result", "tool_use_id": "s2", "content": {"type": "advisor_redacted_result", "encrypted_content": "e"}}),
            json!({"type": "server_tool_use", "id": "w", "name": "web_search", "input": {}}),
        ];
        let kept = for_other_model(blocks.clone());
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[1]["text"], "(The advisor's guidance: check the lock)");
        assert_eq!(kept[2], blocks[5], "other server tools stay");
    }
}
