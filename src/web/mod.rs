//! Web frontend: `harness --web[=ADDR]` serves a browser UI (and the JSON
//! API behind it) for configuring model gateways, dispatching tasks to the
//! main agent or a subagent type, and calling tools directly.
//!
//! The API can run shell commands, so it is guarded: every API request needs
//! a token (`HARNESS_WEB_TOKEN`, or one generated at startup), since other
//! users of the machine can reach a loopback port too; on loopback, requests
//! must also name a loopback host (no DNS rebinding); state-changing
//! requests must carry an `X-Harness` header, which a cross-site form or
//! script cannot send without a CORS preflight.

mod tasks;

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::Agent;
use crate::llm::{Client, Providers};
use crate::mentions;
use crate::settings::{self, Settings};
use crate::skills::Skill;
use crate::subagent::{AgentDef, AgentTool, MessageAgentTool, Reporter, StopAgentTool};
use crate::tools::{FileTracker, ToolRegistry};

use tasks::{Task, Work};

const INDEX_HTML: &str = include_str!("index.html");
pub const DEFAULT_ADDR: &str = "127.0.0.1:8787";

/// Everything the server needs from startup.
pub struct Setup {
    /// `Err` when no provider could be loaded; the UI can fix the config.
    pub providers: Result<Providers>,
    /// Built-in, skill and MCP tools; the `agent` tool is added here, since
    /// it depends on the providers. Direct tool calls share its `read` /
    /// `write` file tracker.
    pub tools: ToolRegistry,
    pub skills: Arc<Vec<Skill>>,
    pub agent_defs: Arc<Vec<AgentDef>>,
    /// MCP server statuses, as shown in the UI.
    pub mcp: Value,
    pub reporter: Reporter,
}

/// The parts that change when the providers are reloaded.
struct Live {
    /// The default client and the `agent` tool built on it, or why no
    /// provider could be loaded.
    model: Result<(Client, Arc<AgentTool>), String>,
    /// `base_tools` plus the `agent` tool.
    tools: ToolRegistry,
}

impl Live {
    fn client(&self) -> ApiResult<&Client> {
        self.model.as_ref().map(|(client, _)| client).map_err(|e| {
            bad_request(format!("no usable model: {e}; configure one on the Models page"))
        })
    }
}

struct AppState {
    base_tools: ToolRegistry,
    skills: Arc<Vec<Skill>>,
    agent_defs: Arc<Vec<AgentDef>>,
    mcp: Value,
    reporter: Reporter,
    live: RwLock<Live>,
    tasks: Mutex<BTreeMap<u64, Arc<Task>>>,
    next_id: AtomicU64,
    token: String,
    /// Listening on loopback only: requests must name a loopback host.
    loopback: bool,
}

type Shared = Arc<AppState>;

/// `--web` or `--web=ADDR` among the arguments. Nothing else may be given
/// with it.
pub fn parse_flag(args: &[String]) -> Result<Option<String>> {
    let Some(flag) = args.iter().find(|a| *a == "--web" || a.starts_with("--web=")) else {
        return Ok(None);
    };
    if args.len() > 1 {
        bail!("--web takes no other arguments");
    }
    let addr = flag.strip_prefix("--web=").unwrap_or(DEFAULT_ADDR);
    Ok(Some(addr.to_string()))
}

pub async fn serve(addr: &str, setup: Setup) -> Result<()> {
    let addr: SocketAddr = tokio::net::lookup_host(addr)
        .await
        .with_context(|| format!("resolving {addr}"))?
        .next()
        .ok_or_else(|| anyhow!("{addr} resolves to no address"))?;
    let loopback = addr.ip().is_loopback();
    let token = std::env::var("HARNESS_WEB_TOKEN").ok().filter(|t| !t.is_empty()).unwrap_or_else(random_token);

    let live = build_live(&setup.tools, &setup.agent_defs, &setup.reporter, setup.providers);
    if let Err(e) = &live.model {
        eprintln!("\x1b[33mno usable model yet ({e}); configure one in the web UI\x1b[0m");
    }
    let state = Arc::new(AppState {
        base_tools: setup.tools,
        skills: setup.skills,
        agent_defs: setup.agent_defs,
        mcp: setup.mcp,
        reporter: setup.reporter,
        live: RwLock::new(live),
        tasks: Mutex::new(BTreeMap::new()),
        next_id: AtomicU64::new(1),
        token,
        loopback,
    });

    let app = Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/api/info", get(info))
        .route("/api/config", post(save_config))
        .route("/api/tools", get(list_tools))
        .route("/api/tools/{name}", post(call_tool))
        .route("/api/tasks", get(list_tasks).post(create_task))
        .route("/api/tasks/{id}", get(get_task).delete(delete_task))
        .route("/api/tasks/{id}/messages", post(send_message))
        .route("/api/tasks/{id}/model", post(switch_model))
        .route("/api/tasks/{id}/cancel", post(cancel_task))
        .route("/api/tasks/{id}/events", get(task_events))
        .route("/api/tasks/{id}/subagents", get(list_subagents))
        .route("/api/tasks/{id}/subagents/{agent_id}/stop", post(stop_subagent))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
    let shown = if loopback { format!("http://localhost:{}", addr.port()) } else { format!("http://{addr}") };
    println!("harness web UI: {shown}/#token={}", state.token);
    println!("\x1b[90mCtrl-C to stop\x1b[0m");
    // Open event streams never finish, so stop without waiting for them.
    tokio::select! {
        result = axum::serve(listener, app) => result.context("web server failed")?,
        _ = tokio::signal::ctrl_c() => {}
    }
    for task in state.tasks.lock().unwrap().values() {
        task.cancel();
    }
    Ok(())
}

fn build_live(base: &ToolRegistry, defs: &Arc<Vec<AgentDef>>, reporter: &Reporter, providers: Result<Providers>) -> Live {
    let mut tools = base.clone();
    let model = providers.and_then(Client::new).map_err(|e| format!("{e:#}")).map(|client| {
        let agent_tool = Arc::new(AgentTool::new(defs.clone(), base.clone(), client.clone(), reporter.clone()));
        tools.register_arc(agent_tool.clone());
        tools.register(MessageAgentTool);
        tools.register(StopAgentTool);
        (client, agent_tool)
    });
    Live { model, tools }
}

/// The tool set for a task's agent: a subagent type's own set, or every
/// tool; the file tools share the task's own file tracker.
fn task_tools(live: &Live, def: Option<&AgentDef>, files: &FileTracker) -> ToolRegistry {
    let mut tools = match (def, &live.model) {
        (Some(def), Ok((_, agent_tool))) => agent_tool.registry_for(def),
        _ => live.tools.clone(),
    };
    tools.track_files(files);
    tools
}

fn random_token() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).is_err() {
        // No /dev/urandom: fall back to the randomly keyed std hasher.
        use std::hash::{BuildHasher, Hasher};
        for chunk in bytes.chunks_mut(8) {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
            chunk.copy_from_slice(&h.finish().to_le_bytes());
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Request guard

fn is_loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };
    matches!(name, "localhost" | "127.0.0.1" | "::1") || name.ends_with(".localhost")
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn guard(State(state): State<Shared>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    // A loopback-only server rejects other host names (DNS rebinding).
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    if state.loopback && !is_loopback_host(host) {
        return (StatusCode::FORBIDDEN, "host not allowed").into_response();
    }
    if req.uri().path().starts_with("/api/") {
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "));
        // EventSource cannot set headers, so the token may come as `?token=`.
        let query = req.uri().query().unwrap_or("");
        let from_query = query.split('&').find_map(|kv| kv.strip_prefix("token="));
        let given = bearer.or(from_query).unwrap_or("");
        if !constant_time_eq(given.as_bytes(), state.token.as_bytes()) {
            return api_error(StatusCode::UNAUTHORIZED, "missing or wrong token; open the URL the server printed at startup").into_response();
        }
        if req.method() != Method::GET && !headers.contains_key("x-harness") {
            return api_error(StatusCode::FORBIDDEN, "missing X-Harness header").into_response();
        }
    }
    next.run(req).await
}

// ---------------------------------------------------------------------------
// Errors

type ApiResult<T> = Result<T, (StatusCode, Json<Value>)>;

fn api_error(status: StatusCode, message: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error": message.to_string()})))
}

fn bad_request(message: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    api_error(StatusCode::BAD_REQUEST, message)
}

fn find_task(state: &AppState, id: u64) -> ApiResult<Arc<Task>> {
    state.tasks.lock().unwrap().get(&id).cloned().ok_or_else(|| api_error(StatusCode::NOT_FOUND, "no such task"))
}

// ---------------------------------------------------------------------------
// Info and model configuration

/// First and last few characters of a secret.
fn mask(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 10 {
        return "••••".into();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

fn providers_json(providers: &Providers) -> Value {
    let list: Vec<Value> = providers
        .names()
        .into_iter()
        .filter_map(|name| {
            let p = providers.provider(name)?;
            Some(json!({
                "name": name,
                "protocol": p.protocol.as_str(),
                "base_url": p.base_url,
                "api_key": p.api_key.as_deref().map(mask),
                "models": p.models.keys().collect::<Vec<_>>(),
            }))
        })
        .collect();
    json!({
        "default_model": providers.default_model,
        "subagent_model": providers.subagent_model,
        "effort": providers.effort.as_str(),
        "providers": list,
        "models": providers.listed_models(),
    })
}

async fn info(State(state): State<Shared>) -> Json<Value> {
    let path = settings::path();
    let text = path.and_then(|p| std::fs::read_to_string(p).ok());
    let live = state.live.read().unwrap();
    let agents: Vec<Value> = state
        .agent_defs
        .iter()
        .map(|d| {
            json!({
                "name": d.name,
                "description": d.description,
                "permission": d.permission.as_str(),
                "tools": d.tools,
                "model": d.model,
                "source": d.path.as_ref().map(|p| p.display().to_string()),
            })
        })
        .collect();
    let skills: Vec<Value> = state
        .skills
        .iter()
        .map(|s| json!({"name": s.name, "description": s.description, "dir": s.dir.display().to_string()}))
        .collect();
    Json(json!({
        "cwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
        "config": {
            "path": path.map(|p| std::path::absolute(p).unwrap_or(p.into()).display().to_string()),
            "text": text,
            "error": live.model.as_ref().err(),
            "active": live.model.as_ref().ok().map(|(c, _)| providers_json(c.providers())),
        },
        "agents": agents,
        "skills": skills,
        "mcp": state.mcp,
    }))
}

#[derive(Deserialize)]
struct SaveConfig {
    text: String,
}

/// Validates the edited settings file with the environment, saves it and
/// switches to the new providers. MCP servers and the sandbox are set up at
/// startup, so changes to those apply on the next start.
async fn save_config(State(state): State<Shared>, Json(req): Json<SaveConfig>) -> ApiResult<Json<Value>> {
    let path = settings::path().ok_or_else(|| bad_request("no settings file: HOME is not set"))?;
    let providers = Settings::parse(&req.text)
        .and_then(|s| Providers::from_settings(&s))
        .map_err(|e| bad_request(format!("{e:#}")))?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| bad_request(format!("creating {}: {e}", dir.display())))?;
    }
    write_private(path, &req.text).map_err(|e| bad_request(format!("writing {}: {e}", path.display())))?;

    let live = build_live(&state.base_tools, &state.agent_defs, &state.reporter, Ok(providers));
    // Idle tasks move to the new providers, keeping their model if it still
    // resolves; running ones keep the old config for this conversation.
    let tasks: Vec<Arc<Task>> = state.tasks.lock().unwrap().values().cloned().collect();
    let mut skipped = 0;
    if let Ok((client, _)) = &live.model {
        for task in tasks {
            let Ok(mut agent) = task.agent.try_lock() else {
                skipped += 1;
                continue;
            };
            task.set_client(&mut agent, task.current_client(client));
            agent.set_tools(task_tools(&live, task.def.as_ref(), &task.files));
        }
    }
    *state.live.write().unwrap() = live;
    let info = info(State(state)).await.0;
    Ok(Json(json!({"info": info, "running_tasks_kept_old_config": skipped})))
}

/// Writes a file only its owner can read, as it may hold API keys; an
/// existing file is narrowed to that too.
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(text.as_bytes())
}

// ---------------------------------------------------------------------------
// Tools

async fn list_tools(State(state): State<Shared>) -> Json<Value> {
    let live = state.live.read().unwrap();
    let tools: Vec<Value> = live
        .tools
        .definitions()
        .into_iter()
        .map(|d| {
            let read_only = live.tools.get(&d.name).is_some_and(|t| t.read_only());
            json!({"name": d.name, "description": d.description, "schema": d.input_schema, "read_only": read_only})
        })
        .collect();
    Json(Value::Array(tools))
}

#[derive(Deserialize)]
struct CallTool {
    #[serde(default)]
    input: Value,
}

async fn call_tool(
    State(state): State<Shared>,
    Path(name): Path<String>,
    Json(req): Json<CallTool>,
) -> ApiResult<Json<Value>> {
    let tool = state.live.read().unwrap().tools.get(&name).cloned();
    let tool = tool.ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("unknown tool {name}")))?;
    let input = if req.input.is_null() { json!({}) } else { req.input };
    let started = std::time::Instant::now();
    let out = tool.run(input).await;
    Ok(Json(json!({
        "content": out.content,
        "is_error": out.is_error,
        "images": out.images,
        "elapsed_ms": started.elapsed().as_millis() as u64,
    })))
}

// ---------------------------------------------------------------------------
// Tasks

/// The message content for `line`: `/<skill> args` loads that skill, and
/// `@path` mentions are attached. Also returns the user event for the log.
fn user_message(state: &AppState, line: &str, files: &FileTracker) -> ApiResult<(Vec<Value>, Value)> {
    let prompt = match line.strip_prefix('/') {
        Some(command) => {
            let (name, args) = command.split_once(char::is_whitespace).unwrap_or((command, ""));
            let skill = state
                .skills
                .iter()
                .find(|s| s.name == name)
                .ok_or_else(|| bad_request(format!("unknown skill /{name}")))?;
            skill.invocation_prompt(args).map_err(bad_request)?
        }
        None => line.to_string(),
    };
    let expanded = mentions::expand(line, files);
    let event = json!({
        "type": "user",
        "text": line,
        "attachments": expanded.attachments.iter().map(|a| a.label.clone()).collect::<Vec<_>>(),
        "warnings": expanded.warnings,
    });
    Ok((expanded.into_content(&prompt), event))
}

fn title_for(prompt: &str) -> String {
    crate::util::ellipsize(prompt.lines().next().unwrap_or("").trim(), 60)
}

async fn list_tasks(State(state): State<Shared>) -> Json<Value> {
    let tasks = state.tasks.lock().unwrap();
    Json(Value::Array(tasks.values().rev().map(|t| t.summary()).collect()))
}

async fn get_task(State(state): State<Shared>, Path(id): Path<u64>) -> ApiResult<Json<Value>> {
    Ok(Json(find_task(&state, id)?.summary()))
}

async fn list_subagents(State(state): State<Shared>, Path(id): Path<u64>) -> ApiResult<Json<Value>> {
    Ok(Json(Value::Array(find_task(&state, id)?.subagents.list())))
}

async fn stop_subagent(
    State(state): State<Shared>,
    Path((id, agent_id)): Path<(u64, String)>,
) -> ApiResult<Json<Value>> {
    let task = find_task(&state, id)?;
    let stopped = task.subagents.stop(&agent_id, true).map_err(|e| api_error(StatusCode::NOT_FOUND, format!("{e:#}")))?;
    Ok(Json(json!({"stopped": stopped})))
}

#[derive(Deserialize)]
struct NewTask {
    prompt: String,
    /// `main` (default) or a subagent type.
    agent: Option<String>,
    model: Option<String>,
    title: Option<String>,
}

async fn create_task(State(state): State<Shared>, Json(req): Json<NewTask>) -> ApiResult<Json<Value>> {
    let prompt = req.prompt.trim();
    if prompt.is_empty() {
        return Err(bad_request("the prompt is empty"));
    }
    let def = match req.agent.as_deref().map(str::trim).filter(|a| !a.is_empty() && *a != "main") {
        Some(name) => Some(
            state
                .agent_defs
                .iter()
                .find(|d| d.name == name)
                .cloned()
                .ok_or_else(|| bad_request(format!("unknown agent type {name}")))?,
        ),
        None => None,
    };
    let files = FileTracker::default();
    let agent = {
        let live = state.live.read().unwrap();
        let client = live.client()?;
        // The chosen model, else what the `agent` tool would use.
        let client = match (req.model.as_deref().map(str::trim).filter(|m| !m.is_empty()), &def, &live.model) {
            (Some(spec), _, _) => client.switch(spec),
            (None, Some(def), Ok((_, agent_tool))) => agent_tool.client_for(def),
            _ => Ok(client.clone()),
        }
        .map_err(|e| bad_request(format!("{e:#}")))?;
        let tools = task_tools(&live, def.as_ref(), &files);
        let system = match &def {
            Some(def) => crate::prompt::subagent(def),
            None => crate::prompt::main(&tools),
        };
        Agent::new(client, tools, system)
    };
    let (content, user) = user_message(&state, prompt, &files)?;
    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    let title = req.title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| title_for(prompt));
    let task = Task::new(id, title, def, files, agent);
    state.tasks.lock().unwrap().insert(id, task.clone());
    task.start_turn(content, user).map_err(bad_request)?;
    Ok(Json(task.summary()))
}

#[derive(Deserialize)]
struct NewMessage {
    prompt: String,
}

async fn send_message(
    State(state): State<Shared>,
    Path(id): Path<u64>,
    Json(req): Json<NewMessage>,
) -> ApiResult<Json<Value>> {
    let task = find_task(&state, id)?;
    let prompt = req.prompt.trim();
    if prompt.is_empty() {
        return Err(bad_request("the prompt is empty"));
    }
    let (work, user) = match prompt.strip_prefix("/compact").filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        Some(focus) => {
            let focus = Some(focus.trim()).filter(|f| !f.is_empty()).map(String::from);
            (Work::Compact(focus), json!({"type": "user", "text": prompt, "attachments": [], "warnings": []}))
        }
        None => {
            let (content, user) = user_message(&state, prompt, &task.files)?;
            (Work::Turn(content), user)
        }
    };
    task.start(work, user).map_err(|e| api_error(StatusCode::CONFLICT, e))?;
    Ok(Json(task.summary()))
}

#[derive(Deserialize)]
struct SwitchModel {
    model: String,
}

async fn switch_model(
    State(state): State<Shared>,
    Path(id): Path<u64>,
    Json(req): Json<SwitchModel>,
) -> ApiResult<Json<Value>> {
    let task = find_task(&state, id)?;
    let client = {
        // Resolve against the task's current provider, as `/model` does.
        task.current_client(state.live.read().unwrap().client()?)
    };
    let client = client.switch(&req.model).map_err(|e| bad_request(format!("{e:#}")))?;
    let mut agent =
        task.agent.try_lock().map_err(|_| api_error(StatusCode::CONFLICT, "cannot switch models while running"))?;
    task.set_client(&mut agent, client);
    drop(agent);
    Ok(Json(task.summary()))
}

async fn cancel_task(State(state): State<Shared>, Path(id): Path<u64>) -> ApiResult<Json<Value>> {
    let task = find_task(&state, id)?;
    if !task.cancel() {
        return Err(api_error(StatusCode::CONFLICT, "the task is not running"));
    }
    Ok(Json(task.summary()))
}

async fn delete_task(State(state): State<Shared>, Path(id): Path<u64>) -> ApiResult<Json<Value>> {
    let task = state.tasks.lock().unwrap().remove(&id).ok_or_else(|| api_error(StatusCode::NOT_FOUND, "no such task"))?;
    task.cancel();
    // Open event streams may keep the task alive for a while; its subagents
    // stop now.
    task.subagents.reset();
    // Once the cancelled turn lets go of the agent.
    tokio::spawn(async move {
        task.agent.lock().await.end_session("other").await;
    });
    Ok(Json(json!({"deleted": id})))
}

#[derive(Deserialize)]
struct EventsQuery {
    /// Last sequence number already seen.
    after: Option<usize>,
}

/// Server-sent events: the task's log from the given point, then live
/// events. Each event's id is its sequence number, so a reconnecting
/// `EventSource` resumes where it left off (`Last-Event-ID`).
async fn task_events(
    State(state): State<Shared>,
    Path(id): Path<u64>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let task = find_task(&state, id)?;
    let last_seen = headers
        .get("last-event-id")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.parse::<usize>().ok())
        .or(query.after);
    let from = last_seen.map_or(0, |n| n + 1);
    let (backlog, rx) = task.subscribe(from);
    drop(task);
    // A receiver that lags behind ends the stream; the browser reconnects
    // and replays the missed events from the log.
    let live = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.ok().map(|item| (item, rx))
    });
    let stream = futures::stream::iter(backlog)
        .chain(live)
        .map(|(seq, event)| Ok(Event::default().id(seq.to_string()).data(&*event)));
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts() {
        for host in ["localhost:8787", "127.0.0.1:1", "[::1]:8787", "localhost", "app.localhost:80"] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in ["evil.com", "127.0.0.1.evil.com:80", "", "[::2]:1"] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }

    #[test]
    fn web_flag() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_flag(&args(&[])).unwrap(), None);
        assert_eq!(parse_flag(&args(&["fix it"])).unwrap(), None);
        assert_eq!(parse_flag(&args(&["--web"])).unwrap().as_deref(), Some(DEFAULT_ADDR));
        assert_eq!(parse_flag(&args(&["--web=0.0.0.0:9000"])).unwrap().as_deref(), Some("0.0.0.0:9000"));
        assert!(parse_flag(&args(&["--web", "hi"])).is_err());
    }

    #[test]
    fn masks_secrets() {
        assert_eq!(mask("short"), "••••");
        assert_eq!(mask("sk-ant-api03-abcdefgh"), "sk-a…efgh");
    }

    #[test]
    fn private_config_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        write_private(&path, "{}").unwrap();
        assert_eq!(mode(&path), 0o600);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, "{\"a\": 1}").unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\": 1}");
    }

    #[test]
    fn titles() {
        assert_eq!(title_for("fix the bug\nmore"), "fix the bug");
        assert_eq!(title_for(&"字".repeat(70)).chars().count(), 61);
    }
}
