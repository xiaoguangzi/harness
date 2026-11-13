//! `code`: the model writes JavaScript that calls its other tools as async
//! functions, so a chain or fan-out of calls takes one round trip and only
//! what the script prints comes back ("code mode"). On with the `code_mode`
//! setting.
//!
//! The script runs in QuickJS on a blocking thread ([`engine`]); each tool
//! call it makes comes back here and runs on this task, through
//! [`crate::agent::run_nested`]: the hooks, permissions, scheduling guards
//! and task-locals (working directory, checkpoint, file tracker) of a direct
//! call all apply.

mod engine;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolOutput, ToolRegistry, parse_input, scheduling, spill};
use engine::{Callable, End, Reply};

pub const NAME: &str = "code";

/// Tools a script cannot call: those that run other agents or tools, wait,
/// or ask the user.
const NOT_CALLABLE: [&str; 8] =
    [NAME, "agent", "message_agent", "stop_agent", "wait_tasks", "skill", "tool_search", "ask_user_question"];

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
/// What the model gets back at most; the rest goes to a file.
const MAX_OUTPUT_BYTES: usize = 100_000;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether conversations get the `code` tool (the `code_mode` setting).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

const DESCRIPTION: &str = "Run JavaScript that calls your other tools as functions, so that work taking many calls, \
or whose results you need only part of, takes one call and returns only what the script prints.

`code` is the body of an async function, run in a fresh QuickJS interpreter: plain modern JavaScript with no Node or \
browser APIs (no file system, network, `require`, `fetch` or timers). It reaches the outside only through the tools:
- `await tools[\"name\"](input)` calls the tool `name` with the same input object as calling it directly, and resolves \
to a string: the text the direct call shows, not JSON (`glob` and `grep` give one path per line, `bash` the command's \
output, `read` numbered lines). Parse it as JSON only where the tool says it returns JSON. A call that fails rejects \
with an Error whose message is the output. Bracket syntax works for every name, including MCP tools whose names contain `-`.
- `ALL_TOOLS` lists `{name, description, input_schema}` for each tool you can call. Not callable: agent, message_agent, \
stop_agent, wait_tasks, skill, tool_search, ask_user_question and code itself.
- Calls started together (`Promise.all`) run concurrently. Each goes through the same hooks and permission checks as a \
direct call. Their output reaches the script whole rather than shortened (up to 8 MB), so filter large results \
there; it is the same text the direct call gives, so `read` still numbers lines and pages long files.
- `print(...)` / `console.log(...)` add a line to what the call returns, and the value you `return` comes last (a \
string as it is, anything else as JSON). Only that comes back to you: print what you need, not whole tool outputs.
- `timeout_ms` (default 120000, at most 600000) bounds the whole script, its tool calls included. Calls it did not \
await are cancelled when it ends. Nothing persists between calls. Images tools return are not passed on, and files \
changed are not shown to the user as diffs.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    code: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub struct CodeTool {
    /// What the script can call: the registry it was built from, less
    /// [`NOT_CALLABLE`].
    tools: ToolRegistry,
    read_only: bool,
}

impl CodeTool {
    /// The tool for the conversation (or subagent) whose tools are
    /// `registry`. Build it again whenever those change, which
    /// [`ToolRegistry::retain`] and [`ToolRegistry::track_files`] do.
    pub fn new(registry: &ToolRegistry) -> Self {
        let mut tools = registry.clone();
        tools.tools.retain(|name, _| !NOT_CALLABLE.contains(&name.as_str()));
        let read_only = tools.tools.values().all(|t| t.read_only());
        Self { tools, read_only }
    }

    fn callable(&self) -> Vec<Callable> {
        let tools = self.tools.tools.values();
        tools.map(|t| Callable { name: t.name().into(), description: t.description().into(), schema: t.input_schema() }).collect()
    }
}

/// Sets the flag that stops the script when the call is dropped.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[async_trait]
impl Tool for CodeTool {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {"type": "string", "description": "The body of an async function: JavaScript that may `await` tool calls and `return` a value."},
                "timeout_ms": {"type": "integer", "description": "How long the whole script may run, in milliseconds (default 120000, at most 600000)."}
            },
            "required": ["code"]
        })
    }

    /// Read-only when every tool it can call is, as for a read-only
    /// subagent.
    fn read_only(&self) -> bool {
        self.read_only
    }

    /// The calls it makes take their own guards: holding the workspace here
    /// would make them wait for this call forever.
    fn access(&self, _input: &Value) -> scheduling::Access {
        scheduling::Access::Concurrent
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(input) => input,
            Err(e) => return e,
        };
        let timeout_ms = input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).clamp(1, MAX_TIMEOUT_MS);
        let (call_tx, mut call_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let _stop_on_drop = StopOnDrop(stop.clone());
        let (code, callable, flag) = (input.code, self.callable(), stop.clone());
        let mut engine = tokio::task::spawn_blocking(move || engine::run(&code, &callable, call_tx, reply_rx, flag));
        let mut reply_tx = Some(reply_tx);
        let mut running = FuturesUnordered::new();
        let deadline = tokio::time::sleep(Duration::from_millis(timeout_ms));
        tokio::pin!(deadline);
        let mut stopped: Option<String> = None;
        let (printed, end) = loop {
            tokio::select! {
                done = &mut engine => {
                    break done.unwrap_or_else(|e| (String::new(), End::Threw(format!("the script's thread failed: {e}"))));
                }
                Some(call) = call_rx.recv() => {
                    let tools = &self.tools;
                    running.push(async move {
                        let (output, halted) = spill::whole(crate::agent::run_nested(tools, &call.name, call.input)).await;
                        (call.id, output, halted)
                    });
                }
                Some((id, output, halted)) = running.next() => {
                    if halted {
                        stopped = Some("a hook stopped the turn".into());
                        stop.store(true, Ordering::Relaxed);
                        reply_tx = None;
                    } else if let Some(tx) = &reply_tx {
                        let result = if output.is_error { Err(output.content) } else { Ok(output.content) };
                        let _ = tx.send(Reply { id, result });
                    }
                }
                () = &mut deadline, if stopped.is_none() => {
                    stopped = Some(format!("it ran past its timeout of {timeout_ms} ms"));
                    stop.store(true, Ordering::Relaxed);
                    reply_tx = None;
                }
            }
        };
        drop(running);
        finish(printed, end, stopped)
    }
}

/// The call's output: what the script printed, then how it failed if it
/// did.
fn finish(printed: String, end: End, stopped: Option<String>) -> ToolOutput {
    let mut text = printed.trim_end().to_string();
    let failure = match end {
        End::Done => None,
        End::Threw(error) => Some(format!("The script failed: {error}")),
        End::Stopped => Some(format!("The script was stopped: {}.", stopped.as_deref().unwrap_or("it was cancelled"))),
    };
    if let Some(failure) = &failure {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(failure);
    } else if text.is_empty() {
        text = "(The script printed and returned nothing.)".into();
    }
    let text = spill::shorten(&text, MAX_OUTPUT_BYTES, NAME);
    if failure.is_some() { ToolOutput::error(text) } else { ToolOutput::ok(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo {
        name: &'static str,
        read_only: bool,
    }

    #[async_trait]
    impl Tool for Echo {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "echoes its input"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {"n": {"type": "integer"}}})
        }
        fn read_only(&self) -> bool {
            self.read_only
        }
        async fn run(&self, input: Value) -> ToolOutput {
            if input["fail"] == true {
                return ToolOutput::error("failed on purpose");
            }
            if let Some(ms) = input["sleep"].as_u64() {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            ToolOutput::ok(format!("{} {}", self.name, input))
        }
    }

    fn registry() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(Echo { name: "look", read_only: true });
        registry.register(Echo { name: "change", read_only: false });
        registry.register(Echo { name: "agent", read_only: false });
        registry.register(CodeTool::new(&registry));
        registry
    }

    async fn run(registry: &ToolRegistry, input: Value) -> ToolOutput {
        registry.run(NAME, input).await
    }

    #[tokio::test]
    async fn runs_calls_and_returns_what_the_script_prints() {
        let code = r#"
            const [a, b] = await Promise.all([tools.look({n: "1"}), tools.change({n: 2})]);
            print(a); return b;
        "#;
        let out = run(&registry(), json!({"code": code})).await;
        // String integers are typed by the schema, as for direct calls.
        assert_eq!(out, ToolOutput::ok("look {\"n\":1}\nchange {\"n\":2}"));
    }

    #[tokio::test]
    async fn excludes_orchestration_tools_and_itself() {
        let out = run(&registry(), json!({"code": "return ALL_TOOLS.map(t => t.name)"})).await;
        assert_eq!(out.content, "[\n  \"change\",\n  \"look\"\n]");
        let out = run(&registry(), json!({"code": "await tools.agent({})"})).await;
        assert!(out.is_error && out.content.contains("no tool named agent"), "{}", out.content);
    }

    #[tokio::test]
    async fn reports_failures_after_what_was_printed() {
        let out = run(&registry(), json!({"code": "print('first'); await tools.look({fail: true})"})).await;
        assert!(out.is_error);
        assert!(out.content.starts_with("first\n\nThe script failed: Error: failed on purpose"), "{}", out.content);
        let out = run(&registry(), json!({"code": "", "extra": 1})).await;
        assert!(out.is_error && out.content.contains("unknown field"), "{}", out.content);
        let out = run(&registry(), json!({"code": ""})).await;
        assert_eq!(out, ToolOutput::ok("(The script printed and returned nothing.)"));
    }

    #[tokio::test]
    async fn times_out_while_a_call_runs_and_while_busy() {
        let code = "print('started'); await tools.look({sleep: 10000})";
        let out = run(&registry(), json!({"code": code, "timeout_ms": 100})).await;
        assert!(out.is_error, "{}", out.content);
        assert_eq!(out.content, "started\n\nThe script was stopped: it ran past its timeout of 100 ms.");
        let out = run(&registry(), json!({"code": "for (;;) {}", "timeout_ms": 100})).await;
        assert!(out.content.contains("timeout of 100 ms"), "{}", out.content);
    }

    #[test]
    fn is_read_only_only_when_all_it_calls_is_and_follows_the_registry() {
        let mut registry = registry();
        assert!(!registry.get(NAME).unwrap().read_only());
        // Keeping the read-only tools rebuilds it with only those, so it stays.
        registry.retain(|t| t.read_only());
        let names: Vec<&str> = registry.names().collect();
        assert_eq!(names, [NAME, "look"]);
        let code = registry.get(NAME).unwrap().clone();
        assert!(code.read_only());
        // A filter that drops `code` drops it.
        registry.retain(|t| t.name() != NAME);
        assert!(registry.get(NAME).is_none());
    }

    #[tokio::test]
    async fn an_allow_list_reaches_what_the_script_can_call() {
        // As a subagent's `tools:` list does through `registry_for`.
        let mut registry = registry();
        registry.retain(|t| matches!(t.name(), NAME | "look"));
        let out = run(&registry, json!({"code": "return ALL_TOOLS.map(t => t.name).join()"})).await;
        assert_eq!(out.content, "look");
        let out = run(&registry, json!({"code": "await tools.change({})"})).await;
        assert!(out.is_error && out.content.contains("no tool named change"), "{}", out.content);
    }

    #[tokio::test]
    async fn scripts_get_tool_output_whole() {
        let text = "x".repeat(200_000);
        let whole = spill::whole(async { spill::shorten(&text, 1000, "t") }).await;
        assert_eq!(whole.len(), text.len());
    }
}
