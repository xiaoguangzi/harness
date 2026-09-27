//! Commands `bash` runs in the background (`run_in_background`), such as dev
//! servers, watchers and long builds. Each conversation has its own set:
//! `bash_output` reads what one printed since the last look, `kill_shell`
//! stops one, and a command that exits by itself is reported through the
//! conversation's inbox. They outlive the turn that started them; clearing
//! or dropping the conversation kills them.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::task::AbortHandle;

use super::bash::{KillGroup, PIPE_GRACE, spawn};
use super::{Tool, ToolOutput, parse_input};
use crate::agent::Inbox;

/// Background commands of one conversation that may run at once.
const MAX_RUNNING: usize = 8;
/// Finished commands kept for `bash_output`; the oldest are forgotten first.
const MAX_KEPT: usize = 16;
/// Output kept per command; older output is dropped first.
const MAX_BUFFER_BYTES: usize = 1024 * 1024;
/// Output `bash_output` returns at once: the newest part of what is new.
const MAX_OUTPUT_BYTES: usize = 30_000;

tokio::task_local! {
    static CURRENT: Arc<Shells>;
}

/// Runs `future`, a turn of the conversation that owns `shells`, so the
/// background commands it starts and reads are that conversation's.
pub async fn scope<F: Future>(shells: Arc<Shells>, future: F) -> F::Output {
    CURRENT.scope(shells, future).await
}

/// The background commands of the conversation whose turn is running.
pub fn current() -> Option<Arc<Shells>> {
    CURRENT.try_with(Clone::clone).ok()
}

/// One conversation's background commands.
pub struct Shells {
    /// Where a command that exits by itself is reported.
    inbox: Arc<Inbox>,
    state: Mutex<List>,
}

#[derive(Default)]
struct List {
    shells: Vec<Arc<Shell>>,
    started: usize,
}

struct Shell {
    id: String,
    command: String,
    started: Instant,
    state: Mutex<ShellState>,
    handle: Mutex<Option<AbortHandle>>,
}

struct ShellState {
    output: Vec<u8>,
    /// Bytes dropped from the front of `output` to keep it bounded.
    dropped: usize,
    /// Bytes of the whole output `bash_output` has returned.
    read: usize,
    status: Status,
}

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Running,
    /// The exit code; `None` when a signal ended it.
    Exited(Option<i32>),
    Killed,
}

impl Shells {
    pub fn new(inbox: Arc<Inbox>) -> Self {
        Self { inbox, state: Mutex::new(List::default()) }
    }

    /// Starts `command` in the background; returns its id.
    pub fn start(&self, command: &str) -> Result<String, String> {
        let mut list = self.state.lock().unwrap();
        if list.shells.iter().filter(|s| s.status() == Status::Running).count() >= MAX_RUNNING {
            return Err(format!(
                "{MAX_RUNNING} background commands are already running; stop one with kill_shell first"
            ));
        }
        let (child, guard) = spawn(command)?;
        list.started += 1;
        let shell = Arc::new(Shell {
            id: format!("shell_{}", list.started),
            command: command.to_string(),
            started: Instant::now(),
            state: Mutex::new(ShellState { output: Vec::new(), dropped: 0, read: 0, status: Status::Running }),
            handle: Mutex::new(None),
        });
        let watched = shell.clone();
        let inbox = self.inbox.clone();
        let task = tokio::spawn(async move {
            let _guard = guard;
            watched.watch(child, &inbox).await;
        });
        *shell.handle.lock().unwrap() = Some(task.abort_handle());
        list.shells.push(shell.clone());
        // Forget the oldest finished commands beyond the limit.
        while list.shells.iter().filter(|s| s.status() != Status::Running).count() > MAX_KEPT {
            let Some(i) = list.shells.iter().position(|s| s.status() != Status::Running) else { break };
            list.shells.remove(i);
        }
        Ok(shell.id.clone())
    }

    fn get(&self, id: &str) -> Result<Arc<Shell>, String> {
        let list = self.state.lock().unwrap();
        list.shells.iter().find(|s| s.id == id.trim()).cloned().ok_or_else(|| {
            let ids: Vec<&str> = list.shells.iter().map(|s| s.id.as_str()).collect();
            match ids.is_empty() {
                true => format!("No background command {id}; none was started"),
                false => format!("No background command {id} (there are {})", ids.join(", ")),
            }
        })
    }

    /// Kills every command still running; returns how many there were.
    pub fn kill_all(&self) -> usize {
        self.state.lock().unwrap().shells.iter().filter(|shell| shell.kill()).count()
    }

    /// Kills the command `id` for the user (`/stop`), telling the model with
    /// its next request; false if it had ended already.
    pub fn stop(&self, id: &str) -> Result<bool, String> {
        let shell = self.get(id)?;
        let killed = shell.kill();
        if killed {
            self.inbox.push(format!("<shell-notification>\nThe user stopped background command {}.\n</shell-notification>", shell.id), false);
        }
        Ok(killed)
    }

    /// Kills every running command for the user (`/stop`), telling the
    /// model; returns how many there were.
    pub fn stop_all(&self) -> usize {
        let list = self.state.lock().unwrap();
        let stopped: Vec<&str> = list.shells.iter().filter(|s| s.kill()).map(|s| s.id.as_str()).collect();
        if !stopped.is_empty() {
            self.inbox.push(
                format!("<shell-notification>\nThe user stopped background commands {}.\n</shell-notification>", stopped.join(", ")),
                false,
            );
        }
        stopped.len()
    }

    /// Kills every command and forgets them, for a new conversation.
    pub fn reset(&self) {
        self.kill_all();
        self.state.lock().unwrap().shells.clear();
    }

    /// `(id, command, running)` for each command, oldest first.
    pub fn list(&self) -> Vec<(String, String, bool)> {
        let list = self.state.lock().unwrap();
        list.shells.iter().map(|s| (s.id.clone(), s.command.clone(), s.status() == Status::Running)).collect()
    }
}

impl Drop for Shells {
    fn drop(&mut self) {
        self.kill_all();
    }
}

impl Shell {
    fn status(&self) -> Status {
        self.state.lock().unwrap().status
    }

    /// Collects the output until the command exits, then reports it.
    async fn watch(&self, mut child: tokio::process::Child, inbox: &Inbox) {
        // Killing the command (aborting this task) kills what it started.
        let mut group = KillGroup(child.id());
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let mut reading = std::pin::pin!(async { tokio::join!(self.pump(stdout), self.pump(stderr)) });
        let mut read_done = false;
        let status = loop {
            tokio::select! {
                _ = &mut reading, if !read_done => read_done = true,
                status = child.wait() => break status,
            }
        };
        // Jobs it left running are the command's intent; leave them be, and
        // only wait a moment for them to let go of the pipes.
        group.0 = None;
        if !read_done {
            let _ = tokio::time::timeout(PIPE_GRACE, reading).await;
        }
        let code = status.ok().and_then(|s| s.code());
        {
            let mut state = self.state.lock().unwrap();
            if state.status != Status::Running {
                return;
            }
            state.status = Status::Exited(code);
        }
        let how = match code {
            Some(code) => format!("exited with code {code}"),
            None => "was ended by a signal".to_string(),
        };
        inbox.push(
            format!(
                "<shell-notification>\nBackground command {} (`{}`) {how}. Read its output with bash_output.\n\
                 </shell-notification>",
                self.id,
                crate::util::ellipsize(&self.command, 200)
            ),
            true,
        );
    }

    /// Appends a stream to the output as it arrives.
    async fn pump(&self, stream: Option<impl AsyncRead + Unpin>) {
        let Some(mut stream) = stream else { return };
        let mut buf = [0u8; 8192];
        loop {
            let n = match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let mut state = self.state.lock().unwrap();
            state.output.extend_from_slice(&buf[..n]);
            let excess = state.output.len().saturating_sub(MAX_BUFFER_BYTES);
            if excess > 0 {
                state.output.drain(..excess);
                state.dropped += excess;
            }
        }
    }

    /// Kills the command if it still runs; returns whether it did.
    fn kill(&self) -> bool {
        {
            let mut state = self.state.lock().unwrap();
            if state.status != Status::Running {
                return false;
            }
            state.status = Status::Killed;
        }
        if let Some(handle) = self.handle.lock().unwrap().take() {
            handle.abort();
        }
        true
    }

    /// The status, and the output since the last call (its newest part if
    /// there is too much).
    fn take_output(&self) -> String {
        let mut state = self.state.lock().unwrap();
        let end = state.dropped + state.output.len();
        let from = state.read.max(state.dropped);
        let mut skipped = from - state.read;
        let new = String::from_utf8_lossy(&state.output[from - state.dropped..]).into_owned();
        state.read = end;
        let mut cut = new.len().saturating_sub(MAX_OUTPUT_BYTES);
        while !new.is_char_boundary(cut) {
            cut += 1;
        }
        skipped += cut;
        let status = match state.status {
            Status::Running => format!("running for {}s", self.started.elapsed().as_secs()),
            Status::Exited(Some(code)) => format!("exited with code {code}"),
            Status::Exited(None) => "ended by a signal".to_string(),
            Status::Killed => "killed".to_string(),
        };
        let mut text = format!("[{}: {status}]\n", self.id);
        if skipped > 0 {
            text.push_str(&format!("... [{skipped} earlier bytes not shown]\n"));
        }
        match &new[cut..] {
            "" => text.push_str("(no new output)"),
            rest => text.push_str(rest),
        }
        text
    }
}

fn shells() -> Result<Arc<Shells>, ToolOutput> {
    current().ok_or_else(|| ToolOutput::error("Background commands only run within a conversation"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellInput {
    shell_id: String,
}

fn shell_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "properties": {"shell_id": {"type": "string", "description": description}},
        "required": ["shell_id"],
        "additionalProperties": false
    })
}

/// `bash_output`: what a background command printed since the last look.
pub struct BashOutputTool;

#[async_trait]
impl Tool for BashOutputTool {
    fn name(&self) -> &str {
        "bash_output"
    }

    fn description(&self) -> &str {
        "Read what a background command (started with bash `run_in_background`) printed since the \
         last bash_output call for it, stdout and stderr together, with its status: running, or \
         its exit code. Only the newest 30000 bytes of new output are returned."
    }

    fn input_schema(&self) -> Value {
        shell_schema("The shell id bash returned, e.g. shell_1.")
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let input: ShellInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let shells = match shells() {
            Ok(s) => s,
            Err(e) => return e,
        };
        match shells.get(&input.shell_id) {
            Ok(shell) => ToolOutput::ok(shell.take_output()),
            Err(e) => ToolOutput::error(e),
        }
    }
}

/// `kill_shell`: stops a background command and what it started.
pub struct KillShellTool;

#[async_trait]
impl Tool for KillShellTool {
    fn name(&self) -> &str {
        "kill_shell"
    }

    fn description(&self) -> &str {
        "Stop a background command started with bash `run_in_background`, and the processes it \
         started. Its output stays readable with bash_output."
    }

    fn input_schema(&self) -> Value {
        shell_schema("The shell id of the command to stop.")
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let input: ShellInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let shells = match shells() {
            Ok(s) => s,
            Err(e) => return e,
        };
        let shell = match shells.get(&input.shell_id) {
            Ok(shell) => shell,
            Err(e) => return ToolOutput::error(e),
        };
        match shell.kill() {
            true => ToolOutput::ok(format!("Killed {}", shell.id)),
            false => ToolOutput::ok(format!("{} had already ended", shell.id)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn run(tool: &dyn Tool, shells: &Arc<Shells>, input: Value) -> ToolOutput {
        scope(shells.clone(), tool.run(input)).await
    }

    async fn wait_until(shells: &Shells, id: &str, done: impl Fn(Status) -> bool) {
        let shell = shells.get(id).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !done(shell.status()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn runs_in_the_background_and_reports_the_exit() {
        let inbox = Arc::new(Inbox::default());
        let shells = Arc::new(Shells::new(inbox.clone()));
        let started = run(&super::super::BashTool, &shells, json!({"command": "echo one; sleep 0.3; echo two; exit 3", "run_in_background": true})).await;
        assert!(!started.is_error && started.content.contains("shell_1"), "{}", started.content);

        wait_until(&shells, "shell_1", |s| s != Status::Running).await;
        let out = run(&BashOutputTool, &shells, json!({"shell_id": "shell_1"})).await;
        assert_eq!(out.content, "[shell_1: exited with code 3]\none\ntwo\n");
        let again = run(&BashOutputTool, &shells, json!({"shell_id": "shell_1"})).await;
        assert!(again.content.ends_with("(no new output)"), "only what is new: {}", again.content);

        assert!(inbox.wants_turn(), "the exit wakes the conversation");
        let note = inbox.take()[0]["text"].as_str().unwrap().to_string();
        assert!(note.contains("shell_1") && note.contains("exited with code 3"), "{note}");
    }

    #[tokio::test]
    async fn kills_a_running_command_and_what_it_started() {
        let inbox = Arc::new(Inbox::default());
        let shells = Arc::new(Shells::new(inbox.clone()));
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("late");
        let command = format!("echo up; (sleep 1; touch {}) & sleep 30", marker.display());
        run(&super::super::BashTool, &shells, json!({"command": command, "run_in_background": true})).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let out = run(&BashOutputTool, &shells, json!({"shell_id": "shell_1"})).await;
        assert!(out.content.starts_with("[shell_1: running for") && out.content.ends_with("up\n"), "{}", out.content);

        let killed = run(&KillShellTool, &shells, json!({"shell_id": "shell_1"})).await;
        assert_eq!(killed.content, "Killed shell_1");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!marker.exists(), "the job it started was killed too");
        assert!(!inbox.wants_turn(), "a killed command is not reported");
        let out = run(&BashOutputTool, &shells, json!({"shell_id": "shell_1"})).await;
        assert!(out.content.starts_with("[shell_1: killed]"), "{}", out.content);
    }

    #[tokio::test]
    async fn dropping_the_conversation_kills_its_commands() {
        let shells = Arc::new(Shells::new(Arc::new(Inbox::default())));
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("late");
        let command = format!("sleep 1; touch {}", marker.display());
        run(&super::super::BashTool, &shells, json!({"command": command, "run_in_background": true})).await;
        drop(shells);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn keeps_only_the_newest_output() {
        let shells = Arc::new(Shells::new(Arc::new(Inbox::default())));
        run(&super::super::BashTool, &shells, json!({"command": "head -c 40000 /dev/zero | tr '\\0' x", "run_in_background": true})).await;
        wait_until(&shells, "shell_1", |s| s != Status::Running).await;
        let out = run(&BashOutputTool, &shells, json!({"shell_id": "shell_1"})).await;
        assert!(out.content.contains("[10000 earlier bytes not shown]"), "{}", &out.content[..100]);
        assert!(out.content.ends_with(&"x".repeat(MAX_OUTPUT_BYTES)));
    }

    #[tokio::test]
    async fn needs_a_conversation_and_a_known_id() {
        let out = super::super::BashTool.run(json!({"command": "true", "run_in_background": true})).await;
        assert!(out.is_error && out.content.contains("conversation"));
        let shells = Arc::new(Shells::new(Arc::new(Inbox::default())));
        let out = run(&BashOutputTool, &shells, json!({"shell_id": "shell_9"})).await;
        assert!(out.is_error && out.content.contains("none was started"), "{}", out.content);
    }
}
