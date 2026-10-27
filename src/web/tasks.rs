//! Tasks: agent conversations that run in the background of the web server.
//!
//! Each task owns an [`Agent`] and an append-only event log. Turns run on a
//! spawned tokio task; every observer callback becomes an event that is
//! appended to the log and broadcast to live subscribers, so a browser can
//! replay a task from any point and then follow it.

use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::task::AbortHandle;

use crate::agent::{Agent, Inbox, Observer};
use crate::extensions::hooks::{Event, Hooks};
use crate::extensions::skills::Skill;
use crate::llm::Client;
use crate::security::permissions::{Answer, Asker, Request};
use crate::session::{self, Recorder, Store};
use crate::subagent::{AgentDef, Forks, Subagents};
use crate::tools::shells::Shells;
use crate::tools::{FileTracker, ToolOutput};

/// Tool results are cut to this many bytes in events; the model still gets
/// the full result.
const MAX_EVENT_RESULT_BYTES: usize = 20_000;
/// How long a task waits idle after a turn before Notification hooks hear
/// that it does.
const IDLE_NOTICE: Duration = Duration::from_secs(60);

/// A serialized event with its sequence number in the task's log. Events
/// are serialized once and shared by the log and every subscriber.
pub type Sequenced = (usize, Arc<str>);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Status {
    Idle,
    Running,
    Waiting,
    Error,
    Cancelled,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Running => "running",
            Status::Waiting => "waiting",
            Status::Error => "error",
            Status::Cancelled => "cancelled",
        }
    }
}

/// What a task runs in the background.
pub enum Work {
    /// A turn on these user message contents; empty for what waits in the
    /// inbox.
    Turn(Vec<Value>),
    /// Compacting the conversation (`/compact`), with what to focus on.
    Compact(Option<String>),
    /// A skill that runs in a subagent, with its arguments, then a turn on
    /// its report.
    Fork(Box<Skill>, String),
    /// Compacting the conversation while the task waits for the user.
    IdleCompact,
}

struct Meta {
    status: Status,
    model: String,
    turns: usize,
    handle: Option<AbortHandle>,
}

pub struct Task {
    pub id: u64,
    pub title: String,
    /// The subagent type the task was dispatched to; `None` for the main agent.
    pub def: Option<AgentDef>,
    /// Files this task's agent has read, for its `write` tool.
    pub files: FileTracker,
    created: u64,
    pub agent: tokio::sync::Mutex<Agent>,
    /// The agent's subagents and inbox, reachable while a turn holds the agent.
    pub subagents: Arc<Subagents>,
    shells: Arc<Shells>,
    inbox: Arc<Inbox>,
    forks: Forks,
    /// The agent's hooks, for telling them the task waits.
    hooks: Hooks,
    meta: Mutex<Meta>,
    log: Mutex<Vec<Arc<str>>>,
    tx: broadcast::Sender<Sequenced>,
    /// The agent's permissions, which the UI's mode menu changes.
    pub permissions: Arc<crate::security::permissions::Permissions>,
    /// Calls waiting for the user's approval, by question id.
    questions: Mutex<std::collections::BTreeMap<u64, tokio::sync::oneshot::Sender<Answer>>>,
    next_question: std::sync::atomic::AtomicU64,
    /// Where the conversation is saved, as the REPL's are; `None` without
    /// a home directory.
    recorder: Mutex<Option<Recorder>>,
}

/// The events that show `messages`, a conversation saved or rewound, as the
/// live ones showed it.
pub fn history_events(messages: &[crate::llm::Message]) -> Vec<Value> {
    let results: std::collections::HashMap<&str, &Value> = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|b| b["type"] == "tool_result")
        .filter_map(|b| Some((b["tool_use_id"].as_str()?, b)))
        .collect();
    let mut events = Vec::new();
    for message in messages {
        if crate::agent::is_compacted(message) {
            events.push(json!({"type": "note", "text": "✻ 会话在这里压缩过"}));
            continue;
        }
        if message.role == "user" {
            if message.content.iter().any(|b| b["type"] == "tool_result") {
                continue;
            }
            let prompt = message.content.iter().rev().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str())
                .find(|t| !t.starts_with('<') && *t != crate::agent::INTERRUPTED);
            if let Some(prompt) = prompt {
                events.push(json!({"type": "user", "text": prompt, "attachments": [], "warnings": []}));
            }
            continue;
        }
        for block in &message.content {
            match block["type"].as_str() {
                Some("text") => {
                    events.push(json!({"type": "text", "text": block["text"]}));
                    events.push(json!({"type": "text_end"}));
                }
                Some("tool_use") => {
                    let (id, name) = (block["id"].as_str().unwrap_or_default(), block["name"].as_str().unwrap_or_default());
                    events.push(json!({"type": "tool_call", "id": id, "name": name, "input": block["input"]}));
                    if let Some(result) = results.get(id) {
                        let text = crate::tools::result_text(&result["content"]);
                        let content = crate::tools::truncate(&text, MAX_EVENT_RESULT_BYTES);
                        events.push(json!({"type": "tool_result", "id": id, "name": name, "content": content, "is_error": result["is_error"] == true}));
                    }
                }
                _ => {}
            }
        }
    }
    events
}

/// Asks about a task's tool calls in the browser: a `permission` event in
/// the task's log, answered through `answer`.
struct WebAsker(Weak<Task>);

#[async_trait::async_trait]
impl Asker for WebAsker {
    async fn ask(&self, request: &Request) -> Option<Answer> {
        let task = self.0.upgrade()?;
        let id = task.next_question.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        task.questions.lock().unwrap().insert(id, tx);
        task.push(json!({
            "type": "permission",
            "id": id,
            "tool": request.tool,
            "input": request.input,
            "reason": request.reason,
            "suggestion": request.suggestion,
            "agent": request.agent,
        }));
        drop(task);
        // Unanswered when the call is dropped: the question goes.
        struct Forget(Weak<Task>, u64);
        impl Drop for Forget {
            fn drop(&mut self) {
                if let Some(task) = self.0.upgrade()
                    && task.questions.lock().unwrap().remove(&self.1).is_some()
                {
                    task.push(json!({"type": "permission_answered", "id": self.1, "answer": "dropped"}));
                }
            }
        }
        let _forget = Forget(self.0.clone(), id);
        rx.await.ok()
    }
}

impl Task {
    /// A task for `agent`. Its subagents' progress goes to the task's log,
    /// and a background subagent's report starts a turn when none runs.
    pub fn new(id: u64, title: String, def: Option<AgentDef>, files: FileTracker, agent: Agent, forks: Forks) -> Arc<Self> {
        let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
        let recorder = Store::default_location().map(|store| Recorder::new(store, cwd));
        Self::recording(id, title, def, files, agent, forks, recorder)
    }

    /// A task for `agent` that goes on with the saved conversation `meta`,
    /// which `agent` holds.
    pub fn resumed(id: u64, files: FileTracker, agent: Agent, forks: Forks, store: Store, meta: session::Meta) -> Arc<Self> {
        let title = if meta.title.is_empty() { "(untitled)".to_string() } else { meta.title.clone() };
        let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
        let mut recorder = Recorder::new(store, cwd);
        recorder.resume(meta);
        let task = Self::recording(id, title, None, files, agent, forks, Some(recorder));
        task.meta.lock().unwrap().turns = 0;
        task
    }

    fn recording(
        id: u64,
        title: String,
        def: Option<AgentDef>,
        files: FileTracker,
        agent: Agent,
        forks: Forks,
        recorder: Option<Recorder>,
    ) -> Arc<Self> {
        if let Some(rec) = &recorder {
            agent.hooks().set_session(&rec.meta().id, Some(rec.file()));
            agent.file_history().set_dir(crate::session::checkpoint::session_dir(&rec.meta().id));
        }
        let created = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let meta = Meta { status: Status::Idle, model: agent.model().to_string(), turns: 0, handle: None };
        Arc::new_cyclic(|task: &Weak<Task>| {
            let events = task.clone();
            agent.subagents().set_reporter(Arc::new(move |event: &Value| {
                if let Some(task) = events.upgrade() {
                    task.push(event.clone());
                }
            }));
            let permissions = agent.permissions().clone();
            permissions.set_asker(Some(Arc::new(WebAsker(task.clone()))));
            let wake = task.clone();
            agent.inbox().on_wake(move || {
                if let Some(task) = wake.upgrade() {
                    task.wake();
                }
            });
            Self {
                id,
                title,
                def,
                files,
                created,
                subagents: agent.subagents().clone(),
                shells: agent.shells().clone(),
                inbox: agent.inbox().clone(),
                forks,
                hooks: agent.hooks().clone(),
                agent: tokio::sync::Mutex::new(agent),
                meta: Mutex::new(meta),
                log: Mutex::new(Vec::new()),
                tx: broadcast::channel(1024).0,
                permissions,
                questions: Mutex::new(std::collections::BTreeMap::new()),
                next_question: std::sync::atomic::AtomicU64::new(1),
                recorder: Mutex::new(recorder),
            }
        })
    }

    /// The saved session the task records to.
    pub fn session_id(&self) -> Option<String> {
        self.recorder.lock().unwrap().as_ref().map(|rec| rec.meta().id.clone())
    }

    /// Saves the conversation, and its checkpoints, as they are now.
    fn save(&self, agent: &Agent) {
        let mut recorder = self.recorder.lock().unwrap();
        let Some(rec) = recorder.as_mut() else { return };
        if rec.meta().title.is_empty() {
            rec.set_title(&self.title);
        }
        if let Err(e) = rec.save(agent.messages(), agent.model()).and_then(|()| agent.file_history().save()) {
            drop(recorder);
            self.push(json!({"type": "hook", "message": format!("the conversation was not saved: {e:#}")}));
        }
    }

    /// Shows the conversation `agent` holds now from the start: a `reset`
    /// event, which clears what the UI shows, then its messages as events.
    pub fn replay(&self, agent: &Agent) {
        self.push(json!({"type": "reset"}));
        for event in history_events(agent.messages()) {
            self.push(event);
        }
    }

    /// The prompts the conversation can go back to before, the most recent
    /// first.
    pub fn checkpoints(&self, agent: &Agent) -> Vec<Value> {
        agent.file_history().checkpoints().iter().rev().map(|c| json!({
            "message": c.message,
            "prompt": c.prompt,
            "time": c.time,
            "files": c.files.len(),
        })).collect()
    }

    /// Goes back to before the prompt at `message` (`agent` is the task's,
    /// locked): the files `edit` and `write` changed since, with `code`,
    /// and the conversation, with `conversation`, whose prompt comes back.
    pub fn rewind(&self, agent: &mut Agent, message: usize, code: bool, conversation: bool) -> anyhow::Result<Option<String>> {
        if !agent.file_history().checkpoints().iter().any(|c| c.message == message) {
            anyhow::bail!("there is no prompt to go back to there");
        }
        if code {
            let changes = agent.file_history().changes(message)?;
            crate::session::checkpoint::restore(&changes)?;
            self.push(json!({"type": "hook", "message": format!("restored {} files as they were before that prompt", changes.len())}));
        }
        let prompt = if conversation { agent.rewind(message) } else { None };
        if prompt.is_some() {
            self.replay(agent);
        }
        self.save(agent);
        Ok(prompt)
    }

    /// Answers question `id` about a tool call; false if nothing waits
    /// for it.
    pub fn answer(&self, id: u64, answer: Answer) -> bool {
        let Some(tx) = self.questions.lock().unwrap().remove(&id) else { return false };
        let name = match &answer {
            Answer::Allow => "allow",
            Answer::AllowAlways => "always",
            Answer::Deny(_) => "deny",
        };
        self.push(json!({"type": "permission_answered", "id": id, "answer": name}));
        tx.send(answer).is_ok()
    }

    pub fn summary(&self) -> Value {
        let meta = self.meta.lock().unwrap();
        json!({
            "id": self.id,
            "title": self.title,
            "agent": self.def.as_ref().map_or("main", |d| d.name.as_str()),
            "model": meta.model,
            "status": meta.status.as_str(),
            "turns": meta.turns,
            "created": self.created,
            "active": meta.handle.is_some(),
            "pending_tasks": self.subagents.running_in_background() + self.shells.pending(),
            "permission_mode": self.permissions.mode().as_str(),
            "session_id": self.session_id(),
        })
    }

    /// The task's current model resolved on `client`'s providers, or
    /// `client` itself if that model no longer resolves.
    pub fn current_client(&self, client: &Client) -> Client {
        let model = self.meta.lock().unwrap().model.clone();
        client.switch(&model).unwrap_or_else(|_| client.clone())
    }

    /// Moves `agent` (this task's, locked) to `client`.
    pub fn set_client(&self, agent: &mut Agent, client: Client) {
        let model = client.spec().to_string();
        agent.set_client(client);
        self.push(json!({"type": "model", "model": model}));
        self.meta.lock().unwrap().model = model;
    }

    /// Appends an event and broadcasts it with its sequence number.
    pub fn push(&self, event: Value) {
        let event: Arc<str> = event.to_string().into();
        let mut log = self.log.lock().unwrap();
        let seq = log.len();
        log.push(event.clone());
        let _ = self.tx.send((seq, event));
    }

    /// Events from sequence number `from` on, plus a receiver for later ones.
    /// Taken under the log lock, so nothing falls between the two.
    pub fn subscribe(&self, from: usize) -> (Vec<Sequenced>, broadcast::Receiver<Sequenced>) {
        let log = self.log.lock().unwrap();
        let backlog = log.iter().enumerate().skip(from).map(|(i, e)| (i, e.clone())).collect();
        (backlog, self.tx.subscribe())
    }

    fn push_status(&self, meta: &Meta) {
        self.push(json!({"type": "status", "status": meta.status.as_str(), "model": meta.model, "active": meta.handle.is_some()}));
    }

    /// Starts a turn in the background. `user` is the event describing the
    /// user's message. Fails if a turn is already running.
    pub fn start_turn(self: &Arc<Self>, content: Vec<Value>, user: Value) -> Result<(), &'static str> {
        self.start(Work::Turn(content), user)
    }

    /// Steering a running turn must wake a wait rather than start a second
    /// model request concurrently. Other work keeps its normal busy check.
    pub fn send(self: &Arc<Self>, work: Work, user: Value) -> Result<(), &'static str> {
        let meta = self.meta.lock().unwrap();
        if meta.handle.is_some() && let Work::Turn(content) = work {
            self.push(user);
            drop(meta);
            self.inbox.push_content(content, true);
            return Ok(());
        }
        drop(meta);
        self.start(work, user)
    }

    /// Starts `work` in the background, like [`Task::start_turn`].
    pub fn start(self: &Arc<Self>, work: Work, user: Value) -> Result<(), &'static str> {
        let mut meta = self.meta.lock().unwrap();
        if meta.handle.is_some() {
            return Err("the task is still running; wait for it or cancel it");
        }
        meta.status = Status::Running;
        meta.turns += 1;
        // A `/command` as typed, for the checkpoints and hooks.
        let command = user["text"].as_str().filter(|t| t.starts_with('/')).map(String::from);
        self.push(user);
        self.push_status(&meta);

        let task = self.clone();
        let run = async move {
            let mut agent = task.agent.lock().await;
            let observer = &mut WebObserver { task: &task };
            let result = match work {
                Work::Turn(content) => {
                    if let Some(command) = &command {
                        agent.set_prompt_label(command);
                    }
                    agent.run_turn(content, observer).await
                }
                Work::Compact(focus) => agent.compact(focus.as_deref(), observer).await,
                Work::IdleCompact => agent.compact_when_idle(observer).await,
                Work::Fork(skill, args) => {
                    let (subagents, client) = (agent.subagents().clone(), agent.client().clone());
                    match task.forks.run_for_user(&skill, &args, subagents, client).await {
                        Ok(prompt) => agent.run_turn(vec![json!({"type": "text", "text": prompt})], observer).await,
                        Err(e) => Err(anyhow::anyhow!(e)),
                    }
                }
            };
            let compact_after = result.as_ref().ok().and_then(|_| agent.idle_compaction_after());
            agent.settle();
            task.save(&agent);
            drop(agent);
            task.finish(result);
            // Reports that arrived as the turn ended get a turn of their own.
            task.wake();
            if let Some(after) = compact_after {
                task.compact_when_idle(after);
            }
        };
        // Holding `meta` until the handle is stored keeps `finish` (which
        // takes the same lock) from running first.
        meta.handle = Some(tokio::spawn(run).abort_handle());
        Ok(())
    }

    /// Compacts the conversation if the task is still idle after `after`,
    /// before its prompt cache expires. A message sent meanwhile waits for
    /// the compaction, then gets its turn.
    fn compact_when_idle(self: &Arc<Self>, after: Duration) {
        let (task, turns) = (Arc::downgrade(self), self.meta.lock().unwrap().turns);
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let Some(task) = task.upgrade() else { return };
            let idle = {
                let meta = task.meta.lock().unwrap();
                meta.turns == turns && meta.status == Status::Idle && meta.handle.is_none()
            };
            if idle {
                let _ = task.start(Work::IdleCompact, json!({"type": "idle_compact"}));
            }
        });
    }

    /// Starts a turn for what waits in the agent's inbox, such as a
    /// background subagent's report, unless a turn is running: that one
    /// takes it along.
    fn wake(self: &Arc<Self>) {
        if self.inbox.wants_turn() {
            let _ = self.start_turn(Vec::new(), json!({"type": "wake"}));
        }
    }

    fn finish(self: &Arc<Self>, result: anyhow::Result<()>) {
        let mut meta = self.meta.lock().unwrap();
        meta.handle = None;
        if self.hooks.has(Event::Notification) {
            let (task, turns) = (Arc::downgrade(self), meta.turns);
            tokio::spawn(async move {
                tokio::time::sleep(IDLE_NOTICE).await;
                let Some(task) = task.upgrade() else { return };
                let (idle, model) = {
                    let meta = task.meta.lock().unwrap();
                    (meta.turns == turns && meta.status == Status::Idle, meta.model.clone())
                };
                if idle {
                    let message = format!("The task \"{}\" is waiting for your input", task.title);
                    task.hooks.notification("idle_prompt", &message, &model).await;
                }
            });
        }
        match result {
            Ok(()) => {
                let pending = self.subagents.running_in_background() + self.shells.pending();
                let waiting = pending > 0 || self.inbox.wants_turn();
                meta.status = if waiting { Status::Waiting } else { Status::Idle };
                self.push(json!({"type": if waiting { "waiting" } else { "done" }, "pending_tasks": pending}));
            }
            Err(e) => {
                meta.status = Status::Error;
                self.push(json!({"type": "error", "message": format!("{e:#}")}));
            }
        }
        self.push_status(&meta);
    }

    /// Stops the running turn. Dropping its future kills running `bash`
    /// commands and foreground subagents; the agent repairs its history
    /// before the next turn. Background subagents carry on.
    pub fn cancel(&self) -> bool {
        let mut meta = self.meta.lock().unwrap();
        if let Some(handle) = meta.handle.take() {
            handle.abort();
        } else if meta.status == Status::Waiting {
            self.shells.stop_pending();
            self.subagents.stop_all();
        } else { return false; }
        meta.status = Status::Cancelled;
        self.push(json!({"type": "cancelled"}));
        self.push_status(&meta);
        true
    }
}

struct WebObserver<'a> {
    task: &'a Task,
}

impl Observer for WebObserver<'_> {
    fn on_waiting(&mut self, tasks: &[String]) {
        let mut meta = self.task.meta.lock().unwrap();
        meta.status = if tasks.is_empty() { Status::Running } else { Status::Waiting };
        self.task.push(json!({"type": "waiting_tasks", "task_ids": tasks}));
        self.task.push_status(&meta);
    }

    fn on_text_delta(&mut self, text: &str) {
        self.task.push(json!({"type": "text", "text": text}));
    }

    fn on_text_end(&mut self) {
        self.task.push(json!({"type": "text_end"}));
    }

    fn on_tool_call(&mut self, name: &str, input: &Value) {
        self.task.push(json!({"type": "tool_call", "name": name, "input": input}));
    }

    fn on_tool_result(&mut self, name: &str, output: &ToolOutput) {
        let content = crate::tools::truncate(&output.content, MAX_EVENT_RESULT_BYTES);
        self.task.push(json!({"type": "tool_result", "name": name, "content": content, "is_error": output.is_error}));
    }

    fn on_tool_call_id(&mut self, id: &str, name: &str, input: &Value) {
        self.task.push(json!({"type": "tool_call", "id": id, "name": name, "input": input}));
    }

    fn on_tool_result_id(&mut self, id: &str, name: &str, output: &ToolOutput) {
        let content = crate::tools::truncate(&output.content, MAX_EVENT_RESULT_BYTES);
        self.task.push(json!({"type": "tool_result", "id": id, "name": name, "content": content, "is_error": output.is_error}));
    }

    fn on_compact_start(&mut self) {
        self.task.push(json!({"type": "compact_start"}));
    }

    fn on_hook(&mut self, message: &str) {
        self.task.push(json!({"type": "hook", "message": message}));
    }

    fn on_compact_end(&mut self, before: u64, after: u64) {
        self.task.push(json!({"type": "compact", "before": before, "after": after}));
    }

    fn on_retry(&mut self, error: &anyhow::Error, attempt: u32, max: u32, wait: Duration) {
        self.task.push(json!({
            "type": "retry",
            "message": crate::llm::retry_reason(error),
            "attempt": attempt,
            "max": max,
            "wait": wait.as_secs(),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Providers;
    use crate::llm::config::tests::{file, no_env};
    use crate::tools::ToolRegistry;

    #[tokio::test]
    async fn a_report_for_an_idle_task_starts_a_turn() {
        // A gateway that refuses every request, so the turn ends at once.
        let app = axum::Router::new().route("/v1/messages", axum::routing::post(|| async { (axum::http::StatusCode::BAD_REQUEST, "no") }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = file(json!({"model": "mock/main", "providers": {
            "mock": {"protocol": "anthropic", "base_url": url, "api_key": "test"}
        }}));
        let client = Client::new(Providers::from_parts(&config, &no_env).unwrap()).unwrap();
        let agent = Agent::new(client, ToolRegistry::new(), "sys");
        let task = Task::new(1, "test".into(), None, FileTracker::default(), agent, Forks::default());
        let (_, mut events) = task.subscribe(0);
        task.inbox.push("<subagent-notification>…</subagent-notification>".into(), true);
        let mut seen = Vec::new();
        while !seen.iter().any(|e: &String| e.contains(r#""type":"error""#)) {
            let (_, event) = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await.unwrap().unwrap();
            seen.push(event.to_string());
        }
        assert!(seen[0].contains(r#""type":"wake""#), "{seen:?}");
        let agent = task.agent.lock().await;
        assert!(agent.messages()[0].content[0]["text"].as_str().unwrap().contains("subagent-notification"), "the report is kept");
    }
}
