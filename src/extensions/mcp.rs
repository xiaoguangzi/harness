//! MCP (Model Context Protocol) client support.
//!
//! Servers are declared under `mcpServers` in the settings file
//! (`settings.rs`), in the same shape as Claude Code's `.mcp.json`:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "fs":     { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
//!     "remote": { "type": "http", "url": "https://example.com/mcp",
//!                 "headers": { "Authorization": "Bearer ${API_TOKEN}" } }
//!   }
//! }
//! ```
//!
//! Each server's tools are exposed to the model as `mcp__<server>__<tool>`.
//! `${VAR}` and `${VAR:-default}` in any string are expanded from the
//! environment.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation};
use rmcp::service::{RunningService, ServiceError};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::{Tool, ToolOutput, ToolRegistry, spill};
use crate::util::expand_env;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(600);
/// How long after a failed reconnect calls fail without trying again.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(10);
const MAX_OUTPUT_BYTES: usize = 100_000;
/// Anthropic tool names must match `^[a-zA-Z0-9_-]{1,64}$`.
const MAX_TOOL_NAME: usize = 64;

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// `stdio` (default when `command` is set) or `http`.
    #[serde(rename = "type")]
    kind: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// The directory a stdio server runs in, if not the working directory.
    cwd: Option<String>,
    url: Option<String>,
    #[serde(default)]
    pub(crate) headers: BTreeMap<String, String>,
    /// Its tools are always sent in full, never deferred behind
    /// `tool_search`.
    #[serde(rename = "alwaysLoad", default)]
    always_load: bool,
}

#[derive(Debug, PartialEq)]
enum Transport {
    Stdio { command: String, args: Vec<String>, env: BTreeMap<String, String>, cwd: Option<String> },
    Http { url: String, headers: BTreeMap<String, String> },
}

impl ServerConfig {
    fn transport(&self) -> Result<Transport> {
        let kind = self.kind.as_deref().unwrap_or(if self.url.is_some() { "http" } else { "stdio" });
        match kind {
            "stdio" => Ok(Transport::Stdio {
                command: expand_env(self.command.as_deref().context("stdio server needs `command`")?),
                args: self.args.iter().map(|a| expand_env(a)).collect(),
                env: self.env.iter().map(|(k, v)| (k.clone(), expand_env(v))).collect(),
                cwd: self.cwd.as_deref().map(expand_env),
            }),
            "http" | "streamable-http" => Ok(Transport::Http {
                url: expand_env(self.url.as_deref().context("http server needs `url`")?),
                headers: self.headers.iter().map(|(k, v)| (k.clone(), expand_env(v))).collect(),
            }),
            other => bail!("unsupported MCP transport type `{other}` (use `stdio` or `http`)"),
        }
    }
}

type Service = RunningService<RoleClient, ClientConfig>;

/// A connection to one MCP server, opened again when it is lost (the
/// server exited, or an HTTP session expired).
pub struct Server {
    pub name: String,
    config: ServerConfig,
    service: std::sync::Mutex<Arc<Service>>,
    /// Held while reconnecting; when the last attempt failed.
    reconnecting: tokio::sync::Mutex<Option<Instant>>,
}

impl Server {
    async fn connect(name: &str, config: &ServerConfig) -> Result<Self> {
        let service = open(name, config, false).await?;
        Ok(Self {
            name: name.to_string(),
            config: config.clone(),
            service: std::sync::Mutex::new(Arc::new(service)),
            reconnecting: tokio::sync::Mutex::new(None),
        })
    }

    fn service(&self) -> Arc<Service> {
        self.service.lock().unwrap().clone()
    }

    /// Calls `tool`. A connection found closed is opened again first; one
    /// lost during the call is opened again too, but only a read-only
    /// call is repeated, since another may already have taken effect.
    async fn call(&self, tool: &str, arguments: serde_json::Map<String, Value>, read_only: bool) -> ToolOutput {
        let params = CallToolRequestParams::new(tool.to_string()).with_arguments(arguments);
        let mut service = self.service();
        let mut reconnected = false;
        if service.is_closed() {
            service = match self.reconnect(&service).await {
                Ok(s) => s,
                Err(e) => return ToolOutput::error(e),
            };
            reconnected = true;
        }
        let result = loop {
            match tokio::time::timeout(CALL_TIMEOUT, service.call_tool(params.clone())).await {
                Ok(Ok(r)) => break r,
                Ok(Err(e @ (ServiceError::TransportClosed | ServiceError::TransportSend(_)))) if !reconnected => {
                    match self.reconnect(&service).await {
                        Ok(s) if read_only => (service, reconnected) = (s, true),
                        Ok(_) => {
                            return ToolOutput::error(format!(
                                "MCP call failed: {e}. The connection to `{}` was lost and has been opened again; \
                                 the call was not repeated, as it may already have taken effect.",
                                self.name
                            ));
                        }
                        Err(re) => return ToolOutput::error(format!("MCP call failed: {e}; {re}")),
                    }
                }
                Ok(Err(e @ (ServiceError::TransportClosed | ServiceError::TransportSend(_)))) => {
                    let log = log_path(&self.name);
                    return ToolOutput::error(format!("MCP call failed: {e}; the server `{}` went away again (log: {})", self.name, log.display()));
                }
                Ok(Err(e)) => return ToolOutput::error(format!("MCP call failed: {e}")),
                Err(_) => return ToolOutput::error("MCP call timed out"),
            }
        };
        let content: Vec<Value> =
            result.content.iter().filter_map(|c| serde_json::to_value(c).ok()).collect();
        let mut out = convert_content(&content);
        if content.is_empty()
            && let Some(structured) = &result.structured_content
        {
            out.content = structured.to_string();
        }
        out.content = spill::shorten(&out.content, MAX_OUTPUT_BYTES, "mcp");
        out.is_error = result.is_error.unwrap_or(false);
        out
    }

    /// Opens the connection again in place of `lost`, unless a concurrent
    /// call already did; after a failed attempt, not again for
    /// [`RECONNECT_BACKOFF`]. The tools stay as first listed.
    async fn reconnect(&self, lost: &Arc<Service>) -> Result<Arc<Service>, String> {
        let mut failed_at = self.reconnecting.lock().await;
        let current = self.service();
        if !Arc::ptr_eq(&current, lost) && !current.is_closed() {
            return Ok(current);
        }
        let log = log_path(&self.name);
        if let Some(at) = *failed_at
            && at.elapsed() < RECONNECT_BACKOFF
        {
            return Err(format!(
                "the MCP server `{}` is disconnected, and reconnecting failed {}s ago (log: {})",
                self.name,
                at.elapsed().as_secs(),
                log.display()
            ));
        }
        match open(&self.name, &self.config, true).await {
            Ok(service) => {
                let service = Arc::new(service);
                *self.service.lock().unwrap() = service.clone();
                *failed_at = None;
                Ok(service)
            }
            Err(e) => {
                *failed_at = Some(Instant::now());
                Err(format!("the MCP server `{}` disconnected, and reconnecting failed: {e:#} (log: {})", self.name, log.display()))
            }
        }
    }
}

/// Starts or reaches the server and initializes the session. A stdio
/// server's log starts afresh unless `append` (when reconnecting, so the
/// reason it went away stays).
async fn open(name: &str, config: &ServerConfig, append: bool) -> Result<Service> {
    let info = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("harness", env!("CARGO_PKG_VERSION")),
    );
    let service = match config.transport()? {
        Transport::Stdio { command, args, env, cwd } => {
            // Server logs go to a file so they don't interleave with the REPL.
            let log = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(append)
                .truncate(!append)
                .open(log_path(name))
                .context("creating MCP log file")?;
            let cmd = tokio::process::Command::new(&command).configure(|c| {
                c.args(&args).envs(&env);
                if let Some(cwd) = &cwd {
                    c.current_dir(cwd);
                }
                // Its own process group, so Ctrl-C in the terminal (which
                // interrupts a turn) doesn't reach the server.
                #[cfg(unix)]
                c.process_group(0);
            });
            let (transport, _) = TokioChildProcess::builder(cmd)
                .stderr(Stdio::from(log))
                .spawn()
                .with_context(|| format!("starting `{command}`"))?;
            tokio::time::timeout(CONNECT_TIMEOUT, info.serve(transport)).await
        }
        Transport::Http { url, headers } => {
            let mut custom = HashMap::new();
            for (k, v) in &headers {
                custom.insert(
                    reqwest::header::HeaderName::try_from(k.as_str())
                        .with_context(|| format!("invalid header name `{k}`"))?,
                    reqwest::header::HeaderValue::try_from(v.as_str())
                        .with_context(|| format!("invalid value for header `{k}`"))?,
                );
            }
            let config = StreamableHttpClientTransportConfig::with_uri(url).custom_headers(custom);
            let transport = StreamableHttpClientTransport::from_config(config);
            tokio::time::timeout(CONNECT_TIMEOUT, info.serve(transport)).await
        }
    };
    service
        .map_err(|_| anyhow!("timed out after {}s during initialize", CONNECT_TIMEOUT.as_secs()))?
        .map_err(|e| anyhow!("initialize failed: {e}"))
}

pub fn log_path(server: &str) -> PathBuf {
    std::env::temp_dir().join(format!("harness-mcp-{}.log", sanitize(server)))
}

/// Converts MCP content blocks (as JSON) into tool output: text is joined,
/// supported images become Anthropic image blocks, anything else is
/// described in a placeholder line.
fn convert_content(content: &[Value]) -> ToolOutput {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for block in content {
        match block["type"].as_str().unwrap_or("") {
            "text" => texts.push(block["text"].as_str().unwrap_or("").to_string()),
            "image" => {
                let mime = block["mimeType"].as_str().unwrap_or("");
                if matches!(mime, "image/png" | "image/jpeg" | "image/gif" | "image/webp") {
                    images.push(json!({
                        "type": "image",
                        "source": {"type": "base64", "media_type": mime, "data": block["data"]},
                    }));
                } else {
                    texts.push(format!("[image of unsupported type {mime} omitted]"));
                }
            }
            "resource" => {
                let r = &block["resource"];
                match r["text"].as_str() {
                    Some(t) => texts.push(t.to_string()),
                    None => texts.push(format!("[binary resource {} omitted]", r["uri"].as_str().unwrap_or("?"))),
                }
            }
            "resource_link" => texts.push(format!("[resource link: {}]", block["uri"].as_str().unwrap_or("?"))),
            other => texts.push(format!("[{other} content omitted]")),
        }
    }
    let mut out = ToolOutput::ok(texts.join("\n"));
    out.images = images;
    out
}

/// Keeps `[A-Za-z0-9_-]`, replacing everything else with `_`.
fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

fn exposed_name(server: &str, tool: &str) -> String {
    let mut name = format!("mcp__{}__{}", sanitize(server), sanitize(tool));
    name.truncate(MAX_TOOL_NAME);
    name
}

/// One MCP server tool, as seen by the model.
struct McpTool {
    server: Arc<Server>,
    tool: String,
    exposed: String,
    description: String,
    schema: Value,
    read_only: bool,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.exposed
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }

    fn eager_input_streaming(&self) -> bool {
        false
    }

    /// Trusts the server's `readOnlyHint`; unannotated tools count as writes.
    fn read_only(&self) -> bool {
        self.read_only
    }

    fn deferred(&self) -> bool {
        !self.server.config.always_load
    }

    async fn run(&self, input: Value) -> ToolOutput {
        match input {
            Value::Object(args) => self.server.call(&self.tool, args, self.read_only).await,
            _ => ToolOutput::error("Invalid input: expected a JSON object"),
        }
    }
}

/// Startup result for one configured server.
#[derive(Clone)]
pub struct Status {
    pub name: String,
    pub outcome: Result<Vec<String>, String>,
    /// How long connecting and listing its tools took, or failing to.
    pub took: Duration,
}

/// A connected server with its tools and status, to carry over into the
/// next [`Manager`].
type Reused = (Arc<Server>, Vec<Arc<McpTool>>, Status);

/// Connected servers, kept alive for the session.
#[derive(Default)]
pub struct Manager {
    servers: Vec<Arc<Server>>,
    /// Their tools, for each tool set made.
    tools: Vec<Arc<McpTool>>,
    pub statuses: Vec<Status>,
}

impl Manager {
    /// Connects to every configured server concurrently and lists their
    /// tools. A server that fails to start is reported, not fatal.
    pub async fn start(configs: &BTreeMap<String, ServerConfig>) -> Self {
        Self::start_reusing(configs.clone(), BTreeMap::new()).await
    }

    /// [`Manager::start`], taking the servers in `reused` as they are.
    async fn start_reusing(configs: BTreeMap<String, ServerConfig>, mut reused: BTreeMap<String, Reused>) -> Self {
        let fresh = configs.iter().filter(|(name, _)| !reused.contains_key(*name));
        let connections = futures::future::join_all(fresh.map(|(name, cfg)| async move {
            let started = Instant::now();
            let connection = async {
                let server = Server::connect(name, cfg).await?;
                // Every page counts: a server that keeps returning cursors
                // must not hold up startup either.
                let tools = tokio::time::timeout(CONNECT_TIMEOUT, server.service().list_all_tools())
                    .await
                    .map_err(|_| anyhow!("timed out after {}s listing tools", CONNECT_TIMEOUT.as_secs()))?
                    .map_err(|e| anyhow!("tools/list failed: {e}"))?;
                Ok::<_, anyhow::Error>((server, tools))
            }
            .await;
            (name.clone(), (connection, started.elapsed()))
        }))
        .await;
        let mut connections: BTreeMap<String, _> = connections.into_iter().collect();

        let mut manager = Self::default();
        for name in configs.keys() {
            if let Some((server, tools, status)) = reused.remove(name) {
                manager.servers.push(server);
                manager.tools.extend(tools);
                manager.statuses.push(status);
                continue;
            }
            let Some((connection, took)) = connections.remove(name) else { continue };
            let outcome = match connection {
                Ok((server, tools)) => {
                    let server = Arc::new(server);
                    let mut names = Vec::new();
                    for t in tools {
                        let exposed = exposed_name(&server.name, &t.name);
                        let description = t.description.as_deref().unwrap_or("").to_string();
                        manager.tools.push(Arc::new(McpTool {
                            server: server.clone(),
                            tool: t.name.to_string(),
                            exposed: exposed.clone(),
                            description: format!("[MCP server `{}`] {description}", server.name),
                            schema: Value::Object((*t.input_schema).clone()),
                            read_only: t.annotations.as_ref().and_then(|a| a.read_only_hint).unwrap_or(false),
                        }));
                        names.push(exposed);
                    }
                    manager.servers.push(server);
                    Ok(names)
                }
                Err(e) => Err(format!("{e:#}")),
            };
            manager.statuses.push(Status { name: name.clone(), outcome, took });
        }
        manager
    }

    /// The server `name` with its tools, if it is connected with `config`.
    fn reusable(&self, name: &str, config: &ServerConfig) -> Option<Reused> {
        let server = self.servers.iter().find(|s| s.name == name && s.config == *config && !s.service().is_closed())?;
        let tools = self.tools.iter().filter(|t| Arc::ptr_eq(&t.server, server)).cloned().collect();
        let status = self.statuses.iter().find(|s| s.name == name)?.clone();
        Some((server.clone(), tools, status))
    }

    /// Adds the servers' tools to `registry`.
    pub fn register(&self, registry: &mut ToolRegistry) {
        for tool in &self.tools {
            registry.register_arc(tool.clone());
        }
    }

    /// Closes every connection and waits for stdio servers to exit. Call it
    /// after the tool registries are dropped: a server still referenced by a
    /// registered tool, or carried over into another manager, is left open.
    pub async fn shutdown(self) {
        drop(self.tools);
        for server in self.servers {
            if let Ok(server) = Arc::try_unwrap(server)
                && let Ok(service) = Arc::try_unwrap(server.service.into_inner().unwrap())
            {
                let _ = service.cancel().await;
            }
        }
    }
}

/// MCP servers connecting in the background, or connected.
pub enum Servers {
    Connecting { configured: usize, task: tokio::task::JoinHandle<Manager> },
    Connected(Manager),
}

impl Default for Servers {
    fn default() -> Self {
        Self::Connected(Manager::default())
    }
}

impl Servers {
    /// Starts connecting to `configs` in the background. A server one of
    /// `reuse` has connected with the same config is taken over instead.
    pub fn connect(configs: &BTreeMap<String, ServerConfig>, reuse: &[&Manager]) -> Self {
        if configs.is_empty() {
            return Self::default();
        }
        let reused = configs
            .iter()
            .filter_map(|(name, cfg)| reuse.iter().find_map(|m| m.reusable(name, cfg)).map(|r| (name.clone(), r)))
            .collect();
        let task = tokio::spawn(Manager::start_reusing(configs.clone(), reused));
        Self::Connecting { configured: configs.len(), task }
    }

    /// Waits for the connections to finish.
    pub async fn wait(&mut self) -> &Manager {
        if let Self::Connecting { task, .. } = self {
            let manager = task.await.unwrap_or_default();
            *self = Self::Connected(manager);
        }
        match self {
            Self::Connected(manager) => manager,
            Self::Connecting { .. } => unreachable!("just connected"),
        }
    }

    pub fn connected(&self) -> Option<&Manager> {
        match self {
            Self::Connected(manager) => Some(manager),
            Self::Connecting { .. } => None,
        }
    }

    /// How many servers are configured.
    pub fn configured(&self) -> usize {
        match self {
            Self::Connected(manager) => manager.statuses.len(),
            Self::Connecting { configured, .. } => *configured,
        }
    }

    /// Closes the connections, or stops connecting (which stops the stdio
    /// servers started so far).
    pub async fn shutdown(self) {
        match self {
            Self::Connected(manager) => manager.shutdown().await,
            Self::Connecting { task, .. } => task.abort(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_and_infers_transport() {
        let parsed: crate::settings::Settings = serde_json::from_value(json!({"mcpServers": {
            "local": {"command": "srv", "args": ["--x"], "env": {"K": "V"}, "cwd": "/srv"},
            "remote": {"url": "https://example.com/mcp", "headers": {"X-Key": "k"}}
        }}))
        .unwrap();
        assert_eq!(
            parsed.mcp_servers["local"].transport().unwrap(),
            Transport::Stdio {
                command: "srv".into(),
                args: vec!["--x".into()],
                env: BTreeMap::from([("K".into(), "V".into())]),
                cwd: Some("/srv".into()),
            }
        );
        assert_eq!(
            parsed.mcp_servers["remote"].transport().unwrap(),
            Transport::Http {
                url: "https://example.com/mcp".into(),
                headers: BTreeMap::from([("X-Key".into(), "k".into())]),
            }
        );
    }

    #[test]
    fn rejects_bad_config() {
        let cfg: ServerConfig = serde_json::from_value(json!({"type": "sse", "url": "x"})).unwrap();
        assert!(cfg.transport().is_err());
        let cfg: ServerConfig = serde_json::from_value(json!({"args": []})).unwrap();
        assert!(cfg.transport().is_err());
    }

    #[test]
    fn tool_names_are_sanitized_and_capped() {
        assert_eq!(exposed_name("my server", "get.thing"), "mcp__my_server__get_thing");
        assert_eq!(exposed_name("s", &"x".repeat(100)).len(), MAX_TOOL_NAME);
    }

    /// A stdio MCP server: `pid` answers with its process id; `die` (not
    /// read-only) exits; `flaky` (read-only) exits the first time, and
    /// answers once `$MARKER` exists.
    const TEST_SERVER: &str = r#"
import json, os, sys
ro = {"readOnlyHint": True}
tools = [{"name": n, "inputSchema": {"type": "object"}, **({"annotations": ro} if n != "die" else {})} for n in ["pid", "die", "flaky"]]
for line in sys.stdin:
    req = json.loads(line)
    if "id" not in req:
        continue
    method, result = req["method"], None
    if method == "initialize":
        result = {"protocolVersion": req["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "t", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": tools}
    elif method == "tools/call":
        name = req["params"]["name"]
        if name == "die" or (name == "flaky" and not os.path.exists(os.environ["MARKER"])):
            open(os.environ["MARKER"], "w").close()
            os._exit(0)
        result = {"content": [{"type": "text", "text": str(os.getpid())}]}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": result}), flush=True)
"#;

    /// The test server as a config, or `None` without python3.
    fn test_server(dir: &std::path::Path, name: &str) -> Option<ServerConfig> {
        if !crate::extensions::lsp::command_exists("python3") {
            eprintln!("python3 not found; skipped");
            return None;
        }
        let script = dir.join("server.py");
        std::fs::write(&script, TEST_SERVER).unwrap();
        let marker = dir.join(format!("{name}.marker"));
        let config = json!({"command": "python3", "args": [script], "env": {"MARKER": marker}});
        Some(serde_json::from_value(config).unwrap())
    }

    async fn call(manager: &Manager, tool: &str) -> ToolOutput {
        let tool = manager.tools.iter().find(|t| t.exposed.ends_with(&format!("__{tool}"))).unwrap();
        tool.run(json!({})).await
    }

    #[tokio::test]
    async fn reopens_a_lost_connection_but_repeats_only_read_only_calls() {
        let dir = tempfile::tempdir().unwrap();
        let Some(config) = test_server(dir.path(), "a") else { return };
        let manager = Manager::start(&BTreeMap::from([("t".to_string(), config)])).await;
        assert!(manager.statuses[0].outcome.is_ok(), "{:?}", manager.statuses[0].outcome);
        let first = call(&manager, "pid").await.content;

        let died = call(&manager, "die").await;
        assert!(died.is_error && died.content.contains("not repeated"), "{}", died.content);
        let second = call(&manager, "pid").await;
        assert!(!second.is_error && second.content != first, "a new server: {first} then {}", second.content);

        // The marker exists now, so the repeated call is answered.
        std::fs::remove_file(dir.path().join("a.marker")).unwrap();
        let flaky = call(&manager, "flaky").await;
        assert!(!flaky.is_error && flaky.content != second.content, "{}", flaky.content);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn carries_over_servers_connected_alike() {
        let dir = tempfile::tempdir().unwrap();
        let Some(config) = test_server(dir.path(), "a") else { return };
        let other = test_server(dir.path(), "b").unwrap();
        let old = Manager::start(&BTreeMap::from([("t".to_string(), config.clone())])).await;
        let pid = call(&old, "pid").await.content;

        let mut same = Servers::connect(&BTreeMap::from([("t".to_string(), config)]), &[&old]);
        assert_eq!(call(same.wait().await, "pid").await.content, pid);
        let mut changed = Servers::connect(&BTreeMap::from([("t".to_string(), other)]), &[&old]);
        assert_ne!(call(changed.wait().await, "pid").await.content, pid);

        // Shutting the old manager down leaves the carried-over server open.
        old.shutdown().await;
        assert_eq!(call(same.wait().await, "pid").await.content, pid);
        same.shutdown().await;
        changed.shutdown().await;
    }

    #[test]
    fn converts_content_blocks() {
        let out = convert_content(&[
            json!({"type": "text", "text": "hello"}),
            json!({"type": "image", "mimeType": "image/png", "data": "AAAA"}),
            json!({"type": "image", "mimeType": "image/bmp", "data": "AAAA"}),
            json!({"type": "resource", "resource": {"uri": "file:///a", "text": "body"}}),
            json!({"type": "resource_link", "uri": "file:///b", "name": "b"}),
        ]);
        assert_eq!(
            out.content,
            "hello\n[image of unsupported type image/bmp omitted]\nbody\n[resource link: file:///b]"
        );
        assert_eq!(
            out.images,
            vec![json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}})]
        );
    }
}
