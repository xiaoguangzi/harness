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
use crate::hooks::{Event, Hooks};
use crate::llm::Client;
use crate::skills::Skill;
use crate::subagent::{AgentDef, Forks, Subagents};
use crate::tools::{FileTracker, ToolOutput};
use crate::tools::shells::Shells;

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
}

impl Task {
    /// A task for `agent`. Its subagents' progress goes to the task's log,
    /// and a background subagent's report starts a turn when none runs.
    pub fn new(id: u64, title: String, def: Option<AgentDef>, files: FileTracker, agent: Agent, forks: Forks) -> Arc<Self> {
        let created = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let meta = Meta { status: Status::Idle, model: agent.model().to_string(), turns: 0, handle: None };
        Arc::new_cyclic(|task: &Weak<Task>| {
            let events = task.clone();
            agent.subagents().set_reporter(Arc::new(move |event: &Value| {
                if let Some(task) = events.upgrade() {
                    task.push(event.clone());
                }
            }));
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
            }
        })
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
                Work::Fork(skill, args) => {
                    let (subagents, client) = (agent.subagents().clone(), agent.client().clone());
                    match task.forks.run_for_user(&skill, &args, subagents, client).await {
                        Ok(prompt) => agent.run_turn(vec![json!({"type": "text", "text": prompt})], observer).await,
                        Err(e) => Err(anyhow::anyhow!(e)),
                    }
                }
            };
            drop(agent);
            task.finish(result);
            // Reports that arrived as the turn ended get a turn of their own.
            task.wake();
        };
        // Holding `meta` until the handle is stored keeps `finish` (which
        // takes the same lock) from running first.
        meta.handle = Some(tokio::spawn(run).abort_handle());
        Ok(())
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
