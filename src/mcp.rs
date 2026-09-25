//! MCP (Model Context Protocol) client support.
//!
//! Servers are declared in a JSON file using the same `mcpServers` shape as
//! Claude Code's `.mcp.json`:
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
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::{Tool, ToolOutput, ToolRegistry, truncate};
use crate::util::expand_env;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_OUTPUT_BYTES: usize = 100_000;
/// Anthropic tool names must match `^[a-zA-Z0-9_-]{1,64}$`.
const MAX_TOOL_NAME: usize = 64;

#[derive(Debug, Deserialize, Default)]
struct ConfigFile {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: BTreeMap<String, ServerConfig>,
}

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
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

#[derive(Debug, PartialEq)]
enum Transport {
    Stdio { command: String, args: Vec<String>, env: BTreeMap<String, String> },
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
            }),
            "http" | "streamable-http" => Ok(Transport::Http {
                url: expand_env(self.url.as_deref().context("http server needs `url`")?),
                headers: self.headers.iter().map(|(k, v)| (k.clone(), expand_env(v))).collect(),
            }),
            other => bail!("unsupported MCP transport type `{other}` (use `stdio` or `http`)"),
        }
    }
}

/// Loads server configs. `HARNESS_MCP_CONFIG` names a single file;
/// otherwise `~/.harness/mcp.json` and then `./.mcp.json` are merged, with
/// the project file winning on name clashes.
pub fn load_config() -> Result<BTreeMap<String, ServerConfig>> {
    let files: Vec<PathBuf> = match std::env::var_os("HARNESS_MCP_CONFIG") {
        Some(p) => vec![PathBuf::from(p)],
        None => {
            let mut v = Vec::new();
            if let Some(home) = std::env::var_os("HOME") {
                v.push(Path::new(&home).join(".harness/mcp.json"));
            }
            v.push(PathBuf::from(".mcp.json"));
            v
        }
    };
    let mut servers = BTreeMap::new();
    for file in files {
        if !file.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        let parsed: ConfigFile =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        servers.extend(parsed.mcp_servers);
    }
    Ok(servers)
}

/// A live connection to one MCP server.
pub struct Server {
    pub name: String,
    service: RunningService<RoleClient, ClientConfig>,
}

impl Server {
    async fn connect(name: &str, config: &ServerConfig) -> Result<Self> {
        let info = ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("harness", env!("CARGO_PKG_VERSION")),
        );
        let service = match config.transport()? {
            Transport::Stdio { command, args, env } => {
                // Server logs go to a file so they don't interleave with the REPL.
                let log = std::fs::File::create(log_path(name)).context("creating MCP log file")?;
                let cmd = tokio::process::Command::new(&command).configure(|c| {
                    c.args(&args).envs(&env);
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
        let service = service
            .map_err(|_| anyhow!("timed out after {}s during initialize", CONNECT_TIMEOUT.as_secs()))?
            .map_err(|e| anyhow!("initialize failed: {e}"))?;
        Ok(Self { name: name.to_string(), service })
    }

    async fn call(&self, tool: &str, arguments: serde_json::Map<String, Value>) -> ToolOutput {
        let params = CallToolRequestParams::new(tool.to_string()).with_arguments(arguments);
        let result = match tokio::time::timeout(CALL_TIMEOUT, self.service.call_tool(params)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return ToolOutput::error(format!("MCP call failed: {e}")),
            Err(_) => return ToolOutput::error("MCP call timed out"),
        };
        let content: Vec<Value> =
            result.content.iter().filter_map(|c| serde_json::to_value(c).ok()).collect();
        let mut out = convert_content(&content);
        if content.is_empty()
            && let Some(structured) = &result.structured_content
        {
            out.content = structured.to_string();
        }
        out.content = truncate(&out.content, MAX_OUTPUT_BYTES);
        out.is_error = result.is_error.unwrap_or(false);
        out
    }
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

    async fn run(&self, input: Value) -> ToolOutput {
        match input {
            Value::Object(args) => self.server.call(&self.tool, args).await,
            _ => ToolOutput::error("Invalid input: expected a JSON object"),
        }
    }
}

/// Startup result for one configured server.
pub struct Status {
    pub name: String,
    pub outcome: Result<Vec<String>, String>,
}

/// Connected servers, kept alive for the session.
#[derive(Default)]
pub struct Manager {
    servers: Vec<Arc<Server>>,
    pub statuses: Vec<Status>,
}

impl Manager {
    /// Connects to every configured server concurrently and registers their
    /// tools. A server that fails to start is reported, not fatal.
    pub async fn start(configs: &BTreeMap<String, ServerConfig>, registry: &mut ToolRegistry) -> Self {
        let connections = futures::future::join_all(configs.iter().map(|(name, cfg)| async move {
            let server = Server::connect(name, cfg).await?;
            // Every page counts: a server that keeps returning cursors must
            // not hold up startup either.
            let tools = tokio::time::timeout(CONNECT_TIMEOUT, server.service.list_all_tools())
                .await
                .map_err(|_| anyhow!("timed out after {}s listing tools", CONNECT_TIMEOUT.as_secs()))?
                .map_err(|e| anyhow!("tools/list failed: {e}"))?;
            Ok::<_, anyhow::Error>((server, tools))
        }))
        .await;

        let mut manager = Self::default();
        for ((name, _), connection) in configs.iter().zip(connections) {
            let outcome = match connection {
                Ok((server, tools)) => {
                    let server = Arc::new(server);
                    let mut names = Vec::new();
                    for t in tools {
                        let exposed = exposed_name(&server.name, &t.name);
                        let description = t.description.as_deref().unwrap_or("").to_string();
                        registry.register(McpTool {
                            server: server.clone(),
                            tool: t.name.to_string(),
                            exposed: exposed.clone(),
                            description: format!("[MCP server `{}`] {description}", server.name),
                            schema: Value::Object((*t.input_schema).clone()),
                            read_only: t.annotations.as_ref().and_then(|a| a.read_only_hint).unwrap_or(false),
                        });
                        names.push(exposed);
                    }
                    manager.servers.push(server);
                    Ok(names)
                }
                Err(e) => Err(format!("{e:#}")),
            };
            manager.statuses.push(Status { name: name.clone(), outcome });
        }
        manager
    }

    /// Closes every connection and waits for stdio servers to exit. Call it
    /// after the tool registry is dropped: a server still referenced by a
    /// registered tool is left to rmcp's drop-time cleanup instead.
    pub async fn shutdown(self) {
        for server in self.servers {
            if let Ok(server) = Arc::try_unwrap(server) {
                let _ = server.service.cancel().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_and_infers_transport() {
        let parsed: ConfigFile = serde_json::from_value(json!({"mcpServers": {
            "local": {"command": "srv", "args": ["--x"], "env": {"K": "V"}},
            "remote": {"url": "https://example.com/mcp", "headers": {"X-Key": "k"}}
        }}))
        .unwrap();
        assert_eq!(
            parsed.mcp_servers["local"].transport().unwrap(),
            Transport::Stdio {
                command: "srv".into(),
                args: vec!["--x".into()],
                env: BTreeMap::from([("K".into(), "V".into())]),
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
