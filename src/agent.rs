//! The agent loop: send the conversation, run any requested tools, send the
//! results back, and repeat until the model ends its turn.

mod compact;
mod usage;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use futures::future::join_all;
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::hooks::{self, Event, Hooks, Outcome};
use crate::llm::{Client, Message, StreamEvent, Usage};
use crate::subagent::Subagents;
use crate::tools::shells::Shells;
use crate::tools::{ToolOutput, ToolRegistry};

pub use compact::is_compacted;
#[cfg(test)]
pub(crate) use compact::tests as mock;
pub use usage::UsageLog;

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
    /// The conversation is being summarized to make room in the context.
    fn on_compact_start(&mut self) {}
    /// The summary replaced the history: the context went from `before` to
    /// about `after` tokens.
    fn on_compact_end(&mut self, _before: u64, _after: u64) {}
    /// A hook has something for the user: its `systemMessage`, what it
    /// blocked, or how it failed.
    fn on_hook(&mut self, _message: &str) {}
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
    /// Commands `bash` runs in the background for this conversation.
    shells: Arc<Shells>,
    /// Tokens used so far, shared with the subagents.
    usage: Arc<UsageLog>,
    /// The history length after the last response, and the tokens that
    /// response reported for everything up to it; `None` once the history
    /// changed otherwise.
    context: Option<(usize, u64)>,
    /// Compactions in the running turn.
    compactions: u32,
    /// Whether a compaction in the running turn took in its prompt.
    turn_compacted: bool,
    hooks: Hooks,
    /// Why the next request starts a session for the SessionStart hooks
    /// (`startup`, `clear`, `resume`, `compact`), if it does; for a
    /// subagent, `startup` runs the SubagentStart hooks.
    session_start: Option<&'static str>,
    /// Whether SessionStart hooks ran for this conversation, so SessionEnd
    /// hooks run when it ends.
    in_session: bool,
}

impl Agent {
    pub fn new(client: Client, tools: ToolRegistry, system: impl Into<String>) -> Self {
        let inbox = Arc::new(Inbox::default());
        let usage = Arc::new(UsageLog::default());
        let mut hooks = Hooks::default();
        hooks.set_client(client.clone());
        let subagents = Arc::new(Subagents::new(inbox.clone(), usage.clone(), hooks.clone()));
        let shells = Arc::new(Shells::new(inbox.clone()));
        Self {
            client,
            tools,
            system: system.into(),
            messages: Vec::new(),
            turn_start: None,
            inbox,
            subagents,
            shells,
            usage,
            context: None,
            compactions: 0,
            turn_compacted: false,
            hooks,
            session_start: Some("startup"),
            in_session: false,
        }
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
        self.hooks.set_client(client.clone());
        self.client = client;
    }

    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Replaces the tool set, e.g. after the providers were reloaded.
    pub fn set_tools(&mut self, tools: ToolRegistry) {
        self.tools = tools;
    }

    /// Replaces the system prompt, for a new conversation after the
    /// plugins were reloaded.
    pub fn set_system(&mut self, system: String) {
        self.system = system;
    }

    pub fn inbox(&self) -> &Arc<Inbox> {
        &self.inbox
    }

    pub fn subagents(&self) -> &Arc<Subagents> {
        &self.subagents
    }

    pub fn shells(&self) -> &Arc<Shells> {
        &self.shells
    }

    /// The hooks this conversation runs.
    pub fn hooks(&self) -> &Hooks {
        &self.hooks
    }

    /// Runs `hooks` instead, e.g. a subagent's; its subagents get them too.
    pub fn set_hooks(&mut self, mut hooks: Hooks) {
        hooks.set_client(self.client.clone());
        self.subagents.set_hooks(hooks.clone());
        self.hooks = hooks;
    }

    /// The tokens this conversation and its subagents used so far.
    pub fn usage(&self) -> &Arc<UsageLog> {
        &self.usage
    }

    /// Counts this agent's requests in `log` too, as a subagent's are in
    /// its parent's.
    pub fn set_usage(&mut self, log: Arc<UsageLog>) {
        self.usage = log;
    }

    /// About how many tokens the next request would take of the context
    /// window.
    pub fn context_tokens(&self) -> u64 {
        self.context_size(&self.tools.definitions())
    }

    /// Forgets the conversation, stopping its subagents and background
    /// commands.
    pub fn clear(&mut self) {
        self.subagents.reset();
        self.shells.reset();
        self.inbox.clear();
        self.messages.clear();
        self.turn_start = None;
        self.context = None;
        self.session_start = Some("clear");
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
        self.session_start = Some("resume");
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
    /// intact, except that a compaction replaces it when the context fills
    /// up (see `compact`).
    pub async fn run_turn(&mut self, mut content: Vec<Value>, observer: &mut dyn Observer) -> Result<()> {
        self.settle();
        if !content.is_empty() && !self.hooks.is_subagent() {
            self.prompt_hooks(&mut content, observer).await?;
        }
        let arrived = self.inbox.take();
        if content.is_empty() && arrived.is_empty() {
            return Ok(());
        }
        let session_start = self.session_start;
        self.turn_start = Some(self.messages.len());
        (self.compactions, self.turn_compacted) = (0, false);
        self.messages.push(Message::user(arrived.iter().cloned().chain(content).collect()));
        let (subagents, client, shells) = (self.subagents.clone(), self.client.clone(), self.shells.clone());
        let nested = self.hooks.is_subagent();
        let turn = crate::subagent::scope(subagents, client, nested, self.run_loop(observer));
        let result = crate::tools::shells::scope(shells, turn).await;
        if result.is_err() {
            // A compaction moves the turn's start.
            let turn_start = self.turn_start.unwrap_or_default();
            self.repair_after_error(turn_start);
            // A turn that got nowhere is removed, but what had arrived for
            // the model stays: it can't be sent again.
            if self.messages.len() == turn_start && !arrived.is_empty() && !self.turn_compacted {
                self.messages.push(Message::user(arrived));
            }
            // What SessionStart hooks added went with the turn.
            if self.messages.len() <= turn_start {
                self.session_start = self.session_start.or(session_start);
            }
        }
        self.turn_start = None;
        result
    }

    /// Runs the UserPromptSubmit hooks on the prompt in `content` (its last
    /// text block): they may refuse it, or add to it.
    async fn prompt_hooks(&mut self, content: &mut Vec<Value>, observer: &mut dyn Observer) -> Result<()> {
        let prompt = content.iter().rev().find(|b| b["type"] == "text").and_then(|b| b["text"].as_str());
        let outcome = self.hooks.user_prompt_submit(prompt.unwrap_or_default(), self.client.spec()).await;
        show_notices(&outcome, observer);
        if let Some(reason) = outcome.block.or(outcome.halt) {
            let reason = if reason.is_empty() { String::new() } else { format!(": {reason}") };
            bail!("a UserPromptSubmit hook blocked the prompt{reason}");
        }
        let added = outcome.context.iter().map(|c| hooks::feedback(Event::UserPromptSubmit, c));
        content.extend(added.map(|text| json!({"type": "text", "text": text})));
        Ok(())
    }

    /// Runs the SessionStart hooks if a session starts with this request,
    /// or a subagent's SubagentStart hooks before its first one; what they
    /// add goes with it.
    async fn session_hooks(&mut self, observer: &mut dyn Observer) {
        let Some(source) = self.session_start.take() else { return };
        let (event, outcome) = if self.hooks.is_subagent() {
            if source != "startup" {
                return;
            }
            (Event::SubagentStart, self.hooks.subagent_start(self.client.spec()).await)
        } else {
            self.in_session = true;
            (Event::SessionStart, self.hooks.session_start(source, self.client.spec()).await)
        };
        show_notices(&outcome, observer);
        if let Some(reason) = &outcome.block {
            observer.on_hook(&format!("{} hook: {reason}", event.as_str()));
        }
        let added: Vec<Value> =
            outcome.context.iter().map(|c| json!({"type": "text", "text": hooks::feedback(event, c)})).collect();
        if added.is_empty() {
            return;
        }
        match self.messages.last_mut() {
            Some(last) if last.role == "user" => last.content.extend(added),
            _ => self.messages.push(Message::user(added)),
        }
    }

    /// Runs the SessionEnd hooks for `reason` if this conversation started a
    /// session, before it is cleared, replaced or dropped; returns what
    /// they have for the user.
    pub async fn end_session(&mut self, reason: &str) -> Vec<String> {
        if !std::mem::take(&mut self.in_session) {
            return Vec::new();
        }
        let outcome = self.hooks.session_end(reason, self.client.spec()).await;
        let mut notices = outcome.notices;
        notices.extend(outcome.block.map(|reason| format!("SessionEnd hook: {reason}")));
        notices
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
        self.context = None;
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
        // Whether a Stop hook already kept this turn going.
        let mut stop_hook_active = false;

        loop {
            // What arrived meanwhile goes after the prompt or the tool results.
            let arrived = self.inbox.take();
            if !arrived.is_empty() {
                match self.messages.last_mut() {
                    Some(last) if last.role == "user" => last.content.extend(arrived),
                    _ => self.messages.push(Message::user(arrived)),
                }
            }
            self.compact_if_full(&definitions, observer).await?;
            self.session_hooks(observer).await;
            observer.on_request_start();
            let (log, model) = (&self.usage, self.client.spec());
            let mut on_event = |event: StreamEvent| match event {
                StreamEvent::TextDelta(text) => observer.on_text_delta(text),
                StreamEvent::TextEnd => observer.on_text_end(),
                StreamEvent::ThinkingDelta(text) => observer.on_thinking_delta(text),
                StreamEvent::ToolInputStart(name) => observer.on_tool_input_start(name),
                StreamEvent::ToolInputDelta(json) => observer.on_tool_input_delta(json),
                StreamEvent::PartialUsage(usage) => {
                    log.record(model, &usage);
                    observer.on_usage(&usage);
                }
                StreamEvent::Retry { error, attempt, wait } => observer.on_retry(error, attempt, wait),
            };
            let response =
                self.client.create(&self.system, &definitions, &self.messages, &mut on_event).await?;
            if let Some(usage) = &response.usage {
                self.usage.record(self.client.spec(), usage);
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
            if let Some(usage) = &response.usage {
                self.context = Some((self.messages.len(), usage.input_tokens + usage.output_tokens));
            }

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
                _ => {
                    // A Stop hook may send the model back to work.
                    let last = self.last_text(self.messages.len() - 1);
                    let outcome = self.hooks.stop(stop_hook_active, &last, self.client.spec()).await;
                    show_notices(&outcome, observer);
                    let event = self.hooks.stop_event();
                    match (outcome.halt, outcome.block) {
                        (None, Some(reason)) => {
                            observer.on_hook(&format!("{} hook: {reason}", event.as_str()));
                            let feedback = json!({"type": "text", "text": hooks::feedback(event, &reason)});
                            self.messages.push(Message::user(vec![feedback]));
                            stop_hook_active = true;
                            continue;
                        }
                        _ => return Ok(()),
                    }
                }
            }

            // PreToolUse hooks see every call first; they may refuse one or
            // change its input.
            let invalid = &response.invalid_tool_inputs;
            let (tools, hooks, model) = (&self.tools, &self.hooks, self.client.spec());
            let checks = join_all(tool_uses.iter().map(|t| async {
                let id = t["id"].as_str().unwrap_or("");
                if invalid.contains_key(id) {
                    return Outcome::default();
                }
                hooks.pre_tool_use(t["name"].as_str().unwrap_or(""), id, &t["input"], model).await
            }))
            .await;
            for (t, check) in tool_uses.iter().zip(&checks) {
                let input = check.updated_input.as_ref().unwrap_or(&t["input"]);
                observer.on_tool_call(t["name"].as_str().unwrap_or(""), input);
                show_notices(check, observer);
            }
            // Run all tool calls from this response concurrently, then send
            // every result back in a single user message. A call whose
            // streamed input was not valid JSON is not run; the model gets
            // the raw text back so it can retry.
            let outputs = join_all(tool_uses.iter().zip(checks).map(|(t, check)| async move {
                let (id, name) = (t["id"].as_str().unwrap_or(""), t["name"].as_str().unwrap_or(""));
                if let Some(raw) = invalid.get(id) {
                    return (ToolOutput::error(json!({"INVALID_JSON": raw}).to_string()), check, None);
                }
                if let Some(reason) = &check.block {
                    let text = hooks::feedback(Event::PreToolUse, &format!("The call was blocked: {reason}"));
                    return (ToolOutput::error(text), check, None);
                }
                if check.halt.is_some() {
                    return (ToolOutput::error("The call was not run: a hook stopped the turn"), check, None);
                }
                let input = check.updated_input.clone().unwrap_or_else(|| t["input"].clone());
                let mut out = tools.run(name, input.clone()).await;
                let post = hooks.post_tool_use(name, id, &input, &out, model).await;
                let event = if out.is_error { Event::PostToolUseFailure } else { Event::PostToolUse };
                if let Some(text) = &post.updated_output {
                    out.content = text.clone();
                }
                let added = check.context.iter().map(|c| hooks::feedback(Event::PreToolUse, c));
                let added = added.chain(post.block.iter().chain(&post.context).map(|c| hooks::feedback(event, c)));
                for text in added {
                    out.content.truncate(out.content.trim_end().len());
                    out.content.push_str(&format!("\n\n{text}"));
                }
                (out, check, Some(post))
            }))
            .await;

            let mut results = Vec::with_capacity(outputs.len());
            let mut halt = None;
            for (t, (out, check, post)) in tool_uses.iter().zip(outputs) {
                observer.on_tool_result(t["name"].as_str().unwrap_or(""), &out);
                if let Some(post) = &post {
                    show_notices(post, observer);
                }
                halt = halt.or(check.halt).or(post.and_then(|p| p.halt));
                results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": t["id"],
                    "content": out.to_result_content(),
                    "is_error": out.is_error,
                }));
            }
            // A hook said to stop (`continue: false`): the turn ends here.
            if let Some(reason) = halt {
                let reason = if reason.is_empty() { String::new() } else { format!(": {reason}") };
                observer.on_hook(&format!("A hook stopped the turn{reason}"));
                results.push(json!({"type": "text", "text": format!("[Stopped by a hook{reason}]")}));
                self.messages.push(Message::user(results));
                return Ok(());
            }
            self.messages.push(Message::user(results));
        }
    }
}

/// Passes what hooks had for the user on to the frontend.
fn show_notices(outcome: &Outcome, observer: &mut dyn Observer) {
    for notice in &outcome.notices {
        observer.on_hook(notice);
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

    #[derive(Default)]
    struct Notices(Vec<String>);

    impl Observer for Notices {
        fn on_text_delta(&mut self, _: &str) {}
        fn on_text_end(&mut self) {}
        fn on_tool_call(&mut self, _: &str, _: &Value) {}
        fn on_tool_result(&mut self, _: &str, _: &ToolOutput) {}
        fn on_hook(&mut self, message: &str) {
            self.0.push(message.to_string());
        }
    }

    fn command(command: &str) -> Value {
        json!([{"hooks": [{"type": "command", "command": command}]}])
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn hooks_around_a_turn() {
        let _guard = crate::util::lock_cwd();
        use compact::tests::{gateway, texts};
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("stopped");
        let (client, mut requests) = gateway(|n| match n {
            0 => (vec![json!({"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "rm -rf /"}})], "tool_use", 100),
            _ => (vec![text("done")], "end_turn", 100),
        })
        .await;
        let mut agent = Agent::new(client, ToolRegistry::default(), "sys");
        agent.set_hooks(crate::hooks::tests::hooks(json!({
            "SessionStart": command("echo on branch main"),
            "UserPromptSubmit": command("echo '{\"hookSpecificOutput\": {\"additionalContext\": \"ticket 42\"}}'"),
            "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo 'no rm' >&2; exit 2"}]}],
            // Sends the model back once, then lets it stop.
            "Stop": command(&format!("[ -e {0} ] && exit 0; touch {0}; echo 'run the tests' >&2; exit 2", marker.display())),
        })));
        let mut seen = Notices::default();
        agent.run_turn(vec![text("clean up")], &mut seen).await.unwrap();

        let first = requests.recv().await.unwrap();
        let prompt = texts(&first["messages"][0]);
        assert_eq!(prompt[0], "clean up");
        assert!(prompt[1].contains("ticket 42") && prompt[1].starts_with("<hook-feedback event=\"UserPromptSubmit\">"));
        assert!(prompt[2].contains("on branch main") && prompt[2].contains("SessionStart"));

        let second = requests.recv().await.unwrap();
        let result = &second["messages"][2]["content"][0];
        assert_eq!(result["is_error"], true);
        assert!(result["content"].as_str().unwrap().contains("The call was blocked: no rm"), "{result}");

        let third = requests.recv().await.unwrap();
        let feedback = texts(&third["messages"][4]);
        assert!(feedback[0].contains("run the tests") && feedback[0].contains("event=\"Stop\""), "{feedback:?}");
        assert!(requests.try_recv().is_err(), "the second stop is let through");
        assert_eq!(seen.0, ["Stop hook: run the tests"]);

        // The next turn is no new session.
        agent.run_turn(vec![text("thanks")], &mut seen).await.unwrap();
        let fourth = requests.recv().await.unwrap();
        let last = fourth["messages"].as_array().unwrap().last().unwrap().clone();
        assert!(texts(&last).iter().all(|t| !t.contains("on branch main")), "{last}");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_blocked_prompt_is_not_sent() {
        let _guard = crate::util::lock_cwd();
        let (client, mut requests) = compact::tests::gateway(|_| (vec![text("hi")], "end_turn", 100)).await;
        let mut agent = Agent::new(client, ToolRegistry::default(), "sys");
        agent.set_hooks(crate::hooks::tests::hooks(json!({"UserPromptSubmit": command("echo 'has a secret' >&2; exit 2")})));
        let error = agent.run_turn(vec![text("key=abc")], &mut Notices::default()).await.unwrap_err();
        assert!(error.to_string().contains("has a secret"), "{error}");
        assert!(agent.messages().is_empty());
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn subagent_start_and_session_end() {
        let _guard = crate::util::lock_cwd();
        let dir = tempfile::tempdir().unwrap();
        let ended = dir.path().join("ended");
        let (client, mut requests) = compact::tests::gateway(|_| (vec![text("ok")], "end_turn", 100)).await;
        let hooks = crate::hooks::tests::hooks(json!({
            "SubagentStart": command("echo only read files"),
            "SessionEnd": command(&format!("cat >> {}", ended.display())),
        }));

        let mut sub = Agent::new(client.clone(), ToolRegistry::default(), "sys");
        sub.set_hooks(hooks.for_subagent("a1", "explore"));
        sub.run_turn(vec![text("find it")], &mut Notices::default()).await.unwrap();
        let first = requests.recv().await.unwrap();
        let sent = compact::tests::texts(&first["messages"][0]);
        assert!(sent[1].contains("only read files") && sent[1].contains("SubagentStart"), "{sent:?}");
        sub.run_turn(vec![text("and more")], &mut Notices::default()).await.unwrap();
        let second = requests.recv().await.unwrap();
        assert_eq!(compact::tests::texts(&second["messages"][2]), ["and more"], "only before its first run");
        sub.end_session("other").await;
        assert!(!ended.exists(), "subagents have no session");

        let mut agent = Agent::new(client, ToolRegistry::default(), "sys");
        agent.set_hooks(hooks);
        agent.end_session("clear").await;
        assert!(!ended.exists(), "no session started yet");
        agent.run_turn(vec![text("hi")], &mut Notices::default()).await.unwrap();
        agent.end_session("clear").await;
        agent.end_session("clear").await;
        agent.clear();
        agent.run_turn(vec![text("again")], &mut Notices::default()).await.unwrap();
        agent.end_session("prompt_input_exit").await;
        let reasons: Vec<String> = std::fs::read_to_string(&ended)
            .unwrap()
            .split_inclusive('}')
            .map(|j| serde_json::from_str::<Value>(j).unwrap()["reason"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(reasons, ["clear", "prompt_input_exit"]);
    }
}
