//! Wait for task results without spending model requests on status polling.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::{join_all, select_all};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolOutput, parse_input};
use super::shells::{Shell, Shells};
use crate::subagent::runtime::{Child, Subagents};

pub struct WaitTasksTool;

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Any,
    #[default]
    All,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    task_ids: Vec<String>,
    #[serde(default)]
    mode: Mode,
    timeout_ms: Option<u64>,
}

enum Handle {
    Shell(Arc<Shell>),
    Agent(Arc<Child>, u64),
}

impl Handle {
    fn running(&self) -> bool {
        match self {
            Self::Shell(shell) => shell.running(),
            Self::Agent(child, run) => child.waiting(*run),
        }
    }

    async fn finished(&self) {
        match self {
            Self::Shell(shell) => shell.finished().await,
            Self::Agent(child, run) => child.finished(*run).await,
        }
    }

    fn result(&self, shells: &Shells, subagents: Option<&Subagents>) -> Value {
        match self {
            Self::Shell(shell) => {
                let mut result = shell.result();
                if result["status"] != "running" {
                    result["output"] = json!(shell.take_output());
                    shells.acknowledge(result["task_id"].as_str().unwrap());
                }
                result
            }
            Self::Agent(child, run) => {
                let result = child.result(*run);
                if result["status"] != "running"
                    && let Some(subagents) = subagents
                {
                    subagents.acknowledge(&child.id, *run);
                }
                result
            }
        }
    }
}

#[async_trait]
impl Tool for WaitTasksTool {
    fn name(&self) -> &str { "wait_tasks" }

    fn description(&self) -> &str {
        "Wait for background command or subagent results when your next step depends on them. \
         Otherwise continue independent work and rely on completion notifications. Accepts shell \
         IDs from bash and agent IDs from agent. mode all waits for all; any returns when one \
         finishes. Completed results are returned and pending duplicate notifications acknowledged. \
         A timeout or user message returns current status without stopping tasks. Do not poll or \
         repeatedly request short waits. Services need readiness checks rather than exit waits."
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {
            "task_ids": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 16},
            "mode": {"type": "string", "enum": ["any", "all"], "description": "Default all."},
            "timeout_ms": {"type": "integer", "minimum": 0, "maximum": 300000,
                "description": "Default 60000. Upper bound for this wait, not a task execution timeout."}
        }, "required": ["task_ids"], "additionalProperties": false})
    }

    fn read_only(&self) -> bool { true }

    async fn run(&self, input: Value) -> ToolOutput {
        let input: Input = match parse_input(input) { Ok(i) => i, Err(e) => return e };
        if input.task_ids.is_empty() || input.task_ids.len() > 16 {
            return ToolOutput::error("task_ids must contain 1 to 16 unique task IDs");
        }
        let mut ids = std::collections::HashSet::new();
        if input.task_ids.iter().any(|id| id.trim() != id || id.is_empty() || !ids.insert(id)) {
            return ToolOutput::error("task_ids must be nonempty, unique IDs without surrounding whitespace");
        }
        let timeout = input.timeout_ms.unwrap_or(60_000);
        if timeout > 300_000 { return ToolOutput::error("timeout_ms must be at most 300000"); }
        let Some(shells) = super::shells::current() else {
            return ToolOutput::error("wait_tasks only runs within a conversation");
        };
        let subagents = crate::subagent::runtime::current().map(|(agents, _)| agents);
        let mut handles = Vec::new();
        for id in &input.task_ids {
            if id.starts_with("shell_") {
                match shells.get(id) {
                    Ok(shell) => handles.push(Handle::Shell(shell)),
                    Err(e) => return ToolOutput::error(e),
                }
            } else if let Some(subagents) = &subagents {
                match subagents.wait_handle(id) {
                    Ok((child, run)) => handles.push(Handle::Agent(child, run)),
                    Err(e) => return ToolOutput::error(e.to_string()),
                }
            } else { return ToolOutput::error(format!("No subagent {id} in this conversation")); }
        }
        let waiting = handles.iter().any(Handle::running);
        let _waiting = waiting.then(|| super::progress::waiting(input.task_ids.clone()));
        let wait = async {
            match input.mode {
                Mode::All => { join_all(handles.iter().map(Handle::finished)).await; }
                Mode::Any => { select_all(handles.iter().map(|h| Box::pin(h.finished()))).await; }
            }
        };
        let reason = tokio::select! {
            biased;
            _ = wait => "completed",
            _ = shells.input_wanted() => "input",
            _ = tokio::time::sleep(Duration::from_millis(timeout)) => "timeout",
        };
        let tasks: Vec<Value> = handles.iter().map(|h| h.result(&shells, subagents.as_deref())).collect();
        ToolOutput::ok(json!({"reason": reason, "tasks": tasks}).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Inbox;
    use crate::tools::{BashTool, KillShellTool};

    async fn start(shells: &Arc<Shells>, command: &str, resource: &str) {
        let out = super::super::shells::scope(shells.clone(), BashTool.run(json!({
            "command": command, "run_in_background": true, "resource": resource
        }))).await;
        assert!(!out.is_error, "{}", out.content);
    }

    #[tokio::test]
    async fn background_resource_is_owned_until_stop_and_other_resources_can_run() {
        let shells = Arc::new(Shells::new(Arc::default()));
        start(&shells, "sleep 30", "wait-test:resource").await;
        let command = json!({"command": "printf released", "resource": "wait-test:resource"});
        let blocked = super::super::shells::scope(shells.clone(), BashTool.run(command.clone()));
        assert!(tokio::time::timeout(Duration::from_millis(20), blocked).await.is_err());
        let independent = BashTool.run(json!({"command": "printf independent", "resource": "wait-test:other"})).await;
        assert_eq!(independent.content, "independent");
        shells.stop("shell_1").unwrap();
        let released = tokio::time::timeout(Duration::from_secs(2), BashTool.run(command)).await.unwrap();
        assert_eq!(released.content, "released");
    }

    async fn wait(shells: &Arc<Shells>, ids: &[&str], mode: &str, timeout: u64) -> Value {
        let out = super::super::shells::scope(shells.clone(), WaitTasksTool.run(json!({
            "task_ids": ids, "mode": mode, "timeout_ms": timeout
        }))).await;
        assert!(!out.is_error, "{}", out.content);
        serde_json::from_str(&out.content).unwrap()
    }

    #[tokio::test]
    async fn any_then_all_collects_failures_without_duplicate_notifications() {
        let inbox = Arc::new(Inbox::default());
        let shells = Arc::new(Shells::new(inbox.clone()));
        start(&shells, "sleep 0.05; echo first; exit 7", "wait-test:first").await;
        start(&shells, "sleep 0.3; echo second", "wait-test:second").await;
        let first = wait(&shells, &["shell_1", "shell_2"], "any", 5000).await;
        assert_eq!(first["reason"], "completed");
        assert_eq!(first["tasks"][0]["exit_code"], 7);
        assert_eq!(first["tasks"][1]["status"], "running");
        assert!(first["tasks"][0]["output"].as_str().unwrap().contains("first"));
        assert!(!inbox.wants_turn());
        let all = wait(&shells, &["shell_1", "shell_2"], "all", 5000).await;
        assert_eq!(all["tasks"][1]["status"], "completed");
        assert!(all["tasks"][1]["output"].as_str().unwrap().contains("second"));
        assert!(!inbox.wants_turn());
    }

    #[tokio::test]
    async fn timeout_and_cancelled_wait_leave_the_process_alive() {
        let shells = Arc::new(Shells::new(Arc::default()));
        start(&shells, "sleep 30", "wait-test:cancel").await;
        let result = wait(&shells, &["shell_1"], "all", 10).await;
        assert_eq!(result["reason"], "timeout");
        assert_eq!(result["tasks"][0]["status"], "running");
        let timed = tokio::time::timeout(Duration::from_millis(20), wait(&shells, &["shell_1"], "all", 5000)).await;
        assert!(timed.is_err());
        assert!(shells.get("shell_1").unwrap().running());
        let killed = super::super::shells::scope(shells.clone(), KillShellTool.run(json!({"shell_id":"shell_1"}))).await;
        assert!(!killed.is_error);
        let result = wait(&shells, &["shell_1"], "all", 5000).await;
        assert_eq!(result["tasks"][0]["status"], "stopped");
    }

    #[tokio::test]
    async fn user_input_interrupts_wait_without_consuming_input_or_output() {
        let inbox = Arc::new(Inbox::default());
        let shells = Arc::new(Shells::new(inbox.clone()));
        start(&shells, "echo progress; sleep 30", "wait-test:input").await;
        let send = async { tokio::time::sleep(Duration::from_millis(40)).await; inbox.push("change direction".into(), true); };
        let (result, _) = tokio::join!(wait(&shells, &["shell_1"], "all", 5000), send);
        assert_eq!(result["reason"], "input");
        assert_eq!(result["tasks"][0]["status"], "running");
        assert_eq!(inbox.take()[0]["text"], "change direction");
        assert!(shells.get("shell_1").unwrap().take_output().contains("progress"));
    }

    #[tokio::test]
    async fn services_do_not_hold_completion_and_unknown_ids_do_not_consume_results() {
        let inbox = Arc::new(Inbox::default());
        let shells = Arc::new(Shells::new(inbox.clone()));
        let out = super::super::shells::scope(shells.clone(), BashTool.run(json!({
            "command": "sleep 30", "run_in_background": true, "background_kind": "service", "resource": "service:test"
        }))).await;
        assert!(!out.is_error);
        assert_eq!(shells.pending(), 0);
        start(&shells, "echo kept", "wait-test:kept").await;
        shells.get("shell_2").unwrap().finished().await;
        let out = super::super::shells::scope(shells.clone(), WaitTasksTool.run(json!({
            "task_ids": ["shell_2", "shell_missing"]
        }))).await;
        assert!(out.is_error);
        assert!(inbox.wants_turn());
        assert!(wait(&shells, &["shell_2"], "all", 0).await["tasks"][0]["output"].as_str().unwrap().contains("kept"));
    }
}
