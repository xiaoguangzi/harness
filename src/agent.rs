//! The agent loop: send the conversation, run any requested tools, send the
//! results back, and repeat until the model ends its turn.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use futures::future::join_all;
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::llm::{Client, Message, StreamEvent, Usage};
use crate::subagent::Subagents;
use crate::tools::{ToolOutput, ToolRegistry};

/// Told to the model about a turn the user cancelled.
pub const INTERRUPTED: &str = "[Interrupted by the user]";

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
    /// The response broke off and is requested again after `wait`: what
    /// streamed since the last `on_request_start` is void, and output
    /// starts over. `attempt` counts from 1 to `llm::MAX_RETRIES`.
    fn on_retry(&mut self, _error: &anyhow::Error, _attempt: u32, _wait: Duration) {}
}

/// Messages for the model that arrive from elsewhere while it works or
/// waits: a background subagent's report for the main agent, a follow-up
/// for a subagent, a message the user typed while a turn ran. The loop
/// hands over what has arrived with its next request; a frontend starts a
/// turn for them when the agent is idle.
#[derive(Default)]
pub struct Inbox {
    queue: Mutex<Queue>,
    arrived: Notify,
    /// Called after each push that wants a turn.
    on_wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

#[derive(Default)]
struct Queue {
    entries: Vec<Entry>,
    next_id: u64,
}

struct Entry {
    id: u64,
    blocks: Vec<Value>,
    /// Whether it should start a turn of its own, rather than wait for the
    /// next one.
    wake: bool,
}

impl Inbox {
    /// Queues `text` for the model. With `wake`, an idle agent should run a
    /// turn for it; otherwise it goes along with the next one.
    pub fn push(&self, text: String, wake: bool) {
        self.push_content(vec![json!({"type": "text", "text": text})], wake);
    }

    /// Queues content blocks for the model, like [`Inbox::push`]; returns
    /// an id to follow them by.
    pub fn push_content(&self, blocks: Vec<Value>, wake: bool) -> u64 {
        let id = {
            let mut queue = self.queue.lock().unwrap();
            let id = queue.next_id;
            queue.next_id += 1;
            queue.entries.push(Entry { id, blocks, wake });
            id
        };
        if wake {
            self.arrived.notify_waiters();
            let hook = self.on_wake.lock().unwrap().clone();
            if let Some(hook) = hook {
                hook();
            }
        }
        id
    }

    /// Whether something waiting wants a turn.
    pub fn wants_turn(&self) -> bool {
        self.queue.lock().unwrap().entries.iter().any(|e| e.wake)
    }

    /// Whether what was pushed as `id` still waits, not yet taken.
    pub fn waiting(&self, id: u64) -> bool {
        self.queue.lock().unwrap().entries.iter().any(|e| e.id == id)
    }

    /// Takes back what was pushed as `id`, if it still waits.
    pub fn withdraw(&self, id: u64) -> bool {
        let mut queue = self.queue.lock().unwrap();
        let before = queue.entries.len();
        queue.entries.retain(|e| e.id != id);
        queue.entries.len() < before
    }

    pub fn clear(&self) {
        self.take();
    }

    /// Takes everything waiting.
    pub fn take(&self) -> Vec<Value> {
        let entries = std::mem::take(&mut self.queue.lock().unwrap().entries);
        entries.into_iter().flat_map(|e| e.blocks).collect()
    }

    /// Sets what to call when something arrives that wants a turn, such
    /// as starting one in a frontend.
    pub fn on_wake(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.on_wake.lock().unwrap() = Some(Arc::new(hook));
    }

    /// Resolves once something waiting wants a turn.
    pub async fn wanted(&self) {
        loop {
            let arrived = self.arrived.notified();
            tokio::pin!(arrived);
            arrived.as_mut().enable();
            if self.wants_turn() {
                return;
            }
            arrived.await;
        }
    }
}

pub struct Agent {
    client: Client,
    tools: ToolRegistry,
    system: String,
    messages: Vec<Message>,
    /// History length when the running turn started; still set on the next
    /// turn only if the previous one was cancelled (its future dropped).
    turn_start: Option<usize>,
    inbox: Arc<Inbox>,
    /// Subagents this conversation started, kept for follow-ups. Their
    /// background reports arrive in `inbox`.
    subagents: Arc<Subagents>,
}

impl Agent {
    pub fn new(client: Client, tools: ToolRegistry, system: impl Into<String>) -> Self {
        let inbox = Arc::new(Inbox::default());
        let subagents = Arc::new(Subagents::new(inbox.clone()));
        Self { client, tools, system: system.into(), messages: Vec::new(), turn_start: None, inbox, subagents }
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

    pub fn inbox(&self) -> &Arc<Inbox> {
        &self.inbox
    }

    pub fn subagents(&self) -> &Arc<Subagents> {
        &self.subagents
    }

    /// Forgets the conversation, stopping its subagents.
    pub fn clear(&mut self) {
        self.subagents.reset();
        self.inbox.clear();
        self.messages.clear();
        self.turn_start = None;
    }

    /// The conversation so far.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Replaces the conversation with a saved one. Subagents it mentions
    /// are gone; the current ones are stopped.
    pub fn restore(&mut self, messages: Vec<Message>) {
        self.clear();
        self.messages = messages;
    }

    /// Repairs the history after a cancelled turn now, rather than when the
    /// next turn starts, e.g. before saving it.
    pub fn settle(&mut self) {
        if self.turn_start.take().is_some() {
            self.repair_after_interrupt();
        }
    }

    /// Text of the last assistant message from index `since` on: a
    /// subagent's answer. Its text blocks are one run of text, possibly
    /// split mid-sentence.
    pub fn last_text(&self, since: usize) -> String {
        let written = self.messages.get(since..).unwrap_or_default();
        let Some(last) = written.iter().rev().find(|m| m.role == "assistant") else {
            return String::new();
        };
        let texts: Vec<&str> =
            last.content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
        texts.concat()
    }

    /// Runs one user turn to completion. `content` is the user message's
    /// content blocks (the prompt plus any attachments); what waits in the
    /// inbox goes first. Empty `content` runs a turn for the inbox alone,
    /// and does nothing if that is empty too. The history only ever grows
    /// by appending, so earlier turns (and their thinking blocks) stay
    /// intact.
    pub async fn run_turn(&mut self, content: Vec<Value>, observer: &mut dyn Observer) -> Result<()> {
        self.settle();
        let arrived = self.inbox.take();
        if content.is_empty() && arrived.is_empty() {
            return Ok(());
        }
        let turn_start = self.messages.len();
        self.turn_start = Some(turn_start);
        self.messages.push(Message::user(arrived.iter().cloned().chain(content).collect()));
        let (subagents, client) = (self.subagents.clone(), self.client.clone());
        let result = crate::subagent::scope(subagents, client, self.run_loop(observer)).await;
        if result.is_err() {
            self.repair_after_error(turn_start);
            // A turn that got nowhere is removed, but what had arrived for
            // the model stays: it can't be sent again.
            if self.messages.len() == turn_start && !arrived.is_empty() {
                self.messages.push(Message::user(arrived));
            }
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
            // What arrived meanwhile goes after the prompt or the tool results.
            let arrived = self.inbox.take();
            if !arrived.is_empty() {
                match self.messages.last_mut() {
                    Some(last) if last.role == "user" => last.content.extend(arrived),
                    _ => self.messages.push(Message::user(arrived)),
                }
            }
            observer.on_request_start();
            let mut on_event = |event: StreamEvent| match event {
                StreamEvent::TextDelta(text) => observer.on_text_delta(text),
                StreamEvent::TextEnd => observer.on_text_end(),
                StreamEvent::ThinkingDelta(text) => observer.on_thinking_delta(text),
                StreamEvent::ToolInputStart(name) => observer.on_tool_input_start(name),
                StreamEvent::ToolInputDelta(json) => observer.on_tool_input_delta(json),
                StreamEvent::PartialUsage(usage) => observer.on_usage(&usage),
                StreamEvent::Retry { error, attempt, wait } => observer.on_retry(error, attempt, wait),
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
                // Something arrived while the model was answering: it
                // carries on with that rather than end the turn.
                _ if self.inbox.wants_turn() => continue,
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
            "model": "gw/m",
            "providers": {"gw": {"protocol": "anthropic", "base_url": "http://127.0.0.1:9", "api_key": "k"}}
        }));
        let client = Client::new(Providers::from_parts(&file, &no_env).unwrap()).unwrap();
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
