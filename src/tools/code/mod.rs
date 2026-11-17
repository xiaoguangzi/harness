//! `code`: the model writes JavaScript that calls its other tools as async
//! functions, so a chain or fan-out of calls takes one round trip and only
//! what the script prints comes back ("code mode"). On unless the
//! `code_mode` setting is `false`.
//!
//! Scripts run in one QuickJS context per tool, on a thread of its own
//! ([`engine`]), so what one defines the next can use. Each tool call a
//! script makes comes back here and runs on this task, through
//! [`crate::agent::run_nested`]: the hooks, permissions, scheduling guards
//! and task-locals (working directory, checkpoint, file tracker) of a direct
//! call all apply, and the frontend shows it as one.

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
use engine::{Callable, End, Job, Reply, Session};

pub const NAME: &str = "code";

/// Tools a script cannot call: those that run other agents or tools, wait,
/// or ask the user.
const NOT_CALLABLE: [&str; 8] =
    [NAME, "agent", "message_agent", "stop_agent", "wait_tasks", "skill", "tool_search", "ask_user_question"];

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
/// What the model gets back at most; the rest goes to a file.
const MAX_OUTPUT_BYTES: usize = 100_000;
/// The images and PDF documents the script's calls returned that come back
/// with its output, at most.
const MAX_IMAGES: usize = 8;
const MAX_DOCUMENTS: usize = 4;

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

Scripts run in one persistent QuickJS context: what a script defines at its top level (variables, functions) is there \
in the next call, so keep results you may need again in variables instead of reading a file or running a command \
again. Declaring a name again replaces it. Top-level `await` works. It is plain modern JavaScript with no Node or \
browser APIs (no file system, network, `require`, `fetch` or timers); it reaches the outside only through the tools:
- `await tools[\"name\"](input)` calls the tool `name` with the same input object as calling it directly. It resolves \
to the text the direct call shows (`bash` the command's output, `read` numbered lines), except for listings: `glob` \
gives an array of paths and `grep` an array of paths, an object mapping each path to its count (`output_mode: \
\"count\"`) or its lines as one string (`\"content\"`); empty when nothing matches. In a script they have no default \
`head_limit`: you get every match unless you ask for fewer. Parse text as JSON only where the tool says it returns \
JSON. A call that fails rejects with an Error whose message is the output. Bracket syntax works for every name, \
including MCP tools whose names contain `-`.
- `ALL_TOOLS` lists `{name, description, input_schema}` for each tool you can call. Not callable: agent, message_agent, \
stop_agent, wait_tasks, skill, tool_search, ask_user_question and code itself.
- Calls started together (`Promise.all`) run concurrently. Each goes through the same hooks and permission checks as a \
direct call, and the user sees each one. Their output reaches the script whole rather than shortened (up to 8 MB), so \
filter large results there; it is the same text the direct call gives, so `read` still numbers lines and pages long \
files. Images and PDFs that `read` gives come back to you with the script's output (8 images and 4 PDFs at most).
- What comes back to you: the lines `print(...)` / `console.log(...)` add, then the value of the script's last \
expression (a string as it is, anything else as JSON; a promise is awaited). Print or end with what you need, not \
whole tool outputs. A script written as a function body that `return`s a value works too, but what it declares does \
not persist.
- `description` is a few words on what the script does, shown to the user in its place.
- `timeout_ms` (default 120000, at most 600000) bounds the whole script, its tool calls included. Calls it did not \
await are cancelled when it ends. Scripts run one at a time, in the order they were called.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    code: String,
    /// For the user only.
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub struct CodeTool {
    /// What the script can call: the registry it was built from, less
    /// [`NOT_CALLABLE`].
    tools: ToolRegistry,
    read_only: bool,
    /// The context this tool's scripts share.
    session: Session,
}

impl CodeTool {
    /// The tool for the conversation (or subagent) whose tools are
    /// `registry`, with a context of its own. Build it again whenever those
    /// change, which [`ToolRegistry::retain`] and
    /// [`ToolRegistry::track_files`] do, or to start the context afresh
    /// ([`ToolRegistry::renew_code`]).
    pub fn new(registry: &ToolRegistry) -> Self {
        let mut tools = registry.clone();
        tools.tools.retain(|name, _| !NOT_CALLABLE.contains(&name.as_str()));
        let read_only = tools.tools.values().all(|t| t.read_only());
        let callable = tools.tools.values().map(|t| Callable {
            name: t.name().into(),
            description: t.description().into(),
            schema: t.input_schema(),
        });
        let session = Session::new(callable.collect());
        Self { tools, read_only, session }
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
                "code": {
                    "type": "string",
                    "description": "JavaScript to run in the persistent context: it may `await` tool calls at its top \
                                    level; the value of its last expression comes back."
                },
                "description": {"type": "string", "description": "A few words on what the script does, for the user (e.g. \"Find the callers of run_nested\")."},
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
        let (done_tx, mut done) = tokio::sync::oneshot::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let _stop_on_drop = StopOnDrop(stop.clone());
        self.session.run(Job { code: input.code, calls: call_tx, replies: reply_rx, stop: stop.clone(), done: done_tx });
        let mut reply_tx = Some(reply_tx);
        let mut running = FuturesUnordered::new();
        let deadline = tokio::time::sleep(Duration::from_millis(timeout_ms));
        tokio::pin!(deadline);
        let mut stopped: Option<String> = None;
        let mut media = Media::default();
        let (printed, end) = loop {
            tokio::select! {
                ended = &mut done => {
                    break ended.unwrap_or_else(|_| (String::new(), End::Threw("the script's thread failed".into())));
                }
                Some(call) = call_rx.recv() => {
                    let tools = &self.tools;
                    running.push(async move {
                        let (output, halted) = spill::whole(crate::agent::run_nested(tools, &call.name, call.input)).await;
                        (call.id, output, halted)
                    });
                }
                Some((id, output, halted)) = running.next() => {
                    media.add(&output.images);
                    if halted {
                        stopped = Some("a hook stopped the turn".into());
                        stop.store(true, Ordering::Relaxed);
                        reply_tx = None;
                    } else if let Some(tx) = &reply_tx {
                        let reply = match (output.is_error, output.value) {
                            (true, _) => Reply { id, result: Err(output.content), json: false },
                            (false, Some(value)) => Reply { id, result: Ok(value.to_string()), json: true },
                            (false, None) => Reply { id, result: Ok(output.content), json: false },
                        };
                        let _ = tx.send(reply);
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
        finish(printed, end, stopped, media)
    }
}

/// The images and PDF documents the script's calls returned, as content
/// blocks, up to [`MAX_IMAGES`] and [`MAX_DOCUMENTS`], and how many more
/// there were.
#[derive(Default)]
struct Media {
    blocks: Vec<Value>,
    images: usize,
    documents: usize,
}

impl Media {
    fn add(&mut self, blocks: &[Value]) {
        for block in blocks {
            let (count, max) = match block["type"].as_str() {
                Some("image") => (&mut self.images, MAX_IMAGES),
                Some("document") => (&mut self.documents, MAX_DOCUMENTS),
                _ => continue,
            };
            *count += 1;
            if *count <= max {
                self.blocks.push(block.clone());
            }
        }
    }

    /// What the model is told of those left out.
    fn left_out(&self) -> Option<String> {
        let images = self.images.saturating_sub(MAX_IMAGES);
        let documents = self.documents.saturating_sub(MAX_DOCUMENTS);
        let what = match (images, documents) {
            (0, 0) => return None,
            (i, 0) => format!("{i} image(s)"),
            (0, d) => format!("{d} PDF document(s)"),
            (i, d) => format!("{i} image(s) and {d} PDF document(s)"),
        };
        Some(format!(
            "({what} the script's calls returned are not shown: a script brings back at most {MAX_IMAGES} images and \
             {MAX_DOCUMENTS} PDFs.)"
        ))
    }
}

/// The call's output: what the script printed, then how it failed if it
/// did, with the images and documents its calls returned.
fn finish(printed: String, end: End, stopped: Option<String>, media: Media) -> ToolOutput {
    let mut text = printed.trim_end().to_string();
    let failure = match end {
        End::Done => None,
        End::Threw(error) => Some(format!("The script failed: {error}")),
        End::Stopped => Some(format!("The script was stopped: {}.", stopped.as_deref().unwrap_or("it was cancelled"))),
    };
    for note in failure.iter().cloned().chain(media.left_out()) {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&note);
    }
    if text.is_empty() {
        text = match media.blocks.is_empty() {
            true => "(The script printed and returned nothing.)",
            false => "(The script printed and returned nothing; what its calls returned follows.)",
        }
        .into();
    }
    let text = spill::shorten(&text, MAX_OUTPUT_BYTES, NAME);
    let mut output = if failure.is_some() { ToolOutput::error(text) } else { ToolOutput::ok(text) };
    output.images = media.blocks;
    output
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
            let mut output = ToolOutput::ok(format!("{} {}", self.name, input));
            for kind in ["image", "document"] {
                let n = input[kind].as_u64().unwrap_or(0);
                output.images.extend((0..n).map(|i| json!({"type": kind, "source": {"data": i}})));
            }
            output
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
    async fn what_a_script_defines_stays_until_the_context_is_renewed() {
        let mut registry = registry();
        let out = run(&registry, json!({"code": "const seen = await tools.look({n: 1})", "description": "Look"})).await;
        assert_eq!(out, ToolOutput::ok("(The script printed and returned nothing.)"));
        let out = run(&registry, json!({"code": "seen"})).await;
        assert_eq!(out, ToolOutput::ok("look {\"n\":1}"));
        // As `/clear` does.
        registry.renew_code();
        let out = run(&registry, json!({"code": "typeof seen"})).await;
        assert_eq!(out, ToolOutput::ok("undefined"));
    }

    #[tokio::test]
    async fn images_and_documents_the_calls_return_come_back_up_to_a_limit() {
        let code = "await tools.look({image: 6}); await tools.look({image: 5, document: 5}); 'read'";
        let out = run(&registry(), json!({"code": code})).await;
        let kinds: Vec<&str> = out.images.iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(kinds.iter().filter(|k| **k == "image").count(), MAX_IMAGES);
        assert_eq!(kinds.iter().filter(|k| **k == "document").count(), MAX_DOCUMENTS);
        assert!(!out.is_error);
        assert!(out.content.starts_with("read\n\n(3 image(s) and 1 PDF document(s) the script's calls"), "{}", out.content);
        let out = run(&registry(), json!({"code": "const r = await tools.look({image: 1})"})).await;
        assert_eq!(out.images.len(), 1);
        assert!(out.content.contains("what its calls returned follows"), "{}", out.content);
    }

    #[tokio::test]
    async fn scripts_get_tool_output_whole() {
        let text = "x".repeat(200_000);
        let whole = spill::whole(async { spill::shorten(&text, 1000, "t") }).await;
        assert_eq!(whole.len(), text.len());
    }
}
