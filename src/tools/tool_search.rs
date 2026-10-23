//! `tool_search`: loads deferred tools when the model needs them.
//!
//! When their definitions would take much of the context, the tools that
//! may wait (MCP tools) are sent with `defer_loading`: the API keeps their
//! descriptions and schemas out of the context, and the model is told only
//! their names. It loads the ones it needs with this tool, which answers
//! with `tool_reference` blocks; the API puts the full definitions in their
//! place. The search itself runs here, as in Claude Code's ToolSearch: by
//! exact name (`select:a,b`), by a server's prefix, or by keywords scored
//! against the parts of the names and the descriptions.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolOutput, parse_input};
use crate::llm::ToolDef;

pub const NAME: &str = "tool_search";
/// Matches a search returns unless the model asks for another number.
const DEFAULT_RESULTS: usize = 5;

const DESCRIPTION: &str = "Loads deferred tools so they can be called. Deferred tools are listed by name only, in a \
<deferred-tools> message; their descriptions and input schemas stay out of the context until this tool loads them. \
A loaded tool is called like any other, for the rest of the conversation.

Query forms:
- \"select:mcp__gh__create_issue,mcp__gh__list_issues\": these tools, by exact name
- \"mcp__gh\": the tools whose names start so, such as one server's
- \"github issue\": keywords; the best matches by name and description, up to max_results
- \"+slack send\": names must contain \"slack\"; the other words rank them";

/// A deferred tool, ready to be scored.
struct Entry {
    name: String,
    /// Lowercased.
    description: String,
    /// Lowercased words of the name: `mcp__gh__listIssues` has `gh`,
    /// `list` and `issues`.
    words: Vec<String>,
    /// Lowercased segments between `__`: `gh` and `listissues`.
    segments: Vec<String>,
    mcp: bool,
}

impl Entry {
    fn new(tool: &ToolDef) -> Self {
        let mcp = tool.name.starts_with("mcp__");
        let rest = if mcp { &tool.name[5..] } else { &tool.name[..] };
        Self {
            name: tool.name.clone(),
            description: tool.description.to_lowercase(),
            words: split_words(rest),
            segments: rest.split("__").filter(|s| !s.is_empty()).map(|s| s.replace(['_', '-'], "").to_lowercase()).collect(),
            mcp,
        }
    }

    /// Whether the name or description has `term` at all.
    fn mentions(&self, term: &str) -> bool {
        self.words.iter().chain(&self.segments).any(|w| w.contains(term)) || has_word(&self.description, term)
    }

    /// Claude Code's weights: a whole word of the name counts most, MCP
    /// names a little more, the description least.
    fn score(&self, terms: &[&str]) -> u32 {
        let (exact, part, segment, segment_part) = if self.mcp { (12, 6, 12, 4) } else { (10, 5, 10, 3) };
        let mut score = 0;
        for term in terms {
            if self.words.iter().any(|w| w == term) {
                score += exact;
            } else if self.words.iter().any(|w| w.contains(term)) {
                score += part;
            }
            if self.segments.iter().any(|s| s == term) {
                score += segment;
            } else if self.segments.iter().any(|s| s.contains(term)) {
                score += segment_part;
            }
            if score == 0 && self.name.to_lowercase().contains(term) {
                score += 3;
            }
            if has_word(&self.description, term) {
                score += 2;
            }
        }
        score
    }
}

/// The words of a tool name: split at `_`, `-` and lower-to-upper case
/// changes, lowercased.
fn split_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if c == '_' || c == '-' {
            words.extend((!word.is_empty()).then(|| std::mem::take(&mut word)));
        } else {
            if c.is_uppercase() && prev_lower {
                words.push(std::mem::take(&mut word));
            }
            word.extend(c.to_lowercase());
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
    }
    words.extend((!word.is_empty()).then_some(word));
    words
}

/// Whether `text` has `term` as a whole word, or words.
fn has_word(text: &str, term: &str) -> bool {
    let inside = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    text.match_indices(term).any(|(at, _)| !inside(text[..at].chars().last()) && !inside(text[at + term.len()..].chars().next()))
}

pub struct ToolSearchTool {
    entries: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    query: String,
    max_results: Option<usize>,
}

impl ToolSearchTool {
    /// Searches `deferred`, the tools sent with `defer_loading`.
    pub fn new(deferred: &[ToolDef]) -> Self {
        Self { entries: deferred.iter().map(Entry::new).collect() }
    }

    /// The names of the tools `query` finds, best first, at most `max`.
    fn search(&self, query: &str, max: usize) -> Vec<&str> {
        let query = query.trim();
        let by_name = |name: &str| self.entries.iter().find(|e| e.name.eq_ignore_ascii_case(name.trim()));
        if let Some(names) = query.strip_prefix("select:") {
            let mut found: Vec<&str> = names.split(',').filter_map(by_name).map(|e| e.name.as_str()).collect();
            found.dedup();
            return found;
        }
        if let Some(entry) = by_name(query) {
            return vec![&entry.name];
        }
        let query = query.to_lowercase();
        if query.starts_with("mcp__") && query.len() > 5 {
            let found: Vec<&str> =
                self.entries.iter().filter(|e| e.name.to_lowercase().starts_with(&query)).map(|e| e.name.as_str()).take(max).collect();
            if !found.is_empty() {
                return found;
            }
        }
        let words: Vec<&str> = query.split_whitespace().collect();
        let required: Vec<&str> = words.iter().filter_map(|w| w.strip_prefix('+')).filter(|w| !w.is_empty()).collect();
        let terms: Vec<&str> = words.iter().map(|w| w.strip_prefix('+').unwrap_or(w)).filter(|w| !w.is_empty()).collect();
        let mut scored: Vec<(u32, &str)> = self
            .entries
            .iter()
            .filter(|e| required.iter().all(|r| e.mentions(r)))
            .map(|e| (e.score(&terms), e.name.as_str()))
            .filter(|(score, _)| *score > 0)
            .collect();
        // Stable: equal scores keep the tools' order.
        scored.sort_by_key(|&(score, _)| std::cmp::Reverse(score));
        scored.into_iter().take(max).map(|(_, name)| name).collect()
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "\"select:<name>[,<name>...]\" for tools by name, or keywords to search for"},
                "max_results": {"type": "integer", "minimum": 1, "description": format!("Most tools a keyword search loads (default {DEFAULT_RESULTS})")},
            },
            "required": ["query"],
            "additionalProperties": false,
        })
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(input) => input,
            Err(e) => return e,
        };
        let found = self.search(&input.query, input.max_results.unwrap_or(DEFAULT_RESULTS).max(1));
        if found.is_empty() {
            return ToolOutput::ok(format!(
                "No deferred tool matches \"{}\". Try other keywords, or a name from the <deferred-tools> list with \
                 \"select:\".",
                input.query
            ));
        }
        // The model gets the references; the text is for the user.
        let mut out = ToolOutput::ok(format!("Loaded {}", found.join(", ")));
        out.references = found.into_iter().map(String::from).collect();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, description: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: description.into(),
            input_schema: json!({"type": "object"}),
            eager_input_streaming: false,
            defer_loading: true,
        }
    }

    fn catalog() -> ToolSearchTool {
        ToolSearchTool::new(&[
            def("mcp__gh__create_issue", "Create a GitHub issue"),
            def("mcp__gh__listPullRequests", "List pull requests"),
            def("mcp__slack__send_message", "Send a message to a channel"),
            def("mcp__mail__send_email", "Send an email message"),
        ])
    }

    #[test]
    fn splits_names_into_words() {
        assert_eq!(split_words("gh__listPullRequests"), ["gh", "list", "pull", "requests"]);
        assert_eq!(split_words("send-email_v2"), ["send", "email", "v2"]);
        assert!(has_word("send an email.", "email") && !has_word("emails", "email"));
    }

    #[test]
    fn finds_tools_by_name_prefix_and_keywords() {
        let s = catalog();
        assert_eq!(s.search("select:mcp__gh__create_issue, MCP__SLACK__SEND_MESSAGE,nope", 5), ["mcp__gh__create_issue", "mcp__slack__send_message"]);
        assert_eq!(s.search("mcp__mail__send_email", 5), ["mcp__mail__send_email"]);
        assert_eq!(s.search("mcp__gh", 5), ["mcp__gh__create_issue", "mcp__gh__listPullRequests"]);
        assert_eq!(s.search("mcp__gh", 1), ["mcp__gh__create_issue"]);
        assert_eq!(s.search("pull requests", 5), ["mcp__gh__listPullRequests"]);
        assert_eq!(s.search("send", 5), ["mcp__slack__send_message", "mcp__mail__send_email"]);
        assert_eq!(s.search("+slack send", 5), ["mcp__slack__send_message"]);
        assert_eq!(s.search("message", 5)[0], "mcp__slack__send_message", "a word of the name beats the description");
        assert!(s.search("calendar", 5).is_empty());
    }

    #[tokio::test]
    async fn answers_with_references() {
        let out = catalog().run(json!({"query": "select:mcp__gh__create_issue"})).await;
        assert_eq!(out.references, ["mcp__gh__create_issue"]);
        assert_eq!(out.to_result_content(), json!([{"type": "tool_reference", "tool_name": "mcp__gh__create_issue"}]));
        let out = catalog().run(json!({"query": "calendar"})).await;
        assert!(out.references.is_empty() && !out.is_error);
        assert!(catalog().run(json!({"pattern": "x"})).await.is_error);
    }
}
