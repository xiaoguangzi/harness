//! Running the tool calls of one response: PreToolUse hooks see them all
//! first, then they run concurrently, each once the permissions let it, and
//! PostToolUse hooks see each result as it comes.

use std::collections::{BTreeMap, HashMap};

use futures::{StreamExt, future::join_all, stream::FuturesUnordered};
use serde_json::{Value, json};

use super::{Agent, Observer, show_notices};
use crate::hooks::{self, Event, Outcome};
use crate::permissions::{Decision, Request};
use crate::tools::ToolOutput;

/// What came of one call: its output, and what the hooks before and after
/// it said (none ran after a call that did not run).
struct Ran {
    output: ToolOutput,
    check: Outcome,
    post: Option<Outcome>,
}

impl Ran {
    /// A call that did not run, with `output` saying why.
    fn refused(output: ToolOutput, check: Outcome) -> Self {
        Self { output, check, post: None }
    }
}

impl Agent {
    /// Runs `calls`, the `tool_use` blocks of one response, showing each
    /// result as it finishes. Returns the content of the user message that
    /// answers them, every result in call order, and whether a hook stopped
    /// the turn. A call whose streamed input was not valid JSON (in
    /// `invalid`) is not run; the model gets the raw text back to retry.
    pub(super) async fn run_calls(
        &self,
        calls: &[Value],
        invalid: &HashMap<String, String>,
        observer: &mut dyn Observer,
    ) -> (Vec<Value>, bool) {
        // A fork starts from the conversation as it is now.
        let forks = calls.iter().any(|t| t["name"] == "agent" && t["input"]["agent_type"] == crate::subagent::FORK);
        if forks {
            let (system, tools, messages) = (self.system.clone(), self.tools.clone(), self.messages.clone());
            self.subagents.set_fork_base(Some(crate::subagent::ForkBase { system, tools, messages }));
        }
        // PreToolUse hooks see every call first; they may refuse one or
        // change its input.
        let model = self.client.spec();
        let checks = join_all(calls.iter().map(|t| async {
            let id = t["id"].as_str().unwrap_or("");
            if invalid.contains_key(id) {
                return Outcome::default();
            }
            self.hooks.pre_tool_use(t["name"].as_str().unwrap_or(""), id, &t["input"], model).await
        }))
        .await;
        for (t, check) in calls.iter().zip(&checks) {
            let input = check.updated_input.as_ref().unwrap_or(&t["input"]);
            observer.on_tool_call_id(t["id"].as_str().unwrap_or(""), t["name"].as_str().unwrap_or(""), input);
            show_notices(check, observer);
        }
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut running: FuturesUnordered<_> = calls
            .iter()
            .zip(checks)
            .enumerate()
            .map(|(i, (call, check))| {
                let id = call["id"].as_str().unwrap_or("").to_string();
                let run = crate::tools::progress::scope(id, progress_tx.clone(), self.run_call(call, check, invalid));
                async move { (i, run.await) }
            })
            .collect();
        drop(progress_tx);
        let mut ran: Vec<Option<Ran>> = (0..calls.len()).map(|_| None).collect();
        let mut waiting = BTreeMap::new();
        let mut progress_open = true;
        while !running.is_empty() {
            tokio::select! {
                event = progress_rx.recv(), if progress_open => match event {
                    Some((id, tasks)) => {
                        if tasks.is_empty() { waiting.remove(&id); } else { waiting.insert(id, tasks); }
                        let tasks: Vec<String> = waiting.values().flatten().cloned().collect();
                        observer.on_waiting(&tasks);
                    }
                    None => progress_open = false,
                },
                Some((i, done)) = running.next() => {
                    let t = &calls[i];
                    observer.on_tool_result_id(t["id"].as_str().unwrap_or(""), t["name"].as_str().unwrap_or(""), &done.output);
                    if let Some(post) = &done.post { show_notices(post, observer); }
                    ran[i] = Some(done);
                }
            }
        }
        observer.on_waiting(&[]);
        if forks {
            self.subagents.set_fork_base(None);
        }

        let mut results = Vec::with_capacity(ran.len());
        let mut halt = None;
        for (t, done) in calls.iter().zip(ran) {
            let Ran { output, check, post } = done.unwrap();
            halt = halt.or(check.halt).or(post.and_then(|p| p.halt));
            results.push(json!({
                "type": "tool_result",
                "tool_use_id": t["id"],
                "content": output.to_result_content(),
                "is_error": output.is_error,
            }));
        }
        // A hook said to stop (`continue: false`): the turn ends here.
        let Some(reason) = halt else { return (results, false) };
        let reason = if reason.is_empty() { String::new() } else { format!(": {reason}") };
        observer.on_hook(&format!("A hook stopped the turn{reason}"));
        results.push(json!({"type": "text", "text": format!("[Stopped by a hook{reason}]")}));
        (results, true)
    }

    /// Runs `call` unless its PreToolUse hooks (`check`) or the permissions
    /// refuse it, then its PostToolUse hooks, whose feedback goes after its
    /// output.
    async fn run_call(&self, call: &Value, check: Outcome, invalid: &HashMap<String, String>) -> Ran {
        let (id, name) = (call["id"].as_str().unwrap_or(""), call["name"].as_str().unwrap_or(""));
        if let Some(raw) = invalid.get(id) {
            return Ran::refused(ToolOutput::error(json!({"INVALID_JSON": raw}).to_string()), check);
        }
        if let Some(reason) = &check.block {
            let text = hooks::feedback(Event::PreToolUse, &format!("The call was blocked: {reason}"));
            return Ran::refused(ToolOutput::error(text), check);
        }
        if check.halt.is_some() {
            return Ran::refused(ToolOutput::error("The call was not run: a hook stopped the turn"), check);
        }
        let input = check.updated_input.clone().unwrap_or_else(|| call["input"].clone());
        if let Err(reason) = self.permitted(name, &input, &check).await {
            return Ran::refused(ToolOutput::error(reason), check);
        }
        let model = self.client.spec();
        let mut output = self.tools.run(name, input.clone()).await;
        let post = self.hooks.post_tool_use(name, id, &input, &output, model).await;
        let event = if output.is_error { Event::PostToolUseFailure } else { Event::PostToolUse };
        if let Some(text) = &post.updated_output {
            output.content = text.clone();
        }
        let added = check.context.iter().map(|c| hooks::feedback(Event::PreToolUse, c));
        let added = added.chain(post.block.iter().chain(&post.context).map(|c| hooks::feedback(event, c)));
        for text in added {
            output.content.truncate(output.content.trim_end().len());
            output.content.push_str(&format!("\n\n{text}"));
        }
        Ran { output, check, post: Some(post) }
    }

    /// Whether the call of `name` with `input` may run: what the permission
    /// rules and mode say, after PreToolUse hooks (`check`), and if they say
    /// to ask, PermissionRequest hooks or the user. `Err` is what the model
    /// is told when it does not run.
    async fn permitted(&self, name: &str, input: &Value, check: &Outcome) -> Result<(), String> {
        let tool = self.tools.get(name).map(|t| t.as_ref());
        let reason = match self.permissions.decide(name, tool, input, check.permission.as_ref()) {
            Decision::Allow => return Ok(()),
            Decision::Deny(reason) => return Err(reason),
            Decision::Ask(reason) => reason,
        };
        let suggestion = crate::permissions::suggestion(name, input);
        let hooked = self.hooks.permission_request(name, input, &suggestion, self.client.spec()).await;
        if let Some(reason) = hooked.block {
            return Err(format!("A PermissionRequest hook denied this call: {reason}"));
        }
        if hooked.permission == Some(crate::permissions::HookDecision::Allow) {
            return Ok(());
        }
        let agent = self.hooks.subagent_label();
        let request = Request { tool: name.to_string(), input: input.clone(), reason, suggestion, agent };
        self.permissions.ask(request).await
    }
}
