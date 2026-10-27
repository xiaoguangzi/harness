//! The agent loop: send the conversation, run any requested tools, send the
//! results back, and repeat until the model ends its turn.

mod compact;
mod deferred;
mod usage;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use futures::{StreamExt, future::join_all, stream::FuturesUnordered};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::checkpoint::FileHistory;
use crate::hooks::{self, Event, Hooks, Outcome};
use crate::llm::{Client, Message, StreamEvent, Usage};
use crate::permissions::{Decision, Mode, Permissions, Request};
use crate::subagent::Subagents;
use crate::tools::shells::Shells;
use crate::tools::{ToolOutput, ToolRegistry};

pub use compact::{is_compacted, set_idle as set_idle_compaction};
#[cfg(test)]
pub(crate) use compact::tests as mock;
pub use usage::UsageLog;

/// Told to the model about a turn the user cancelled.
pub const INTERRUPTED: &str = "[Interrupted by the user]";
/// Tells the model plan mode went on or off.
const PLAN_ON: &str = "<permission-mode>Plan mode is on: research with the tools that only read, then present a plan for \
the user to approve. Don't change files or run commands that change things; such calls are refused until the user \
turns plan mode off.</permission-mode>";
const PLAN_OFF: &str = "<permission-mode>Plan mode is off: you may change things now, such as carrying out the plan \
the user approved.</permission-mode>";
/// Starts the block that tells the model when the user sent a message.
pub const TIMESTAMP_START: &str = "<timestamp>";
/// Comes before a `/btw` question.
const SIDE_QUESTION: &str = "A side question from the user, about the conversation so far. Answer it briefly from what \
you already know, without calling any tools; neither the question nor your answer is kept in the conversation, so \
don't start or plan any work.";

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
    /// Identifies calls independently of their tool name, including repeated
    /// calls to the same tool in one batch. Existing observers keep working.
    fn on_tool_call_id(&mut self, _id: &str, name: &str, input: &Value) {
        self.on_tool_call(name, input);
    }
    fn on_tool_result_id(&mut self, _id: &str, name: &str, output: &ToolOutput) {
        self.on_tool_result(name, output);
    }
    /// Token counts of a request: a finished one, or one that failed
    /// partway (counted as far as its stream got).
    fn on_usage(&mut self, _usage: &Usage) {}
    /// The response broke off and is requested again after `wait`: what
    /// streamed since the last `on_request_start` is void, and output
    /// starts over. `attempt` counts from 1 to `max`.
    fn on_retry(&mut self, _error: &anyhow::Error, _attempt: u32, _max: u32, _wait: Duration) {}
    /// The conversation is being summarized to make room in the context.
    fn on_compact_start(&mut self) {}
    /// The summary replaced the history: the context went from `before` to
    /// about `after` tokens.
    fn on_compact_end(&mut self, _before: u64, _after: u64) {}
    /// A hook has something for the user: its `systemMessage`, what it
    /// blocked, or how it failed.
    fn on_hook(&mut self, _message: &str) {}
    /// Empty when no tool is waiting; otherwise the task IDs being awaited.
    fn on_waiting(&mut self, _tasks: &[String]) {}
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
    completion: Option<String>,
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
        self.enqueue(blocks, wake, None)
    }

    pub fn push_completion(&self, key: String, text: String) {
        self.enqueue(vec![json!({"type": "text", "text": text})], true, Some(key));
    }

    /// Explicitly collected results must not also wake a later turn.
    pub fn acknowledge(&self, key: &str) {
        self.queue.lock().unwrap().entries.retain(|e| e.completion.as_deref() != Some(key));
    }

    pub async fn input_wanted(&self) {
        loop {
            let arrived = self.arrived.notified();
            tokio::pin!(arrived);
            arrived.as_mut().enable();
            if self.queue.lock().unwrap().entries.iter().any(|e| e.wake && e.completion.is_none()) { return; }
            arrived.await;
        }
    }

    fn enqueue(&self, blocks: Vec<Value>, wake: bool, completion: Option<String>) -> u64 {
        let deferred = completion.is_some();
        let id = {
            let mut queue = self.queue.lock().unwrap();
            let id = queue.next_id;
            queue.next_id += 1;
            queue.entries.push(Entry { id, blocks, wake, completion });
            id
        };
        if wake {
            self.arrived.notify_waiters();
            let hook = self.on_wake.lock().unwrap().clone();
            if let Some(hook) = hook {
                // Completion is published under the task state lock. Invoke
                // frontend hooks outside that lock to avoid lock inversion.
                if deferred { tokio::spawn(async move { hook(); }); } else { hook(); }
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
    /// The files changed since each prompt, as they were before, shared
    /// with the subagents.
    history: Arc<FileHistory>,
    /// Whether `history` is this conversation's, so its prompts start
    /// checkpoints; a subagent's changes go to its parent's.
    checkpoints: bool,
    /// How the next prompt is listed among the checkpoints, if not as its
    /// text.
    next_prompt: Option<String>,
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
    /// Compacts as if the context window were at most this big.
    compact_window: Option<u64>,
    /// Which tool calls run, ask or are refused; shared with the subagents.
    permissions: Arc<Permissions>,
    /// The permission mode the model was last told about, if any.
    told_mode: Option<Mode>,
    /// Whether this conversation defers tools behind `tool_search`, once
    /// decided (see `deferred`).
    tool_search: Option<bool>,
    /// A model chosen while a turn ran, switched to when the next one
    /// starts: a turn's requests stay on one model.
    switch_to: Arc<Mutex<Option<Client>>>,
}

impl Agent {
    pub fn new(client: Client, tools: ToolRegistry, system: impl Into<String>) -> Self {
        let inbox = Arc::new(Inbox::default());
        let usage = Arc::new(UsageLog::default());
        let history = Arc::new(FileHistory::default());
        let permissions = Arc::new(Permissions::default());
        let mut hooks = Hooks::default();
        hooks.set_client(client.clone());
        hooks.set_inbox(inbox.clone());
        hooks.set_permissions(permissions.clone());
        let subagents = Arc::new(Subagents::new(inbox.clone(), usage.clone(), history.clone(), hooks.clone()));
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
            history,
            checkpoints: true,
            next_prompt: None,
            context: None,
            compactions: 0,
            turn_compacted: false,
            hooks,
            session_start: Some("startup"),
            in_session: false,
            compact_window: None,
            permissions,
            told_mode: None,
            tool_search: None,
            switch_to: Arc::default(),
        }
    }

    /// Which tool calls run, ask the user or are refused.
    pub fn permissions(&self) -> &Arc<Permissions> {
        &self.permissions
    }

    /// Uses `permissions` from now on, as do the subagents started after.
    #[cfg(test)]
    pub fn set_permissions(&mut self, permissions: Arc<Permissions>) {
        self.hooks.set_permissions(permissions.clone());
        self.subagents.set_hooks(self.hooks.clone());
        self.permissions = permissions;
    }

    /// Tells the model about a change to or from plan mode with the next
    /// request, once per change.
    fn tell_mode(&mut self) {
        let mode = self.permissions.mode();
        let was = self.told_mode.replace(mode);
        if was == Some(mode) || self.hooks.is_subagent() {
            return;
        }
        if mode == Mode::Plan {
            self.inbox.push(PLAN_ON.to_string(), false);
        } else if was == Some(Mode::Plan) {
            self.inbox.push(PLAN_OFF.to_string(), false);
        }
    }

    /// The context window the conversation is compacted to fit: the
    /// model's, or a smaller one set with [`Agent::set_compact_window`].
    pub fn context_window(&self) -> u64 {
        self.client.context_window().min(self.compact_window.unwrap_or(u64::MAX))
    }

    /// Compacts the conversation as if the context window were at most
    /// `window` tokens.
    pub fn set_compact_window(&mut self, window: Option<u64>) {
        self.compact_window = window;
    }

    /// Answers `question` from the conversation so far without adding
    /// either to it (`/btw`): one request with the same system prompt and
    /// tools, so it reads the prompt cache, its text streamed to `on_text`.
    pub async fn side_question(&mut self, question: &str, on_text: &mut (dyn FnMut(&str) + Send)) -> Result<String> {
        self.settle();
        let definitions = self.tools.definitions();
        let mut request = self.messages.clone();
        let asked = json!({"type": "text", "text": format!("{SIDE_QUESTION}\n\n{question}")});
        match request.last_mut() {
            Some(last) if last.role == "user" => last.content.push(asked),
            _ => request.push(Message::user(vec![asked])),
        }
        let (log, model) = (&self.usage, self.client.spec());
        let mut on_event = |event: StreamEvent| match event {
            StreamEvent::TextDelta(text) => on_text(text),
            StreamEvent::PartialUsage(usage) => log.record(model, &usage),
            _ => {}
        };
        let response = self.client.create(&self.system, &definitions, &request, &mut on_event).await?;
        if let Some(usage) = &response.usage {
            self.usage.record(self.client.spec(), usage);
        }
        let answer: String = response.content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
        if answer.trim().is_empty() {
            bail!("the model gave no answer{}", if response.stop_reason.as_deref() == Some("tool_use") { " (it tried to use a tool)" } else { "" });
        }
        Ok(answer)
    }

    /// Starts a fork's conversation with `messages`, its parent's, whose
    /// last message has calls without results: `results` answer them in
    /// the fork's first message, ahead of its prompt.
    pub fn fork_from(&mut self, messages: Vec<Message>, results: Vec<Value>) {
        self.messages = messages;
        self.inbox.push_content(results, false);
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

    /// Where a model chosen while a turn runs waits for the next turn.
    pub fn switch_to(&self) -> &Arc<Mutex<Option<Client>>> {
        &self.switch_to
    }

    /// Switches to the model waiting in [`Agent::switch_to`], if any.
    pub fn switch_now(&mut self) {
        let next = self.switch_to.lock().unwrap().take();
        if let Some(client) = next {
            self.set_client(client);
        }
    }

    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Replaces the tool set, e.g. after the providers were reloaded.
    pub fn set_tools(&mut self, tools: ToolRegistry) {
        self.tools = tools;
        // The model hears of any change to the deferred tools with the next prompt.
        if self.tool_search.is_some() {
            self.sync_tool_search(&[]);
        }
    }

    /// Replaces the system prompt, for a new conversation.
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
    /// A subagent's hooks bring its parent's permissions.
    pub fn set_hooks(&mut self, mut hooks: Hooks) {
        match hooks.permissions() {
            Some(permissions) => self.permissions = permissions.clone(),
            None => hooks.set_permissions(self.permissions.clone()),
        }
        hooks.set_client(self.client.clone());
        hooks.set_inbox(self.inbox.clone());
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

    /// The checkpoints of this conversation's prompts.
    pub fn file_history(&self) -> &Arc<FileHistory> {
        &self.history
    }

    /// Keeps the files this agent changes in `history`, a parent's; its
    /// own prompts start no checkpoints there.
    pub fn set_file_history(&mut self, history: Arc<FileHistory>) {
        self.history = history;
        self.checkpoints = false;
    }

    /// Lists the next prompt among the checkpoints as `text`, e.g. the
    /// `/command` it was made from, rather than as the prompt itself; the
    /// UserPromptSubmit hooks get it too.
    pub fn set_prompt_label(&mut self, text: &str) {
        self.next_prompt = Some(text.to_string());
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
        self.told_mode = None;
        self.tool_search = None;
        self.session_start = Some("clear");
        if self.checkpoints {
            self.history.clear();
        }
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

    /// Takes the conversation back to just before the prompt whose
    /// checkpoint is at `message`, and returns that prompt. The code stays
    /// as it is (see [`FileHistory::changes`]); the model must read files
    /// again before changing them.
    pub fn rewind(&mut self, message: usize) -> Option<String> {
        self.settle();
        let checkpoint = self.history.checkpoints().into_iter().find(|c| c.message == message)?;
        self.messages.truncate(message);
        self.history.truncate(message);
        self.context = None;
        if let Some(files) = self.tools.files() {
            files.retain(&[]);
        }
        Some(checkpoint.prompt)
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
        self.switch_now();
        // What the user typed: hooks see a `/command` before it was expanded.
        let typed = self.next_prompt.take().unwrap_or_else(|| prompt_text(&content));
        let prompt = (!content.is_empty()).then(|| typed.clone());
        // What the user sends notes the time; tool results don't. It is kept
        // in the history and the system prompt stays as it was, so the
        // cached prefix of the conversation still applies.
        let stamp = prompt.as_ref().and_then(|_| crate::prompt::timestamp()).map(|now| {
            json!({"type": "text", "text": format!("{TIMESTAMP_START}{now}</timestamp>")})
        });
        if !content.is_empty() && !self.hooks.is_subagent() {
            self.prompt_hooks(&typed, &mut content, observer).await?;
        }
        self.tell_mode();
        let arrived = self.inbox.take();
        if content.is_empty() && arrived.is_empty() {
            return Ok(());
        }
        let session_start = self.session_start;
        if let Some(prompt) = prompt.filter(|_| self.checkpoints) {
            self.history.begin(self.messages.len(), prompt);
        }
        // Tool results that arrived for a fork must come first.
        let notice = self.sync_tool_search(&[]);
        self.turn_start = Some(self.messages.len());
        (self.compactions, self.turn_compacted) = (0, false);
        self.messages.push(Message::user(arrived.iter().cloned().chain(notice).chain(stamp).chain(content).collect()));
        let (subagents, client, shells) = (self.subagents.clone(), self.client.clone(), self.shells.clone());
        let nested = self.hooks.is_subagent();
        let history = self.history.clone();
        let turn = crate::subagent::scope(subagents, client, nested, self.run_loop(observer));
        let result = crate::checkpoint::scope(history, crate::tools::shells::scope(shells, turn)).await;
        if result.is_err() {
            // A compaction moves the turn's start.
            let turn_start = self.turn_start.unwrap_or_default();
            self.repair_after_error(turn_start);
            if self.checkpoints {
                self.history.truncate(self.messages.len());
            }
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

    /// Runs the UserPromptSubmit hooks on `typed`, the prompt as the user
    /// typed it: they may refuse it, or add to `content`.
    async fn prompt_hooks(&mut self, typed: &str, content: &mut Vec<Value>, observer: &mut dyn Observer) -> Result<()> {
        let outcome = self.hooks.user_prompt_submit(typed, self.client.spec()).await;
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
                StreamEvent::Retry { error, attempt, max, wait } => observer.on_retry(error, attempt, max, wait),
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
            // A fork starts from the conversation as it is now.
            let forks = tool_uses.iter().any(|t| t["name"] == "agent" && t["input"]["agent_type"] == crate::subagent::FORK);
            if forks {
                let (system, tools, messages) = (self.system.clone(), self.tools.clone(), self.messages.clone());
                self.subagents.set_fork_base(Some(crate::subagent::ForkBase { system, tools, messages }));
            }
            if let Some(usage) = &response.usage {
                self.context = Some((self.messages.len(), usage.input_tokens + usage.output_tokens));
            }

            match stop_reason {
                "tool_use" => {}
                // A server tool paused the response: it goes on from here.
                "pause_turn" => continue,
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
            let (tools, hooks, model, permissions) = (&self.tools, &self.hooks, self.client.spec(), &*self.permissions);
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
                observer.on_tool_call_id(t["id"].as_str().unwrap_or(""), t["name"].as_str().unwrap_or(""), input);
                show_notices(check, observer);
            }
            // Run all tool calls from this response concurrently, then send
            // every result back in a single user message. A call whose
            // streamed input was not valid JSON is not run; the model gets
            // the raw text back so it can retry.
            let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut calls: FuturesUnordered<_> = tool_uses.iter().zip(checks).enumerate().map(|(i, (t, check))| {
                let tx = progress_tx.clone();
                async move {
                    let output = crate::tools::progress::scope(t["id"].as_str().unwrap_or("").to_string(), tx, async move {
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
                        if let Err(reason) = permitted(permissions, tools, hooks, name, &input, &check, model).await {
                            return (ToolOutput::error(reason), check, None);
                        }
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
                    }).await;
                    (i, output)
                }
            }).collect();
            drop(progress_tx);
            let mut outputs: Vec<_> = (0..tool_uses.len()).map(|_| None).collect();
            let mut waiting = std::collections::BTreeMap::new();
            let mut progress_open = true;
            while !calls.is_empty() {
                tokio::select! {
                    event = progress_rx.recv(), if progress_open => match event {
                        Some((id, tasks)) => {
                            if tasks.is_empty() { waiting.remove(&id); } else { waiting.insert(id, tasks); }
                            let tasks: Vec<String> = waiting.values().flatten().cloned().collect();
                            observer.on_waiting(&tasks);
                        }
                        None => progress_open = false,
                    },
                    Some((i, output)) = calls.next() => {
                        let t = &tool_uses[i];
                        observer.on_tool_result_id(t["id"].as_str().unwrap_or(""), t["name"].as_str().unwrap_or(""), &output.0);
                        if let Some(post) = &output.2 { show_notices(post, observer); }
                        outputs[i] = Some(output);
                    }
                }
            }
            observer.on_waiting(&[]);
            if forks {
                self.subagents.set_fork_base(None);
            }

            let mut results = Vec::with_capacity(outputs.len());
            let mut halt = None;
            for (t, output) in tool_uses.iter().zip(outputs) {
                let (out, check, post) = output.unwrap();
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

/// Whether the call of `name` with `input` may run: what the permission
/// rules and mode say, after PreToolUse hooks (`check`), and if they say to
/// ask, PermissionRequest hooks or the user. `Err` is what the model is
/// told when it does not run.
async fn permitted(
    permissions: &Permissions,
    tools: &ToolRegistry,
    hooks: &Hooks,
    name: &str,
    input: &Value,
    check: &Outcome,
    model: &str,
) -> Result<(), String> {
    let reason = match permissions.decide(name, tools.get(name).map(|t| t.as_ref()), input, check.permission.as_ref()) {
        Decision::Allow => return Ok(()),
        Decision::Deny(reason) => return Err(reason),
        Decision::Ask(reason) => reason,
    };
    let suggestion = crate::permissions::suggestion(name, input);
    let hooked = hooks.permission_request(name, input, &suggestion, model).await;
    if let Some(reason) = hooked.block {
        return Err(format!("A PermissionRequest hook denied this call: {reason}"));
    }
    if hooked.permission == Some(crate::permissions::HookDecision::Allow) {
        return Ok(());
    }
    let request = Request { tool: name.to_string(), input: input.clone(), reason, suggestion, agent: hooks.subagent_label() };
    permissions.ask(request).await
}

/// Whether `block` is the time [`Agent`] noted for a user's message.
pub fn is_timestamp(block: &Value) -> bool {
    block["type"] == "text" && block["text"].as_str().is_some_and(|t| t.starts_with(TIMESTAMP_START))
}

/// The prompt in a user message's `content`: its last text block.
fn prompt_text(content: &[Value]) -> String {
    let text = content.iter().rev().find(|b| b["type"] == "text").and_then(|b| b["text"].as_str());
    text.unwrap_or_default().to_string()
}

/// Passes what hooks had for the user on to the frontend.
fn show_notices(outcome: &Outcome, observer: &mut dyn Observer) {
    for notice in &outcome.notices {
        observer.on_hook(notice);
    }
}

#[cfg(test)]
pub(crate) mod tests {
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

    #[tokio::test]
    async fn a_model_chosen_during_a_turn_is_used_from_the_next() {
        let (client, mut requests) = compact::tests::gateway(|_| (vec![text("done")], "end_turn", 100)).await;
        let mut agent = Agent::new(client.clone(), ToolRegistry::default(), "sys");
        *agent.switch_to().lock().unwrap() = Some(client.switch("mock/other").unwrap());
        assert_eq!(agent.model(), "mock/main", "not before a turn starts");
        agent.run_turn(vec![text("go")], &mut Notices::default()).await.unwrap();
        assert_eq!(requests.recv().await.unwrap()["model"], "other");
        assert_eq!(agent.model(), "mock/other");
    }

    #[derive(Default)]
    pub(crate) struct Notices(pub(crate) Vec<String>);

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
    async fn calls_that_need_approval_ask_or_are_refused() {
        let _guard = crate::util::lock_cwd();
        use crate::permissions::{Answer, Asker, Mode, Permissions};
        use compact::tests::texts;
        let bash = |id: &str, command: &str| json!({"type": "tool_use", "id": id, "name": "bash", "input": {"command": command}});
        let (client, mut requests) = compact::tests::gateway(move |n| match n {
            0 => (vec![bash("t1", "echo one"), bash("t2", "echo two")], "tool_use", 100),
            _ => (vec![text("done")], "end_turn", 100),
        })
        .await;
        let mut agent = Agent::new(client, ToolRegistry::with_defaults(crate::tools::FileTracker::default()), "sys");
        let config = serde_json::from_value(json!({"defaultMode": "plan", "allow": ["Bash(echo two:*)"]})).unwrap();
        agent.set_permissions(Arc::new(Permissions::with_config(Arc::new(config))));
        agent.run_turn(vec![text("go")], &mut Notices::default()).await.unwrap();
        let first = requests.recv().await.unwrap();
        assert!(texts(&first["messages"][0]).iter().any(|t| t.starts_with("<permission-mode>Plan mode is on")), "the model is told");
        let results = &requests.recv().await.unwrap()["messages"][2]["content"];
        assert!(results[0]["content"].as_str().unwrap().contains("Plan mode is on"), "{results}");
        assert_eq!(results[1]["content"], "two\n", "an allow rule lets it run");

        struct Yes;
        #[async_trait::async_trait]
        impl Asker for Yes {
            async fn ask(&self, _: &crate::permissions::Request) -> Option<Answer> {
                Some(Answer::Allow)
            }
        }
        agent.permissions().set_mode(Mode::Default);
        let (client, mut requests) = compact::tests::gateway(move |n| match n {
            0 => (vec![bash("t3", "echo three")], "tool_use", 100),
            _ => (vec![text("done")], "end_turn", 100),
        })
        .await;
        agent.set_client(client);
        agent.run_turn(vec![text("again")], &mut Notices::default()).await.unwrap();
        let first = requests.recv().await.unwrap();
        let last = first["messages"].as_array().unwrap().last().unwrap().clone();
        assert!(texts(&last).iter().any(|t| t.starts_with("<permission-mode>Plan mode is off")), "{last}");
        let result = requests.recv().await.unwrap()["messages"].as_array().unwrap().last().unwrap()["content"][0].clone();
        assert!(result["content"].as_str().unwrap().contains("cannot be asked for here"), "no one to ask: {result}");
        agent.permissions().set_asker(Some(Arc::new(Yes)));
        let (client, mut requests) = compact::tests::gateway(move |n| match n {
            0 => (vec![bash("t4", "echo four")], "tool_use", 100),
            _ => (vec![text("done")], "end_turn", 100),
        })
        .await;
        agent.set_client(client);
        agent.run_turn(vec![text("once more")], &mut Notices::default()).await.unwrap();
        let _ = requests.recv().await;
        let result = requests.recv().await.unwrap()["messages"].as_array().unwrap().last().unwrap()["content"][0].clone();
        assert_eq!(result["content"], "four\n", "the user said yes");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn prompt_hooks_see_a_command_as_typed() {
        let _guard = crate::util::lock_cwd();
        let (client, _requests) = compact::tests::gateway(|_| (vec![text("hi")], "end_turn", 100)).await;
        let mut agent = Agent::new(client, ToolRegistry::default(), "sys");
        let hook = r#"grep -q '"prompt":"/review src"' && { echo 'typed' >&2; exit 2; }; exit 0"#;
        agent.set_hooks(crate::hooks::tests::hooks(json!({"UserPromptSubmit": command(hook)})));
        agent.set_prompt_label("/review src");
        let error = agent.run_turn(vec![text("Review the code in src ...")], &mut Notices::default()).await;
        assert!(error.unwrap_err().to_string().contains("typed"));
        agent.run_turn(vec![text("plain")], &mut Notices::default()).await.unwrap();
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn rewinds_to_before_a_prompt() {
        let _guard = crate::util::lock_cwd();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "v0").unwrap();
        let path = file.display().to_string();
        let write = move |content: &str| {
            json!({"type": "tool_use", "id": content, "name": "write", "input": {"path": path, "content": content}})
        };
        let (client, _requests) = compact::tests::gateway(move |n| match n {
            0 => (vec![write("v1")], "tool_use", 100),
            2 => (vec![write("v2")], "tool_use", 100),
            _ => (vec![text("done")], "end_turn", 100),
        })
        .await;
        let files = crate::tools::FileTracker::default();
        files.record(&file);
        let mut agent = Agent::new(client, ToolRegistry::with_defaults(files.clone()), "sys");
        agent.set_prompt_label("/first");
        agent.run_turn(vec![text("one")], &mut Notices::default()).await.unwrap();
        agent.run_turn(vec![text("two")], &mut Notices::default()).await.unwrap();
        let listed: Vec<(usize, String)> =
            agent.file_history().checkpoints().into_iter().map(|c| (c.message, c.prompt)).collect();
        assert_eq!(listed, [(0, "/first".to_string()), (4, "two".to_string())]);

        crate::checkpoint::restore(&agent.file_history().changes(4).unwrap()).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1");
        assert_eq!(agent.rewind(4).as_deref(), Some("two"));
        assert_eq!(agent.messages().len(), 4);
        assert_eq!(agent.file_history().checkpoints().len(), 1);
        assert!(agent.rewind(4).is_none(), "gone with its messages");
        assert!(files.check_writable(&file).is_err(), "the model must read the file again");
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
