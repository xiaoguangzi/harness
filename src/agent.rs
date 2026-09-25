//! The agent loop: send the conversation, run any requested tools, send the
//! results back, and repeat until the model ends its turn.

use anyhow::{Result, bail};
use futures::future::join_all;
use serde_json::{Value, json};

use crate::llm::{Client, Message, StreamEvent, Usage};
use crate::tools::{ToolOutput, ToolRegistry};

/// Told to the model about a turn the user cancelled.
const INTERRUPTED: &str = "[Interrupted by the user]";

/// Receives progress events so the frontend decides how to display them.
pub trait Observer: Send {
    /// A fragment of assistant text, as it streams in.
    fn on_text_delta(&mut self, text: &str);
    /// The current text block is complete.
    fn on_text_end(&mut self);
    /// A request to the model is about to be sent.
    fn on_request_start(&mut self) {}
    /// A fragment of the model's reasoning; empty when reasoning starts
    /// without visible text.
    fn on_thinking_delta(&mut self, _text: &str) {}
    /// The model started writing a call to `name`; its JSON input follows
    /// in fragments.
    fn on_tool_input_start(&mut self, _name: &str) {}
    fn on_tool_input_delta(&mut self, _json: &str) {}
    fn on_tool_call(&mut self, name: &str, input: &Value);
    fn on_tool_result(&mut self, name: &str, output: &ToolOutput);
    /// Token counts of a request: a finished one, or one that failed
    /// partway (counted as far as its stream got).
    fn on_usage(&mut self, _usage: &Usage) {}
}

pub struct Agent {
    client: Client,
    tools: ToolRegistry,
    system: String,
    messages: Vec<Message>,
    /// History length when the running turn started; still set on the next
    /// turn only if the previous one was cancelled (its future dropped).
    turn_start: Option<usize>,
}

impl Agent {
    pub fn new(client: Client, tools: ToolRegistry, system: impl Into<String>) -> Self {
        Self { client, tools, system: system.into(), messages: Vec::new(), turn_start: None }
    }

    /// The `provider/model` in use.
    pub fn model(&self) -> &str {
        self.client.spec()
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Switches models; the conversation continues on the new one.
    pub fn set_client(&mut self, client: Client) {
        self.client = client;
    }

    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Replaces the tool set, e.g. after the providers were reloaded.
    pub fn set_tools(&mut self, tools: ToolRegistry) {
        self.tools = tools;
    }

    /// Forgets the conversation.
    pub fn clear(&mut self) {
        self.messages.clear();
        self.turn_start = None;
    }

    /// Text of the last assistant message: a subagent's answer. Its text
    /// blocks are one run of text, possibly split mid-sentence.
    pub fn last_text(&self) -> String {
        let Some(last) = self.messages.iter().rev().find(|m| m.role == "assistant") else {
            return String::new();
        };
        let texts: Vec<&str> =
            last.content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
        texts.concat()
    }

    /// Runs one user turn to completion. `content` is the user message's
    /// content blocks (the prompt plus any attachments). The history only
    /// ever grows by appending, so earlier turns (and their thinking blocks)
    /// stay intact.
    pub async fn run_turn(&mut self, content: Vec<Value>, observer: &mut dyn Observer) -> Result<()> {
        if self.turn_start.take().is_some() {
            self.repair_after_interrupt();
        }
        let turn_start = self.messages.len();
        self.turn_start = Some(turn_start);
        self.messages.push(Message::user(content));
        let result = self.run_loop(observer).await;
        if result.is_err() {
            self.repair_after_error(turn_start);
        }
        self.turn_start = None;
        result
    }

    /// Leaves the history valid after a failed turn. Tool calls that never
    /// got results are dropped; tool rounds that did run are kept, so the
    /// model knows about their side effects. A turn that got nowhere is
    /// removed entirely, so the user can simply retry.
    fn repair_after_error(&mut self, turn_start: usize) {
        let unanswered = self
            .messages
            .last()
            .is_some_and(|m| m.role == "assistant" && m.content.iter().any(|b| b["type"] == "tool_use"));
        if unanswered {
            self.messages.pop();
        }
        if self.messages.len() == turn_start + 1 {
            self.messages.truncate(turn_start);
        }
    }

    /// Leaves the history valid after a cancelled turn, keeping what the
    /// user asked for so the model knows the request was cut short. Tool
    /// calls that never finished get error results saying so.
    fn repair_after_interrupt(&mut self) {
        let Some(last) = self.messages.last_mut() else { return };
        let note = json!({"type": "text", "text": INTERRUPTED});
        if last.role == "user" {
            last.content.push(note);
            return;
        }
        let mut results: Vec<Value> = last
            .content
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .map(|t| {
                json!({"type": "tool_result", "tool_use_id": t["id"], "content": INTERRUPTED, "is_error": true})
            })
            .collect();
        if !results.is_empty() {
            results.push(note);
            self.messages.push(Message::user(results));
        }
    }

    async fn run_loop(&mut self, observer: &mut dyn Observer) -> Result<()> {
        let definitions = self.tools.definitions();

        loop {
            observer.on_request_start();
            let mut on_event = |event: StreamEvent| match event {
                StreamEvent::TextDelta(text) => observer.on_text_delta(text),
                StreamEvent::TextEnd => observer.on_text_end(),
                StreamEvent::ThinkingDelta(text) => observer.on_thinking_delta(text),
                StreamEvent::ToolInputStart(name) => observer.on_tool_input_start(name),
                StreamEvent::ToolInputDelta(json) => observer.on_tool_input_delta(json),
                StreamEvent::PartialUsage(usage) => observer.on_usage(&usage),
            };
            let response =
                self.client.create(&self.system, &definitions, &self.messages, &mut on_event).await?;
            if let Some(usage) = &response.usage {
                observer.on_usage(usage);
            }
            let stop_reason = response.stop_reason.as_deref().unwrap_or("");

            if stop_reason == "refusal" {
                let detail = response.stop_details.map(|d| d.to_string()).unwrap_or_default();
                bail!("the model declined this request {detail}");
            }

            let content = response.content;
            let tool_uses: Vec<Value> =
                content.iter().filter(|b| b["type"] == "tool_use").cloned().collect();
            self.messages.push(Message { role: "assistant", content, source: Some(self.client.spec().to_string()) });

            match stop_reason {
                "tool_use" => {}
                "max_tokens" if !tool_uses.is_empty() => {
                    // A tool_use block cut off by max_tokens may carry partial
                    // input; don't execute it.
                    bail!("response hit max_tokens while writing a tool call; raise HARNESS_MAX_TOKENS");
                }
                "max_tokens" => {
                    observer.on_text_delta("[stopped: max_tokens reached]");
                    observer.on_text_end();
                    return Ok(());
                }
                _ => return Ok(()),
            }

            for t in &tool_uses {
                observer.on_tool_call(t["name"].as_str().unwrap_or(""), &t["input"]);
            }
            // Run all tool calls from this response concurrently, then send
            // every result back in a single user message. A call whose
            // streamed input was not valid JSON is not run; the model gets
            // the raw text back so it can retry.
            let invalid = &response.invalid_tool_inputs;
            let outputs = join_all(tool_uses.iter().map(|t| async {
                let id = t["id"].as_str().unwrap_or("");
                match invalid.get(id) {
                    Some(raw) => ToolOutput::error(json!({"INVALID_JSON": raw}).to_string()),
                    None => self.tools.run(t["name"].as_str().unwrap_or(""), t["input"].clone()).await,
                }
            }))
            .await;

            let mut results = Vec::with_capacity(outputs.len());
            for (t, out) in tool_uses.iter().zip(outputs) {
                observer.on_tool_result(t["name"].as_str().unwrap_or(""), &out);
                results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": t["id"],
                    "content": out.to_result_content(),
                    "is_error": out.is_error,
                }));
            }
            self.messages.push(Message::user(results));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Providers;
    use crate::llm::config::tests::{file, no_env};

    fn agent(messages: Vec<Message>) -> Agent {
        let file = file(json!({
            "default_model": "gw/m",
            "providers": {"gw": {"protocol": "anthropic", "base_url": "http://127.0.0.1:9", "api_key": "k"}}
        }));
        let client = Client::new(Providers::from_parts(vec![file], &no_env).unwrap()).unwrap();
        let mut agent = Agent::new(client, ToolRegistry::default(), "sys");
        agent.messages = messages;
        agent
    }

    fn text(t: &str) -> Value {
        json!({"type": "text", "text": t})
    }

    #[test]
    fn an_interrupted_request_is_kept_with_a_note() {
        let mut a = agent(vec![Message::user(vec![text("do it")])]);
        a.repair_after_interrupt();
        assert_eq!(a.messages.len(), 1);
        assert_eq!(a.messages[0].content, vec![text("do it"), text(INTERRUPTED)]);
    }

    #[test]
    fn interrupted_tool_calls_get_error_results() {
        let call = json!({"type": "tool_use", "id": "t1", "name": "bash", "input": {}});
        let mut a = agent(vec![
            Message::user(vec![text("run it")]),
            Message { role: "assistant", content: vec![text("ok"), call], source: None },
        ]);
        a.repair_after_interrupt();
        assert_eq!(a.messages.len(), 3);
        assert_eq!(
            a.messages[2].content,
            vec![
                json!({"type": "tool_result", "tool_use_id": "t1", "content": INTERRUPTED, "is_error": true}),
                text(INTERRUPTED),
            ]
        );
    }
}
