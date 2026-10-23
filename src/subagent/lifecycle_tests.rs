//! Subagent runs against a local server speaking the Anthropic streaming
//! protocol: foreground and background runs, follow-ups, stopping, and how
//! reports and messages reach the agents' inboxes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use super::*;
use crate::agent::mock::texts as sent;
use crate::agent::{INTERRUPTED, Inbox, Observer, is_timestamp};
use crate::llm::Providers;
use crate::llm::config::tests::{file, no_env};

struct Reply {
    blocks: Vec<Value>,
    stop: &'static str,
    delay: Duration,
}

fn answer(text: &str) -> Reply {
    Reply { blocks: vec![json!({"type": "text", "text": text})], stop: "end_turn", delay: Duration::ZERO }
}

fn slow(text: &str, delay: Duration) -> Reply {
    Reply { delay, ..answer(text) }
}

fn calls(blocks: Vec<Value>) -> Reply {
    Reply { blocks, stop: "tool_use", ..answer("") }
}

fn call(id: &str, name: &str, input: Value) -> Value {
    json!({"type": "tool_use", "id": id, "name": name, "input": input})
}

fn messages(body: &Value) -> usize {
    body["messages"].as_array().map_or(0, Vec::len)
}

fn text(prompt: &str) -> Vec<Value> {
    vec![json!({"type": "text", "text": prompt})]
}

/// A model server that answers each request with `reply(body)` and hands
/// the request bodies to the test.
struct Mock {
    client: Client,
    requests: mpsc::UnboundedReceiver<Value>,
    server: tokio::task::JoinHandle<()>,
}

impl Mock {
    async fn new(reply: impl Fn(&Value) -> Reply + Send + Sync + 'static) -> Self {
        let (tx, requests) = mpsc::unbounded_channel();
        let reply = Arc::new(reply);
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                let response = reply(&body);
                let _ = tx.send(body);
                async move {
                    tokio::time::sleep(response.delay).await;
                    let mut events = vec![json!({"type": "message_start", "message": {"content": []}})];
                    for (index, block) in response.blocks.into_iter().enumerate() {
                        let partial = block["input"].to_string();
                        let is_call = block["type"] == "tool_use";
                        let start = if is_call { json!({"type": "tool_use", "id": block["id"], "name": block["name"], "input": {}}) } else { block };
                        events.push(json!({"type": "content_block_start", "index": index, "content_block": start}));
                        if is_call {
                            events.push(json!({"type": "content_block_delta", "index": index,
                                "delta": {"type": "input_json_delta", "partial_json": partial}}));
                        }
                        events.push(json!({"type": "content_block_stop", "index": index}));
                    }
                    events.push(json!({"type": "message_delta", "delta": {"stop_reason": response.stop}}));
                    events.push(json!({"type": "message_stop"}));
                    let sse: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
                    ([("content-type", "text/event-stream")], sse)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let config = file(json!({"model": "mock/main", "providers": {
            "mock": {"protocol": "anthropic", "base_url": url, "api_key": "fixture-only"}
        }}));
        let client = Client::new(Providers::from_parts(&config, &no_env).unwrap()).unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { client, requests, server }
    }

    async fn next(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv()).await.unwrap().unwrap()
    }

    /// The main agent's tools: the built-ins plus the subagent tools.
    fn tools(&self) -> ToolRegistry {
        let mut tools = ToolRegistry::with_defaults(FileTracker::default());
        let defs = Arc::new(vec![explore(), general_purpose()]);
        tools.register(AgentTool::new(defs, tools.clone(), self.client.clone(), silent()));
        tools.register(MessageAgentTool);
        tools.register(StopAgentTool);
        tools
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn silent() -> Reporter {
    Arc::new(|_: &Value| {})
}

struct Quiet;

impl Observer for Quiet {
    fn on_text_delta(&mut self, _text: &str) {}
    fn on_text_end(&mut self) {}
    fn on_tool_call(&mut self, _name: &str, _input: &Value) {}
    fn on_tool_result(&mut self, _name: &str, _output: &ToolOutput) {}
}

fn conversation() -> (Arc<Inbox>, Arc<Subagents>) {
    let inbox = Arc::new(Inbox::default());
    (inbox.clone(), Arc::new(Subagents::new(inbox, Arc::default(), Arc::default(), Hooks::default())))
}

fn first_id(subagents: &Subagents) -> String {
    subagents.list()[0]["id"].as_str().unwrap().to_string()
}

fn texts(inbox: &Inbox) -> String {
    inbox.take().iter().filter_map(|b| b["text"].as_str().map(String::from)).collect()
}

#[tokio::test]
async fn waits_for_mixed_tasks_and_does_not_replay_the_collected_report() {
    let mock = Mock::new(|_| slow("child evidence", Duration::from_millis(50))).await;
    let tools = mock.tools();
    let (inbox, subagents) = conversation();
    let shells = Arc::new(crate::tools::shells::Shells::new(inbox.clone()));
    scope(subagents.clone(), mock.client.clone(), false, crate::tools::shells::scope(shells, async {
        let out = tools.run("agent", json!({"agent_type": "explore", "description": "search", "prompt": "find", "run_in_background": true})).await;
        assert!(!out.is_error);
        let id = first_id(&subagents);
        let out = tools.run("bash", json!({"command": "sleep 0.1; echo shell-evidence", "run_in_background": true, "resource": "mixed-test:shell"})).await;
        assert!(!out.is_error);
        let out = tools.run("wait_tasks", json!({"task_ids": [id, "shell_1"], "mode": "all"})).await;
        assert!(!out.is_error, "{}", out.content);
        let result: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(result["reason"], "completed");
        assert_eq!(result["tasks"][0]["output"], "child evidence");
        assert!(result["tasks"][1]["output"].as_str().unwrap().contains("shell-evidence"));
        assert!(!inbox.wants_turn(), "both completion notifications were acknowledged");
        let out = tools.run("message_agent", json!({"agent_id": id, "message": "again", "run_in_background": true})).await;
        assert!(!out.is_error);
        tokio::time::timeout(Duration::from_secs(5), inbox.wanted()).await.unwrap();
        assert!(texts(&inbox).contains("child evidence"), "a new generation still notifies");
    })).await;
}

#[tokio::test]
async fn foreground_reports_come_back_as_text_and_can_be_continued() {
    let mut mock = Mock::new(|body| answer(if messages(body) > 1 { "follow-up report" } else { "first report" })).await;
    let tools = mock.tools();
    let (_, subagents) = conversation();
    // New subagents start on the conversation's current model, not on the
    // one the tool was built with.
    let current = mock.client.switch("switched").unwrap();
    let id = scope(subagents.clone(), current, false, async {
        let out = tools.run("agent", json!({"agent_type": "explore", "description": "find code", "prompt": "first"})).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("first report\n\n(agent_id: "), "{}", out.content);
        let body = mock.next().await;
        assert_eq!(body["model"], "switched");
        let names: Vec<&str> = body["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, ["glob", "grep"]);

        let id = first_id(&subagents);
        let out = tools.run("message_agent", json!({"agent_id": id, "message": "and then?"})).await;
        assert!(out.content.starts_with("follow-up report"), "{}", out.content);
        let body = mock.next().await;
        assert_eq!(messages(&body), 3, "it keeps its context");
        assert_eq!(body["messages"][1]["content"][0]["text"], "first report");
        assert_eq!(sent(&body["messages"][2]), ["and then?"]);
        assert_eq!(body["model"], "switched");
        id
    })
    .await;
    assert_eq!(subagents.list()[0]["status"], "completed");
    assert!(conversation().1.stop(&id, false).is_err(), "IDs belong to their conversation");
}

#[tokio::test]
async fn background_reports_arrive_after_the_turn_and_wake_the_parent() {
    let mut mock = Mock::new(|body| {
        if body["system"] != "parent" {
            return slow("child evidence", Duration::from_millis(200));
        }
        match messages(body) {
            1 => calls(vec![call("t1", "agent", json!({
                "agent_type": "explore", "description": "search", "prompt": "investigate", "run_in_background": true
            }))]),
            3 => answer("Started; I'll report back."),
            _ => answer("Summary from the child evidence"),
        }
    })
    .await;
    let mut parent = Agent::new(mock.client.clone(), mock.tools(), "parent");
    let woken = Arc::new(AtomicBool::new(false));
    let hook = woken.clone();
    parent.inbox().on_wake(move || hook.store(true, Ordering::SeqCst));

    parent.run_turn(text("delegate"), &mut Quiet).await.unwrap();
    assert_eq!(parent.last_text(0), "Started; I'll report back.");
    assert_eq!(parent.subagents().running_in_background(), 1, "it outlives the turn");

    tokio::time::timeout(Duration::from_secs(5), parent.inbox().wanted()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !woken.load(Ordering::SeqCst) { tokio::task::yield_now().await; }
    }).await.unwrap();
    assert!(woken.load(Ordering::SeqCst));
    assert_eq!(parent.subagents().running_in_background(), 0);
    parent.run_turn(Vec::new(), &mut Quiet).await.unwrap();
    assert_eq!(parent.last_text(0), "Summary from the child evidence");

    let mut bodies = Vec::new();
    for _ in 0..4 {
        bodies.push(mock.next().await);
    }
    let last = bodies.iter().find(|b| b["system"] == "parent" && messages(b) == 5).unwrap();
    let note = &sent(&last["messages"][4])[0];
    assert!(note.contains("<status>completed</status>") && note.contains("child evidence"), "{note}");
    // With nothing waiting, a turn without a prompt does nothing.
    parent.run_turn(Vec::new(), &mut Quiet).await.unwrap();
    assert_eq!(parent.messages().len(), 6);
}

#[tokio::test]
async fn a_running_subagent_reads_messages_at_its_next_step() {
    let mut mock = Mock::new(|body| {
        if body.to_string().contains("also check the tests") {
            answer("covered the code and the tests")
        } else {
            slow("covered the code", Duration::from_millis(300))
        }
    })
    .await;
    let tools = mock.tools();
    let (parent, subagents) = conversation();
    scope(subagents.clone(), mock.client.clone(), false, async {
        let started = tools
            .run("agent", json!({"agent_type": "explore", "description": "d", "prompt": "go", "run_in_background": true}))
            .await;
        assert!(started.content.contains("in the background"), "{}", started.content);
        mock.next().await;
        let id = first_id(&subagents);
        let out = tools.run("message_agent", json!({"agent_id": id, "message": "also check the tests"})).await;
        assert!(out.content.contains("(0 tool calls in 0s)") && out.content.contains("next step"), "{}", out.content);
    })
    .await;
    tokio::time::timeout(Duration::from_secs(5), parent.wanted()).await.unwrap();
    // The message came after its first answer, and it carried on.
    let body = mock.next().await;
    assert_eq!(messages(&body), 3);
    assert_eq!(body["messages"][1]["content"][0]["text"], "covered the code");
    assert!(sent(&body["messages"][2])[0].contains("also check the tests"));
    assert!(texts(&parent).contains("covered the code and the tests"));
}

#[tokio::test]
async fn stopped_subagents_keep_their_conversation() {
    let mut mock = Mock::new(|_| slow("slow", Duration::from_secs(60))).await;
    let tools = mock.tools();
    let (parent, subagents) = conversation();
    scope(subagents.clone(), mock.client.clone(), false, async {
        // Dropping a foreground call, as cancelling the parent's turn does,
        // stops its subagent.
        let call = tools.run("agent", json!({"agent_type": "explore", "description": "slow", "prompt": "go"}));
        tokio::select! {
            _ = call => panic!("the call finished"),
            _ = mock.next() => {}
        }
        assert_eq!(subagents.list()[0]["status"], "stopped");
        let id = first_id(&subagents);

        let out = tools.run("message_agent", json!({"agent_id": id, "message": "retry", "run_in_background": true})).await;
        assert!(out.content.contains("in the background"), "{}", out.content);
        let body = mock.next().await;
        assert_eq!(sent(&body["messages"][0])[1], INTERRUPTED, "the stopped run is noted");
        assert_eq!(sent(&body["messages"][1]), ["retry"]);

        let out = tools.run("stop_agent", json!({"agent_id": id})).await;
        assert!(out.content.starts_with("Stopped"), "{}", out.content);
        let out = tools.run("stop_agent", json!({"agent_id": id})).await;
        assert!(out.content.contains("not running"), "{}", out.content);
        assert!(parent.take().is_empty(), "the model knows what it stopped");

        // The user stopping one is news for the parent, but not worth a turn.
        tools.run("message_agent", json!({"agent_id": id, "message": "again", "run_in_background": true})).await;
        assert!(subagents.stop(&id, true).unwrap());
        assert!(!parent.wants_turn());
        assert!(texts(&parent).contains("<status>stopped</status>"));
    })
    .await;
}

#[tokio::test]
async fn running_subagents_are_limited_and_stop_with_the_conversation() {
    let mock = Mock::new(|body| {
        if body["system"] == "parent" && messages(body) == 1 {
            let spawn = |i: usize| {
                call(&format!("t{i}"), "agent", json!({
                    "agent_type": "explore", "description": "slow", "prompt": "go", "run_in_background": true
                }))
            };
            calls((0..=runtime::MAX_RUNNING).map(spawn).collect())
        } else if body["system"] == "parent" {
            answer("waiting")
        } else {
            slow("slow", Duration::from_secs(60))
        }
    })
    .await;
    let mut parent = Agent::new(mock.client.clone(), mock.tools(), "parent");
    parent.run_turn(text("fan out"), &mut Quiet).await.unwrap();
    let results = &parent.messages()[2].content;
    let refused: Vec<&Value> = results.iter().filter(|r| r["is_error"] == true).collect();
    assert_eq!(refused.len(), 1, "{results:?}");
    assert!(refused[0]["content"].as_str().unwrap().contains("already running"));
    assert_eq!(parent.subagents().running_in_background(), runtime::MAX_RUNNING);

    parent.clear();
    assert!(parent.subagents().list().is_empty());
    assert_eq!(parent.subagents().running_in_background(), 0);
}

/// Pushes a note into an inbox when called.
struct NoteTool(Arc<Inbox>);

#[async_trait]
impl Tool for NoteTool {
    fn name(&self) -> &str {
        "note"
    }
    fn description(&self) -> &str {
        "test"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn run(&self, _input: Value) -> ToolOutput {
        self.0.push("a report arrived".into(), true);
        ToolOutput::ok("noted")
    }
}

#[tokio::test]
async fn what_arrives_goes_with_the_next_request() {
    let mut mock = Mock::new(|body| match messages(body) {
        1 => calls(vec![call("t1", "note", json!({}))]),
        3 => slow("first answer", Duration::from_millis(300)),
        _ => answer("second answer"),
    })
    .await;
    let mut agent = Agent::new(mock.client.clone(), ToolRegistry::new(), "sys");
    let mut tools = ToolRegistry::new();
    tools.register(NoteTool(agent.inbox().clone()));
    agent.set_tools(tools);
    agent.inbox().push("waiting before".into(), false);

    let inbox = agent.inbox().clone();
    let late = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        inbox.push("arrived while answering".into(), true);
    });
    agent.run_turn(text("hi"), &mut Quiet).await.unwrap();
    late.await.unwrap();

    let first = mock.next().await;
    // The prompt notes the time; tool results and what arrives later don't.
    assert!(is_timestamp(&first["messages"][0]["content"][1]), "{first}");
    assert_eq!(sent(&first["messages"][0]), ["waiting before", "hi"]);
    // After the tool results; then the turn carries on instead of ending.
    let second = mock.next().await;
    assert_eq!(second["messages"][2]["content"].as_array().unwrap().len(), 2, "{second}");
    assert_eq!(second["messages"][2]["content"][1], json!({"type": "text", "text": "a report arrived"}));
    let third = mock.next().await;
    assert_eq!(third["messages"][4]["content"], json!([{"type": "text", "text": "arrived while answering"}]));
    assert_eq!(agent.last_text(0), "second answer");
    assert!(agent.inbox().take().is_empty());
}

#[tokio::test]
async fn general_purpose_gets_the_main_tools_but_cannot_delegate() {
    let mock = Mock::new(|_| answer("ok")).await;
    let tool = AgentTool::new(Arc::new(vec![general_purpose()]), mock.tools(), mock.client.clone(), silent());
    let registry = tool.registry_for(&general_purpose());
    assert_eq!(registry.names().collect::<Vec<_>>(), vec!["bash", "bash_output", "edit", "glob", "grep", "kill_shell", "read", "wait_tasks", "write"]);
}

#[tokio::test]
async fn a_continued_subagent_keeps_its_own_read_tracker() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("owned.txt");
    std::fs::write(&path, "before").unwrap();
    let target = path.clone();
    let mock = Mock::new(move |body| match messages(body) {
        1 => calls(vec![call("r", "read", json!({"path": target}))]),
        5 => calls(vec![call("w", "write", json!({"path": target, "content": "after"}))]),
        _ => answer("done"),
    })
    .await;
    let parent_files = FileTracker::default();
    let mut tools = ToolRegistry::with_defaults(parent_files.clone());
    tools.register(AgentTool::new(Arc::new(vec![general_purpose()]), tools.clone(), mock.client.clone(), silent()));
    tools.register(MessageAgentTool);
    let (_, subagents) = conversation();
    scope(subagents.clone(), mock.client.clone(), false, async {
        let first = tools.run("agent", json!({"agent_type": "general-purpose", "description": "d", "prompt": "read"})).await;
        assert!(!first.is_error, "{}", first.content);
        let second = tools.run("message_agent", json!({"agent_id": first_id(&subagents), "message": "now edit"})).await;
        assert!(!second.is_error, "{}", second.content);
    })
    .await;
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "after");
    assert!(parent_files.check_writable(&path).is_err(), "the subagent's reads are not the parent's");
}

#[tokio::test]
async fn outside_a_conversation_only_foreground_runs_work() {
    let mock = Mock::new(|_| answer("report")).await;
    let tools = mock.tools();
    let background = json!({"agent_type": "explore", "description": "d", "prompt": "p", "run_in_background": true});
    assert!(tools.run("agent", background).await.is_error);
    assert!(tools.run("message_agent", json!({"agent_id": "x", "message": "m"})).await.is_error);
    let out = tools.run("agent", json!({"agent_type": "explore", "description": "d", "prompt": "p"})).await;
    assert!(out.content.starts_with("report"), "{}", out.content);
}
