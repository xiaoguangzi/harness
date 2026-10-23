//! Deferring tools behind `tool_search`: whether a conversation does, and
//! what the model is told about the tools it can load.
//!
//! A conversation decides once, before its first request: it defers the
//! tools that may be deferred (MCP tools) when the provider can, and their
//! definitions would take at least a tenth of the context window. The
//! decision holds for the rest of the conversation, so the tool list, and
//! with it the prompt cache, stays as it is. The model is told the deferred
//! tools' names in a `<deferred-tools>` message, again whenever they change
//! and after a compaction; which ones it was told about last is read back
//! from the history.

use serde_json::{Value, json};

use super::Agent;
use crate::llm::{Message, ToolDef};
use crate::tools::tool_search::{self, ToolSearchTool};

pub(super) const NOTICE_START: &str = "<deferred-tools>";
const NOTICE_END: &str = "</deferred-tools>";
/// Tools are deferred once their definitions would take this share of the
/// context window.
const MIN_PERCENT: u64 = 10;

const INTRO: &str = "These tools are deferred: only their names are here, not their descriptions or input schemas. \
Load the ones you need with tool_search before calling them, by name (\"select:<name>[,<name>...]\") or by keywords.";
const CHANGED: &str = "The deferred tools changed; these are the ones there are now. Load the ones you need with \
tool_search before calling them, by name (\"select:<name>[,<name>...]\") or by keywords.";

impl Agent {
    /// Registers `tool_search` with the deferred tools if this conversation
    /// defers them, deciding so before its first request. Returns what the
    /// model is to be told about them, if it was not told yet; `loaded`
    /// are tools it loaded before a compaction, to load again.
    pub(super) fn sync_tool_search(&mut self, loaded: &[String]) -> Option<Value> {
        self.tools.retain(|t| t.name() != tool_search::NAME);
        let deferred = self.tools.deferrable();
        let told = told(&self.messages);
        let (client, window) = (&self.client, self.context_window());
        let on = *self
            .tool_search
            .get_or_insert_with(|| told.is_some() || (client.tool_search() && !deferred.is_empty() && worth_deferring(&deferred, window)));
        if !on {
            return None;
        }
        if !deferred.is_empty() {
            self.tools.register(ToolSearchTool::new(&deferred));
        }
        let names: Vec<&str> = deferred.iter().map(|d| d.name.as_str()).collect();
        match &told {
            Some(told) if told.iter().eq(&names) => None,
            None if names.is_empty() => None,
            _ => Some(notice(if told.is_some() { CHANGED } else { INTRO }, &names, loaded)),
        }
    }
}

/// Whether the definitions of `deferred` would take enough of a context
/// window of `window` tokens to defer them.
fn worth_deferring(deferred: &[ToolDef], window: u64) -> bool {
    let bytes: usize =
        deferred.iter().map(|t| t.name.len() + t.description.len() + t.input_schema.to_string().len()).sum();
    // About four bytes a token.
    bytes as u64 / 4 >= window / 100 * MIN_PERCENT
}

fn notice(intro: &str, names: &[&str], loaded: &[String]) -> Value {
    let mut text = format!("{NOTICE_START}\n{intro}");
    if !loaded.is_empty() {
        text.push_str(&format!(
            "\n\nYou loaded these before the conversation was compacted; load them again to call them: {}.",
            loaded.join(", ")
        ));
    }
    if !names.is_empty() {
        text.push_str(&format!("\n\n{}", names.join("\n")));
    }
    json!({"type": "text", "text": format!("{text}\n{NOTICE_END}")})
}

/// The deferred tools the model was last told about in `messages`, if any.
fn told(messages: &[Message]) -> Option<Vec<String>> {
    let text = messages
        .iter()
        .rev()
        .filter(|m| m.role == "user")
        .flat_map(|m| m.content.iter().rev())
        .filter_map(|b| b["text"].as_str())
        .find_map(|t| t.strip_prefix(NOTICE_START))?;
    let body = text.split(NOTICE_END).next().unwrap_or_default();
    // The names are the last paragraph, one per line; the intro has spaces.
    let last = body.rsplit("\n\n").next().unwrap_or_default();
    Some(last.lines().map(str::trim).filter(|l| !l.is_empty() && !l.contains(char::is_whitespace)).map(String::from).collect())
}

/// The tools `tool_search` loaded in `messages`.
pub(super) fn loaded(messages: &[Message]) -> Vec<String> {
    let mut names: Vec<String> = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|b| b["type"] == "tool_result")
        .filter_map(|b| b["content"].as_array())
        .flatten()
        .filter(|b| b["type"] == "tool_reference")
        .filter_map(|b| b["tool_name"].as_str().map(String::from))
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_back_what_the_model_was_told() {
        let message = |block: Value| Message::user(vec![json!({"type": "text", "text": "hi"}), block]);
        assert_eq!(told(&[message(json!({"type": "text", "text": "x"}))]), None);
        let first = notice(INTRO, &["mcp__a__x", "mcp__b__y"], &["mcp__a__x".into()]);
        assert_eq!(told(&[message(first.clone())]).unwrap(), ["mcp__a__x", "mcp__b__y"]);
        let none = notice(CHANGED, &[], &[]);
        assert_eq!(told(&[message(first), message(none)]).unwrap(), Vec::<String>::new(), "the latest counts");
    }

    #[test]
    fn defers_when_the_definitions_are_large() {
        let def = |bytes: usize| ToolDef {
            name: "mcp__a__x".into(),
            description: "d".repeat(bytes),
            input_schema: json!({}),
            eager_input_streaming: false,
            defer_loading: true,
        };
        assert!(!worth_deferring(&[def(30_000)], 200_000), "about 7.5k tokens of 200k");
        assert!(worth_deferring(&[def(30_000), def(60_000)], 200_000), "about 22.5k");
    }

    struct Big;

    #[async_trait::async_trait]
    impl crate::tools::Tool for Big {
        fn name(&self) -> &str {
            "mcp__big__thing"
        }
        fn description(&self) -> &str {
            static LONG: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| "does a thing. ".repeat(400));
            &LONG
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn deferred(&self) -> bool {
            true
        }
        async fn run(&self, _: Value) -> crate::tools::ToolOutput {
            crate::tools::ToolOutput::ok("did it")
        }
    }

    struct Quiet;

    impl super::super::Observer for Quiet {
        fn on_text_delta(&mut self, _: &str) {}
        fn on_text_end(&mut self) {}
        fn on_tool_call(&mut self, _: &str, _: &Value) {}
        fn on_tool_result(&mut self, _: &str, _: &crate::tools::ToolOutput) {}
    }

    #[tokio::test]
    async fn the_model_loads_deferred_tools_with_tool_search() {
        let (client, mut requests) = crate::agent::mock::gateway_with(json!({"tool_search": true}), |n| match n {
            0 => (vec![json!({"type": "tool_use", "id": "t1", "name": "tool_search", "input": {"query": "big thing"}})], "tool_use", 100),
            1 => (vec![json!({"type": "tool_use", "id": "t2", "name": "mcp__big__thing", "input": {}})], "tool_use", 100),
            _ => (vec![json!({"type": "text", "text": "done"})], "end_turn", 100),
        })
        .await;
        let mut tools = crate::tools::ToolRegistry::new();
        tools.register(Big);
        let mut agent = Agent::new(client, tools, "sys");
        agent.run_turn(vec![json!({"type": "text", "text": "go"})], &mut Quiet).await.unwrap();

        let first = requests.recv().await.unwrap();
        let sent: Vec<(&str, bool)> =
            first["tools"].as_array().unwrap().iter().map(|t| (t["name"].as_str().unwrap(), t["defer_loading"] == true)).collect();
        assert_eq!(sent, [("mcp__big__thing", true), ("tool_search", false)]);
        let texts = crate::agent::mock::texts(&first["messages"][0]);
        assert!(texts[0].starts_with(NOTICE_START) && texts[0].ends_with("\n\nmcp__big__thing\n</deferred-tools>"), "{texts:?}");
        assert_eq!(texts[1], "go");

        let second = requests.recv().await.unwrap();
        let result = &second["messages"][2]["content"][0];
        assert_eq!(result["content"], json!([{"type": "tool_reference", "tool_name": "mcp__big__thing"}]));
        let third = requests.recv().await.unwrap();
        assert_eq!(third["messages"][4]["content"][0]["content"], "did it");

        agent.run_turn(vec![json!({"type": "text", "text": "again"})], &mut Quiet).await.unwrap();
        let next = requests.recv().await.unwrap();
        assert_eq!(crate::agent::mock::texts(next["messages"].as_array().unwrap().last().unwrap()), ["again"], "told once");

        agent.compact(None, &mut Quiet).await.unwrap();
        let summarize = requests.recv().await.unwrap();
        assert_eq!(summarize["tools"], first["tools"], "the same tools, so the cache still applies");
        let notice = agent.messages[0].content.last().unwrap()["text"].as_str().unwrap();
        assert!(notice.starts_with(NOTICE_START) && notice.contains("load them again to call them: mcp__big__thing."), "{notice}");
        assert_eq!(told(&agent.messages).unwrap(), ["mcp__big__thing"]);

        agent.clear();
        let mut few = crate::tools::ToolRegistry::new();
        few.register(crate::tools::ReadTool::new(Default::default()));
        agent.set_tools(few);
        agent.run_turn(vec![json!({"type": "text", "text": "small"})], &mut Quiet).await.unwrap();
        let small = requests.recv().await.unwrap();
        assert_eq!(small["tools"].as_array().unwrap().len(), 1, "nothing to defer, no tool_search");
        assert_eq!(crate::agent::mock::texts(&small["messages"][0]), ["small"]);
    }
}
