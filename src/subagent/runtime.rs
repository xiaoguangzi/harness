//! The subagents one conversation started. Each keeps its conversation after
//! it finishes, so the parent can send it follow-ups. A foreground run hands
//! its report to the tool call waiting for it; a background run reports
//! through the parent's inbox, which starts a turn when the parent is idle.
//! Background runs outlive the parent's turn, even an interrupted one;
//! clearing or dropping the conversation stops them.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::AbortHandle;

use super::{MAX_RESULT_BYTES, Reporter};
use crate::agent::{Agent, Inbox, Observer, UsageLog};
use crate::hooks::Hooks;
use crate::llm::Client;
use crate::tools::{ToolOutput, truncate};

/// Subagents of one conversation that may run at once.
pub const MAX_RUNNING: usize = 8;
/// Finished subagents kept for follow-ups; the oldest are forgotten first.
const MAX_KEPT: usize = 32;
/// Reports are cut to this many bytes in progress events.
const MAX_EVENT_REPORT_BYTES: usize = 20_000;

tokio::task_local! {
    static CONTEXT: (Arc<Subagents>, Client);
}

/// Runs `future`, a turn of the conversation that owns `subagents`, so the
/// subagent tools it calls act on that conversation and new subagents start
/// from its current `client`.
pub async fn scope<F: Future>(subagents: Arc<Subagents>, client: Client, future: F) -> F::Output {
    CONTEXT.scope((subagents, client), future).await
}

/// The subagents and model of the conversation whose turn is running.
pub fn current() -> Option<(Arc<Subagents>, Client)> {
    CONTEXT.try_with(Clone::clone).ok()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Status {
    Running,
    Completed,
    Failed,
    Stopped,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Completed => "completed",
            Status::Failed => "failed",
            Status::Stopped => "stopped",
        }
    }
}

pub struct Subagents {
    /// The parent's inbox, where background reports go.
    parent: Arc<Inbox>,
    /// The parent's token counts, which the subagents add to.
    usage: Arc<UsageLog>,
    /// Where progress goes, if the frontend set its own place.
    reporter: Mutex<Option<Reporter>>,
    /// The parent's hooks, which each subagent runs as its own.
    hooks: Mutex<Hooks>,
    /// In the order they started.
    children: Mutex<Vec<Arc<Child>>>,
}

pub struct Child {
    pub id: String,
    pub agent_type: String,
    pub description: String,
    pub model: String,
    agent: tokio::sync::Mutex<Agent>,
    /// The child's own inbox: messages sent to it while it runs.
    inbox: Arc<Inbox>,
    reporter: Reporter,
    background: AtomicBool,
    tool_calls: AtomicUsize,
    state: Mutex<State>,
}

struct State {
    status: Status,
    /// Counts runs, so a stopped run cannot overwrite a later one.
    run: u64,
    started: Instant,
    /// How long the last run took, once it ended.
    took: Option<Duration>,
    handle: Option<AbortHandle>,
}

/// A started run of a subagent.
pub struct Run {
    pub child: Arc<Child>,
    run: u64,
    done: oneshot::Receiver<ToolOutput>,
}

impl Run {
    /// Waits for the run's report. Dropping the wait (the parent's turn was
    /// cancelled) stops the run.
    pub async fn report(self) -> ToolOutput {
        struct StopOnDrop<'a>(&'a Child, u64);
        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                self.0.stop(Some(self.1), None);
            }
        }
        let _stop = StopOnDrop(&self.child, self.run);
        let id = &self.child.id;
        match self.done.await {
            Ok(output) => {
                let next = format!("(agent_id: {id} — continue this subagent with message_agent)");
                ToolOutput { content: format!("{}\n\n{next}", output.content), ..output }
            }
            Err(_) => ToolOutput::error(format!(
                "Subagent {id} was stopped before it finished; changes it made so far remain. Its \
                 conversation is kept: continue it with message_agent."
            )),
        }
    }
}

/// How a message sent to a subagent was taken.
pub enum Sent {
    /// It was running; it reads the message at its next step.
    Queued,
    Started(Run),
}

impl Subagents {
    pub fn new(parent: Arc<Inbox>, usage: Arc<UsageLog>, hooks: Hooks) -> Self {
        Self { parent, usage, reporter: Mutex::new(None), hooks: Mutex::new(hooks), children: Mutex::new(Vec::new()) }
    }

    /// Gives subagents started from here on `hooks` (as theirs).
    pub fn set_hooks(&self, hooks: Hooks) {
        *self.hooks.lock().unwrap() = hooks;
    }

    /// Sends this conversation's subagent progress to `reporter` instead of
    /// the `agent` tool's.
    pub fn set_reporter(&self, reporter: Reporter) {
        *self.reporter.lock().unwrap() = Some(reporter);
    }

    /// Starts `agent` on `prompt` as a new subagent.
    pub fn spawn(
        &self,
        mut agent: Agent,
        agent_type: &str,
        description: &str,
        prompt: String,
        background: bool,
        fallback: &Reporter,
    ) -> Result<Run> {
        let mut children = self.children.lock().unwrap();
        check_capacity(&children)?;
        agent.set_usage(self.usage.clone());
        while children.len() >= MAX_KEPT {
            let Some(oldest) = children.iter().position(|c| !c.is_running()) else { break };
            children.remove(oldest);
        }
        let id = new_id(&children);
        agent.set_hooks(self.hooks.lock().unwrap().for_subagent(&id, agent_type));
        let child = Arc::new(Child {
            id,
            agent_type: agent_type.to_string(),
            description: description.to_string(),
            model: agent.model().to_string(),
            inbox: agent.inbox().clone(),
            agent: tokio::sync::Mutex::new(agent),
            reporter: self.reporter.lock().unwrap().clone().unwrap_or_else(|| fallback.clone()),
            background: AtomicBool::new(background),
            tool_calls: AtomicUsize::new(0),
            state: Mutex::new(State {
                status: Status::Stopped,
                run: 0,
                started: Instant::now(),
                took: None,
                handle: None,
            }),
        });
        children.push(child.clone());
        // Started under the lock, so a message sent meanwhile cannot start it too.
        Ok(self.start(&child, prompt, background))
    }

    /// Sends `message` to subagent `id`: a running one reads it at its next
    /// step, any other starts a new run on it.
    pub fn send(&self, id: &str, message: String, background: bool) -> Result<Sent> {
        let children = self.children.lock().unwrap();
        let child = find(&children, id)?;
        {
            let state = child.state.lock().unwrap();
            if state.status == Status::Running {
                child.inbox.push(format!("A message from the agent that started you:\n\n{message}"), true);
                return Ok(Sent::Queued);
            }
        }
        check_capacity(&children)?;
        Ok(Sent::Started(self.start(&child, message, background)))
    }

    fn start(&self, child: &Arc<Child>, prompt: String, background: bool) -> Run {
        let (tx, done) = oneshot::channel();
        let mut state = child.state.lock().unwrap();
        state.run += 1;
        state.status = Status::Running;
        state.started = Instant::now();
        state.took = None;
        child.background.store(background, Ordering::Relaxed);
        child.tool_calls.store(0, Ordering::Relaxed);
        let run = state.run;
        let task = child.clone().run(run, prompt, self.parent.clone(), tx);
        // The run takes this lock to finish, so it cannot end before its
        // handle is stored.
        state.handle = Some(tokio::spawn(task).abort_handle());
        child.report(&state, None);
        drop(state);
        Run { child: child.clone(), run, done }
    }

    /// Stops subagent `id` if it is running; its conversation stays for
    /// follow-ups. When the user stopped it, the parent hears so with its
    /// next turn. False if it was not running.
    pub fn stop(&self, id: &str, by_user: bool) -> Result<bool> {
        let child = find(&self.children.lock().unwrap(), id)?;
        Ok(child.stop(None, by_user.then_some(&self.parent)))
    }

    /// Stops every running subagent, as the user asked; returns how many.
    pub fn stop_all(&self) -> usize {
        let children = self.children.lock().unwrap().clone();
        children.iter().filter(|c| c.stop(None, Some(&self.parent))).count()
    }

    /// Stops and forgets every subagent, e.g. for a new conversation.
    pub fn reset(&self) {
        let children = std::mem::take(&mut *self.children.lock().unwrap());
        for child in children {
            child.stop(None, None);
        }
    }

    /// How many subagents run in the background, which report on their own.
    pub fn running_in_background(&self) -> usize {
        let children = self.children.lock().unwrap();
        children.iter().filter(|c| c.is_running() && c.background.load(Ordering::Relaxed)).count()
    }

    /// Each subagent's current state, oldest first.
    pub fn list(&self) -> Vec<Value> {
        self.children.lock().unwrap().iter().map(|c| c.snapshot(&c.state.lock().unwrap())).collect()
    }
}

impl Drop for Subagents {
    fn drop(&mut self) {
        self.reset();
    }
}

fn find(children: &[Arc<Child>], id: &str) -> Result<Arc<Child>> {
    match children.iter().find(|c| c.id == id) {
        Some(child) => Ok(child.clone()),
        None => bail!("no subagent `{id}` in this conversation (subagents do not outlive it, a restart or /clear)"),
    }
}

fn check_capacity(children: &[Arc<Child>]) -> Result<()> {
    if children.iter().filter(|c| c.is_running()).count() >= MAX_RUNNING {
        bail!("{MAX_RUNNING} subagents are already running; wait for one to finish or stop one first");
    }
    Ok(())
}

/// Six hex digits, unique among `children`.
fn new_id(children: &[Arc<Child>]) -> String {
    loop {
        let id = format!("{:06x}", RandomState::new().hash_one(Instant::now()) & 0xff_ffff);
        if !children.iter().any(|c| c.id == id) {
            return id;
        }
    }
}

impl Child {
    fn is_running(&self) -> bool {
        self.state.lock().unwrap().status == Status::Running
    }

    fn label(&self) -> String {
        format!("{}: {}", self.agent_type, self.description)
    }

    async fn run(self: Arc<Self>, run: u64, prompt: String, parent: Arc<Inbox>, done: oneshot::Sender<ToolOutput>) {
        let mut agent = self.agent.lock().await;
        // A stopped run leaves the history to repair before counting it.
        agent.settle();
        let start = agent.messages().len();
        let mut observer = SubObserver { child: &self };
        let mut content = vec![json!({"type": "text", "text": prompt})];
        loop {
            let result = agent.run_turn(std::mem::take(&mut content), &mut observer).await;
            let mut state = self.state.lock().unwrap();
            if state.run != run || state.status != Status::Running {
                return;
            }
            // A message came in just as it finished: carry on with it.
            if result.is_ok() && self.inbox.wants_turn() {
                continue;
            }
            let output = outcome(&agent, start, result, self.tool_calls.load(Ordering::Relaxed));
            state.status = if output.is_error { Status::Failed } else { Status::Completed };
            state.took = Some(state.started.elapsed());
            state.handle = None;
            self.report(&state, Some(&output));
            // Pushed under the lock, so nobody sees it finished but unreported.
            if self.background.load(Ordering::Relaxed) {
                parent.push(self.notification(state.status, &output.content), true);
            }
            drop(state);
            let _ = done.send(output);
            return;
        }
    }

    /// Stops the running run (only run `run`, if given). A background one
    /// stopped by the user leaves a note in `parent`'s inbox.
    fn stop(&self, run: Option<u64>, parent: Option<&Arc<Inbox>>) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.status != Status::Running || run.is_some_and(|r| r != state.run) {
            return false;
        }
        if let Some(handle) = state.handle.take() {
            handle.abort();
        }
        state.status = Status::Stopped;
        state.took = Some(state.started.elapsed());
        self.report(&state, None);
        if let Some(parent) = parent
            && self.background.load(Ordering::Relaxed)
        {
            let note = "The user stopped this subagent before it finished; changes it made so far remain.";
            parent.push(self.notification(Status::Stopped, note), false);
        }
        true
    }

    /// What the parent is told when a background run ends.
    fn notification(&self, status: Status, report: &str) -> String {
        let next = match status {
            Status::Stopped => "It can be continued with message_agent.",
            _ => "The user has not seen this report; relay what matters. Send the subagent follow-ups with message_agent.",
        };
        format!(
            "<subagent-notification>\n<agent_id>{}</agent_id>\n<agent>{}</agent>\n<status>{}</status>\n\
             <report>\n{report}\n</report>\n</subagent-notification>\n{next}",
            self.id,
            self.label(),
            status.as_str(),
        )
    }

    fn snapshot(&self, state: &State) -> Value {
        let took = state.took.unwrap_or_else(|| state.started.elapsed());
        json!({
            "id": self.id,
            "agent_type": self.agent_type,
            "description": self.description,
            "label": self.label(),
            "model": self.model,
            "status": state.status.as_str(),
            "background": self.background.load(Ordering::Relaxed),
            "tool_calls": self.tool_calls.load(Ordering::Relaxed),
            "seconds": took.as_secs(),
        })
    }

    /// Reports the status in `state`, this child's (locked), with the
    /// report of a run that just ended.
    fn report(&self, state: &State, output: Option<&ToolOutput>) {
        let mut event = self.snapshot(state);
        event["type"] = json!("subagent_status");
        if let Some(output) = output {
            event["report"] = json!(truncate(&output.content, MAX_EVENT_REPORT_BYTES));
            event["is_error"] = json!(output.is_error);
        }
        (self.reporter)(&event);
    }
}

/// A run's result: the last text it wrote, or why there is none.
fn outcome(agent: &Agent, start: usize, result: Result<()>, tool_calls: usize) -> ToolOutput {
    let report = truncate(agent.last_text(start).trim(), MAX_RESULT_BYTES);
    match result {
        Ok(()) if report.is_empty() => ToolOutput::error("The subagent finished without a report"),
        Ok(()) => ToolOutput::ok(report),
        Err(e) => ToolOutput::error(format!(
            "The subagent failed after {tool_calls} tool calls: {e:#}{}",
            if report.is_empty() { String::new() } else { format!("\n\nLast output:\n{report}") }
        )),
    }
}

/// Reports a subagent's tool calls; its text is read from the finished
/// conversation instead of being streamed.
struct SubObserver<'a> {
    child: &'a Child,
}

impl Observer for SubObserver<'_> {
    fn on_text_delta(&mut self, _text: &str) {}
    fn on_text_end(&mut self) {}
    fn on_tool_call(&mut self, name: &str, input: &Value) {
        let child = self.child;
        child.tool_calls.fetch_add(1, Ordering::Relaxed);
        (child.reporter)(&json!({
            "type": "subagent",
            "id": child.id,
            "label": child.label(),
            "background": child.background.load(Ordering::Relaxed),
            "tool": name,
            "input": input,
        }));
    }
    fn on_tool_result(&mut self, _name: &str, _output: &ToolOutput) {}
}
