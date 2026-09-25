//! Tasks: agent conversations that run in the background of the web server.
//!
//! Each task owns an [`Agent`] and an append-only event log. Turns run on a
//! spawned tokio task; every observer callback becomes an event that is
//! appended to the log and broadcast to live subscribers, so a browser can
//! replay a task from any point and then follow it.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::task::AbortHandle;

use crate::agent::{Agent, Observer};
use crate::llm::Client;
use crate::subagent::AgentDef;
use crate::tools::FileTracker;

/// Tool results are cut to this many bytes in events; the model still gets
/// the full result.
const MAX_EVENT_RESULT_BYTES: usize = 20_000;

tokio::task_local! {
    /// The task whose turn is running on this tokio task, so subagent
    /// progress (reported through a process-wide callback) reaches it.
    static CURRENT: Arc<Task>;
}

/// Appends a subagent's tool call to the running task's log. False when no
/// web task is running here (the CLI, or a tool called directly).
pub fn forward_subagent(label: &str, tool: &str, input: &Value) -> bool {
    CURRENT
        .try_with(|task| {
            task.push(json!({"type": "subagent", "label": label, "tool": tool, "input": input}));
        })
        .is_ok()
}

/// A serialized event with its sequence number in the task's log. Events
/// are serialized once and shared by the log and every subscriber.
pub type Sequenced = (usize, Arc<str>);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Status {
    Idle,
    Running,
    Error,
    Cancelled,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Running => "running",
            Status::Error => "error",
            Status::Cancelled => "cancelled",
        }
    }
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
    meta: Mutex<Meta>,
    log: Mutex<Vec<Arc<str>>>,
    tx: broadcast::Sender<Sequenced>,
}

impl Task {
    pub fn new(id: u64, title: String, def: Option<AgentDef>, files: FileTracker, agent: Agent) -> Self {
        let created = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let meta = Meta { status: Status::Idle, model: agent.model().to_string(), turns: 0, handle: None };
        Self {
            id,
            title,
            def,
            files,
            created,
            agent: tokio::sync::Mutex::new(agent),
            meta: Mutex::new(meta),
            log: Mutex::new(Vec::new()),
            tx: broadcast::channel(1024).0,
        }
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
        self.push(json!({"type": "status", "status": meta.status.as_str(), "model": meta.model}));
    }

    /// Starts a turn in the background. `user` is the event describing the
    /// user's message. Fails if a turn is already running.
    pub fn start_turn(self: &Arc<Self>, content: Vec<Value>, user: Value) -> Result<(), &'static str> {
        let mut meta = self.meta.lock().unwrap();
        if meta.status == Status::Running {
            return Err("the task is still running; wait for it or cancel it");
        }
        meta.status = Status::Running;
        meta.turns += 1;
        self.push(user);
        self.push_status(&meta);

        let task = self.clone();
        let run = async move {
            let mut agent = task.agent.lock().await;
            let result = agent.run_turn(content, &mut WebObserver { task: &task }).await;
            drop(agent);
            task.finish(result);
        };
        // Holding `meta` until the handle is stored keeps `finish` (which
        // takes the same lock) from running first.
        meta.handle = Some(tokio::spawn(CURRENT.scope(self.clone(), run)).abort_handle());
        Ok(())
    }

    fn finish(&self, result: anyhow::Result<()>) {
        let mut meta = self.meta.lock().unwrap();
        meta.handle = None;
        match result {
            Ok(()) => {
                meta.status = Status::Idle;
                self.push(json!({"type": "done"}));
            }
            Err(e) => {
                meta.status = Status::Error;
                self.push(json!({"type": "error", "message": format!("{e:#}")}));
            }
        }
        self.push_status(&meta);
    }

    /// Stops the running turn. Dropping its future kills running `bash`
    /// commands; the agent repairs its history before the next turn.
    pub fn cancel(&self) -> bool {
        let mut meta = self.meta.lock().unwrap();
        let Some(handle) = meta.handle.take() else { return false };
        handle.abort();
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
    fn on_text_delta(&mut self, text: &str) {
        self.task.push(json!({"type": "text", "text": text}));
    }

    fn on_text_end(&mut self) {
        self.task.push(json!({"type": "text_end"}));
    }

    fn on_tool_call(&mut self, name: &str, input: &Value) {
        self.task.push(json!({"type": "tool_call", "name": name, "input": input}));
    }

    fn on_tool_result(&mut self, name: &str, content: &str, is_error: bool) {
        let content = crate::tools::truncate(content, MAX_EVENT_RESULT_BYTES);
        self.task.push(json!({"type": "tool_result", "name": name, "content": content, "is_error": is_error}));
    }
}
