//! LSP (Language Server Protocol) client, for the language servers plugins
//! bring in Claude Code's format (`.lsp.json`, or `lspServers` in the
//! manifest):
//!
//! ```json
//! { "go": { "command": "gopls", "args": ["serve"], "extensionToLanguage": { ".go": "go" } } }
//! ```
//!
//! A server starts the first time the `lsp` tool asks about a file with one
//! of its extensions, and stays up for the session. The tool asks it for
//! definitions, references, hover text, symbols and diagnostics. Like MCP
//! servers, language servers run outside the sandbox; the files the tool
//! names are checked against its read rules.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Notify, oneshot};

use crate::tools::{Tool, ToolOutput, parse_input, spill};
use crate::util::expand_env;

/// How long a server may take to start, and to answer a request.
const START_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a server that failed to start is not tried again.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// How long `diagnostics` waits for a server to report on a file it just
/// got.
const DIAGNOSTICS_WAIT: Duration = Duration::from_secs(10);
const MAX_OUTPUT_BYTES: usize = 30_000;
/// Locations listed for `references` and the like.
const MAX_LOCATIONS: usize = 100;

/// A language server, as a plugin configures it.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// File extensions (`.go`) and the language ids the server knows them by.
    pub extensions: BTreeMap<String, String>,
    pub initialization_options: Value,
    /// What it gets for `workspace/configuration`.
    pub settings: Value,
}

impl ServerConfig {
    /// Reads Claude Code's shape; the fields the harness does not use
    /// (`transport`, timeouts, restarts) are ignored.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            command: String,
            #[serde(default)]
            args: Vec<String>,
            #[serde(default)]
            env: BTreeMap<String, String>,
            #[serde(default)]
            extension_to_language: BTreeMap<String, String>,
            #[serde(default)]
            initialization_options: Value,
            #[serde(default)]
            settings: Value,
            transport: Option<String>,
        }
        let raw: Raw = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        if raw.transport.as_deref().is_some_and(|t| t != "stdio") {
            return Err(format!("transport `{}` is not supported (only `stdio`)", raw.transport.unwrap_or_default()));
        }
        if raw.extension_to_language.is_empty() {
            return Err("it names no file extensions (`extensionToLanguage`)".into());
        }
        let extensions =
            raw.extension_to_language.into_iter().map(|(ext, lang)| (ext.trim_start_matches('.').to_string(), lang)).collect();
        Ok(Self {
            command: raw.command,
            args: raw.args,
            env: raw.env,
            extensions,
            initialization_options: raw.initialization_options,
            settings: raw.settings,
        })
    }
}

/// Whether `command` can be started: a path that exists, or a name on
/// `PATH`.
pub fn command_exists(command: &str) -> bool {
    let command = expand_env(command);
    if command.contains('/') {
        return Path::new(&command).is_file();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|dir| dir.join(&command).is_file())
}

/// A server started, or why and when it could not start.
type Started = Result<Arc<Server>, (String, Instant)>;

/// The configured servers, started as they are needed.
#[derive(Default)]
pub struct Manager {
    configs: BTreeMap<String, ServerConfig>,
    /// Each server started so far.
    started: tokio::sync::Mutex<HashMap<String, Started>>,
}

impl Manager {
    pub fn new(configs: BTreeMap<String, ServerConfig>) -> Self {
        Self { configs, started: tokio::sync::Mutex::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.configs.is_empty()
    }

    /// The server for `path` (by its extension), started if need be, and
    /// the language id of the file.
    async fn server_for(&self, path: &Path) -> Result<(Arc<Server>, String), String> {
        let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
        let Some((name, config, language)) =
            self.configs.iter().find_map(|(name, c)| c.extensions.get(&ext).map(|lang| (name, c, lang.clone())))
        else {
            let known: Vec<String> = self.configs.values().flat_map(|c| c.extensions.keys().map(|e| format!(".{e}"))).collect();
            return Err(format!("no language server handles `.{ext}` files (they handle {})", known.join(", ")));
        };
        let mut started = self.started.lock().await;
        if let Some(Ok(server)) = started.get(name)
            && server.is_running()
        {
            return Ok((server.clone(), language));
        }
        if let Some(Err((e, at))) = started.get(name)
            && at.elapsed() < RETRY_AFTER
        {
            return Err(e.clone());
        }
        let server = Server::start(name, config).await.map(Arc::new);
        started.insert(name.clone(), server.clone().map_err(|e| (e, Instant::now())));
        server.map(|s| (s, language))
    }

    /// Stops the servers that were started.
    pub async fn shutdown(&self) {
        let started = std::mem::take(&mut *self.started.lock().await);
        for server in started.into_values().flatten() {
            server.shutdown().await;
        }
    }
}

type Pending = Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>;

/// A running language server.
struct Server {
    writer: Arc<tokio::sync::Mutex<ChildStdin>>,
    next_id: AtomicI64,
    pending: Arc<Pending>,
    diagnostics: Arc<Diagnostics>,
    /// The files it was given, by URI: their version and text.
    documents: tokio::sync::Mutex<HashMap<String, (i64, String)>>,
    child: Mutex<Option<Child>>,
    /// Why it stopped, once it has.
    exited: Arc<Mutex<Option<String>>>,
}

/// What the server reported about each file, by URI, with a count that goes
/// up with each report.
#[derive(Default)]
struct Diagnostics {
    by_uri: Mutex<HashMap<String, (u64, Vec<Value>)>>,
    reports: AtomicU64,
    changed: Notify,
}

impl Server {
    async fn start(name: &str, config: &ServerConfig) -> Result<Self, String> {
        let log = std::fs::File::create(log_path(name)).map_err(|e| format!("creating its log: {e}"))?;
        let command = expand_env(&config.command);
        let mut cmd = Command::new(&command);
        cmd.args(config.args.iter().map(|a| expand_env(a)))
            .envs(config.env.iter().map(|(k, v)| (k, expand_env(v))))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            // Ctrl-C in the terminal interrupts a turn, not the server.
            .process_group(0);
        let mut child = cmd.spawn().map_err(|e| format!("starting `{command}`: {e}"))?;
        let (stdin, stdout) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
        let server = Self {
            writer: Arc::new(tokio::sync::Mutex::new(stdin)),
            next_id: AtomicI64::new(1),
            pending: Arc::default(),
            diagnostics: Arc::default(),
            documents: tokio::sync::Mutex::default(),
            child: Mutex::new(Some(child)),
            exited: Arc::default(),
        };
        server.read_messages(stdout, config.settings.clone());
        let root = std::env::current_dir().unwrap_or_default();
        let root_uri = path_uri(&root);
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "harness", "version": env!("CARGO_PKG_VERSION")},
            "rootUri": root_uri,
            "rootPath": root.display().to_string(),
            "workspaceFolders": [{"uri": root_uri, "name": root.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()}],
            "initializationOptions": config.initialization_options,
            "capabilities": {
                "textDocument": {
                    "synchronization": {"didSave": false, "dynamicRegistration": false},
                    "definition": {"linkSupport": true},
                    "implementation": {"linkSupport": true},
                    "references": {},
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                    "publishDiagnostics": {"relatedInformation": false},
                },
                "workspace": {"configuration": true, "workspaceFolders": true, "symbol": {}},
                "general": {"positionEncodings": ["utf-16"]},
            },
        });
        match tokio::time::timeout(START_TIMEOUT, server.request("initialize", params)).await {
            Err(_) => return Err(format!("it did not initialize within {}s", START_TIMEOUT.as_secs())),
            Ok(Err(e)) => return Err(format!("initialize failed: {e}")),
            Ok(Ok(_)) => {}
        }
        server.notify("initialized", json!({})).await?;
        if !config.settings.is_null() {
            server.notify("workspace/didChangeConfiguration", json!({"settings": config.settings})).await?;
        }
        Ok(server)
    }

    fn is_running(&self) -> bool {
        self.exited.lock().unwrap().is_none()
    }

    /// Reads what the server sends, on a task of its own: answers to
    /// requests, diagnostics, and its own requests, which get `settings`
    /// for `workspace/configuration` and an empty answer otherwise.
    fn read_messages(&self, stdout: ChildStdout, settings: Value) {
        let (pending, diagnostics, writer, exited) =
            (self.pending.clone(), self.diagnostics.clone(), self.writer.clone(), self.exited.clone());
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let why = loop {
                let message = match read_message(&mut reader).await {
                    Ok(Some(message)) => message,
                    Ok(None) => break "the language server exited".to_string(),
                    Err(e) => break format!("reading from the language server: {e}"),
                };
                let method = message["method"].as_str();
                match (method, message.get("id")) {
                    (None, Some(id)) => {
                        let sender = id.as_i64().and_then(|id| pending.lock().unwrap().remove(&id));
                        if let Some(sender) = sender {
                            let result = match message.get("error") {
                                Some(error) => Err(error["message"].as_str().unwrap_or("error").to_string()),
                                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                            };
                            let _ = sender.send(result);
                        }
                    }
                    (Some(method), Some(id)) => {
                        let result = match method {
                            // Each item asks for a section of the settings.
                            "workspace/configuration" => {
                                let section = |item: &Value| {
                                    let keys = item["section"].as_str().unwrap_or_default();
                                    keys.split('.').filter(|k| !k.is_empty()).fold(&settings, |value, key| &value[key]).clone()
                                };
                                let items = message["params"]["items"].as_array().cloned().unwrap_or_default();
                                Value::Array(items.iter().map(section).collect())
                            }
                            _ => Value::Null,
                        };
                        let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
                        let _ = write_message(&mut *writer.lock().await, &reply).await;
                    }
                    (Some("textDocument/publishDiagnostics"), None) => {
                        let uri = message["params"]["uri"].as_str().unwrap_or_default().to_string();
                        let list = message["params"]["diagnostics"].as_array().cloned().unwrap_or_default();
                        let n = diagnostics.reports.fetch_add(1, Ordering::SeqCst) + 1;
                        diagnostics.by_uri.lock().unwrap().insert(uri, (n, list));
                        diagnostics.changed.notify_waiters();
                    }
                    _ => {}
                }
            };
            *exited.lock().unwrap() = Some(why.clone());
            for (_, sender) in pending.lock().unwrap().drain() {
                let _ = sender.send(Err(why.clone()));
            }
            diagnostics.changed.notify_waiters();
        });
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        if let Some(why) = self.exited.lock().unwrap().clone() {
            return Err(why);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = write_message(&mut *self.writer.lock().await, &message).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(format!("writing to the language server: {e}"));
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("the language server went away".into()),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("{method} got no answer within {}s", REQUEST_TIMEOUT.as_secs()))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        write_message(&mut *self.writer.lock().await, &message).await.map_err(|e| format!("writing to the language server: {e}"))
    }

    /// Gives the server `path` as it is on disk (opening it, or sending its
    /// new text); returns its URI and text, and whether it was new to the
    /// server.
    async fn sync(&self, path: &Path, language: &str) -> Result<(String, String, bool), String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        let uri = path_uri(path);
        let mut documents = self.documents.lock().await;
        let changed = match documents.get_mut(&uri) {
            Some((_, old)) if *old == text => false,
            Some((version, old)) => {
                *version += 1;
                *old = text.clone();
                let params = json!({"textDocument": {"uri": uri, "version": *version}, "contentChanges": [{"text": text}]});
                self.notify("textDocument/didChange", params).await?;
                true
            }
            None => {
                let params = json!({"textDocument": {"uri": uri, "languageId": language, "version": 1, "text": text}});
                self.notify("textDocument/didOpen", params).await?;
                documents.insert(uri.clone(), (1, text.clone()));
                true
            }
        };
        Ok((uri, text, changed))
    }

    /// The diagnostics for `uri`: once the server reports on it after
    /// report number `after`, if `fresh`, else what it said last.
    async fn diagnostics(&self, uri: &str, after: u64, fresh: bool) -> Option<Vec<Value>> {
        let deadline = tokio::time::Instant::now() + DIAGNOSTICS_WAIT;
        loop {
            let changed = self.diagnostics.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some((n, list)) = self.diagnostics.by_uri.lock().unwrap().get(uri)
                && (!fresh || *n > after)
            {
                return Some(list.clone());
            }
            if !self.is_running() || tokio::time::timeout_at(deadline, changed).await.is_err() {
                return self.diagnostics.by_uri.lock().unwrap().get(uri).map(|(_, list)| list.clone());
            }
        }
    }

    async fn shutdown(&self) {
        if self.is_running() {
            let _ = tokio::time::timeout(Duration::from_secs(2), self.request("shutdown", Value::Null)).await;
            let _ = self.notify("exit", Value::Null).await;
        }
        let child = self.child.lock().unwrap().take();
        if let Some(mut child) = child
            && tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_err()
        {
            let _ = child.kill().await;
        }
    }
}

pub fn log_path(server: &str) -> PathBuf {
    let name: String = server.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    std::env::temp_dir().join(format!("harness-lsp-{name}.log"))
}

/// Reads one message: `Content-Length` headers, a blank line, the JSON.
/// `None` at the end of the stream.
async fn read_message<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<Option<Value>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            if length.is_some() {
                break;
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; length.unwrap_or(0)];
    reader.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

async fn write_message<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W, message: &Value) -> std::io::Result<()> {
    let body = message.to_string();
    writer.write_all(format!("Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await?;
    writer.flush().await
}

/// `file://` and the absolute path, with what a URI cannot hold escaped.
fn path_uri(path: &Path) -> String {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => uri.push(byte as char),
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

/// The path a `file://` URI names.
fn uri_path(uri: &str) -> PathBuf {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    let mut bytes = Vec::with_capacity(rest.len());
    let mut iter = rest.bytes();
    while let Some(byte) = iter.next() {
        if byte == b'%' {
            let hex: Vec<u8> = iter.by_ref().take(2).collect();
            match std::str::from_utf8(&hex).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                Some(decoded) => bytes.push(decoded),
                None => bytes.extend(std::iter::once(b'%').chain(hex)),
            }
        } else {
            bytes.push(byte);
        }
    }
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

/// The UTF-16 offset LSP counts for the 1-based `column` (in characters)
/// of `line`.
fn utf16_column(line: &str, column: u32) -> u32 {
    line.chars().take(column.saturating_sub(1) as usize).map(|c| c.len_utf16() as u32).sum()
}

/// The 1-based column in characters of UTF-16 offset `offset` in `line`.
fn char_column(line: &str, offset: u64) -> usize {
    let mut units = 0;
    for (i, c) in line.chars().enumerate() {
        if units >= offset {
            return i + 1;
        }
        units += c.len_utf16() as u64;
    }
    line.chars().count() + 1
}

/// Reads and caches the files locations point into.
#[derive(Default)]
struct Sources(HashMap<PathBuf, Option<Vec<String>>>);

impl Sources {
    fn line(&mut self, path: &Path, line: usize) -> Option<&str> {
        let lines = self.0.entry(path.to_path_buf()).or_insert_with(|| {
            std::fs::read_to_string(path).ok().map(|t| t.lines().map(String::from).collect())
        });
        lines.as_ref()?.get(line).map(String::as_str)
    }

    /// `path:line:column` (1-based, relative to the working directory) of
    /// LSP position `start` in `uri`, and the line's text.
    fn place(&mut self, uri: &str, start: &Value) -> String {
        let path = uri_path(uri);
        let line = start["line"].as_u64().unwrap_or(0) as usize;
        let offset = start["character"].as_u64().unwrap_or(0);
        let shown = display_path(&path);
        match self.line(&path, line).map(String::from) {
            Some(text) => format!("{shown}:{}:{}: {}", line + 1, char_column(&text, offset), text.trim()),
            None => format!("{shown}:{}:{}", line + 1, offset + 1),
        }
    }
}

/// `path` relative to the working directory when it is inside it.
fn display_path(path: &Path) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    path.strip_prefix(&cwd).unwrap_or(path).display().to_string()
}

/// The locations in a definition or references answer: a `Location`, a
/// list of them, or `LocationLink`s.
fn locations(result: &Value) -> Vec<(String, Value)> {
    let items = match result {
        Value::Array(items) => items.clone(),
        Value::Null => Vec::new(),
        item => vec![item.clone()],
    };
    items
        .iter()
        .filter_map(|item| {
            let uri = item["uri"].as_str().or(item["targetUri"].as_str())?;
            let range = if item["range"].is_object() { &item["range"] } else { &item["targetSelectionRange"] };
            Some((uri.to_string(), range["start"].clone()))
        })
        .collect()
}

fn format_locations(result: &Value, what: &str) -> String {
    let mut found = locations(result);
    // A server may name one file by two paths (through a symlink).
    let mut seen = Vec::new();
    found.retain(|(uri, start)| {
        let path = uri_path(uri);
        let key = (std::fs::canonicalize(&path).unwrap_or(path), start["line"].clone(), start["character"].clone());
        let new = !seen.contains(&key);
        seen.push(key);
        new
    });
    if found.is_empty() {
        return format!("No {what} found");
    }
    let mut sources = Sources::default();
    let mut lines: Vec<String> = found.iter().take(MAX_LOCATIONS).map(|(uri, start)| sources.place(uri, start)).collect();
    if found.len() > MAX_LOCATIONS {
        lines.push(format!("... [{} more]", found.len() - MAX_LOCATIONS));
    }
    lines.join("\n")
}

/// Hover contents as text: `MarkupContent`, a `MarkedString` or a list.
fn hover_text(contents: &Value) -> String {
    match contents {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(hover_text).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n\n"),
        Value::Object(map) => match (map.get("language").and_then(Value::as_str), map.get("value").and_then(Value::as_str)) {
            (Some(language), Some(value)) => format!("```{language}\n{value}\n```"),
            (None, Some(value)) => value.to_string(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

const SYMBOL_KINDS: [&str; 26] = [
    "file", "module", "namespace", "package", "class", "method", "property", "field", "constructor", "enum",
    "interface", "function", "variable", "constant", "string", "number", "boolean", "array", "object", "key",
    "null", "enum member", "struct", "event", "operator", "type parameter",
];

fn kind_name(kind: &Value) -> &'static str {
    kind.as_u64().and_then(|k| SYMBOL_KINDS.get(k.saturating_sub(1) as usize)).copied().unwrap_or("symbol")
}

/// A document's symbols, nested ones indented: `DocumentSymbol`s or
/// `SymbolInformation`s.
fn format_symbols(result: &Value, out: &mut Vec<String>, depth: usize) {
    for symbol in result.as_array().into_iter().flatten() {
        let range = if symbol["selectionRange"].is_object() { &symbol["selectionRange"] } else { &symbol["location"]["range"] };
        let line = range["start"]["line"].as_u64().unwrap_or(0) + 1;
        let detail = symbol["detail"].as_str().filter(|d| !d.is_empty()).map(|d| format!(" — {d}")).unwrap_or_default();
        let name = symbol["name"].as_str().unwrap_or("?");
        out.push(format!("{}{} {name}{detail} (line {line})", "  ".repeat(depth), kind_name(&symbol["kind"])));
        format_symbols(&symbol["children"], out, depth + 1);
    }
}

fn format_workspace_symbols(result: &Value) -> String {
    let mut sources = Sources::default();
    let symbols = result.as_array().cloned().unwrap_or_default();
    if symbols.is_empty() {
        return "No symbols found".into();
    }
    let mut lines: Vec<String> = symbols
        .iter()
        .take(MAX_LOCATIONS)
        .map(|s| {
            let location = &s["location"];
            let start = if location["range"].is_object() { &location["range"]["start"] } else { &json!({"line": 0, "character": 0}) };
            let place = sources.place(location["uri"].as_str().unwrap_or_default(), start);
            format!("{} {} — {place}", kind_name(&s["kind"]), s["name"].as_str().unwrap_or("?"))
        })
        .collect();
    if symbols.len() > MAX_LOCATIONS {
        lines.push(format!("... [{} more]", symbols.len() - MAX_LOCATIONS));
    }
    lines.join("\n")
}

fn format_diagnostics(path: &Path, list: &[Value], text: &str) -> String {
    if list.is_empty() {
        return format!("No problems reported in {}", display_path(path));
    }
    let lines: Vec<&str> = text.lines().collect();
    let shown = display_path(path);
    let mut out: Vec<String> = list
        .iter()
        .map(|d| {
            let start = &d["range"]["start"];
            let line = start["line"].as_u64().unwrap_or(0) as usize;
            let column = char_column(lines.get(line).copied().unwrap_or_default(), start["character"].as_u64().unwrap_or(0));
            let severity = match d["severity"].as_u64() {
                Some(1) => "error",
                Some(2) => "warning",
                Some(3) => "info",
                Some(4) => "hint",
                _ => "problem",
            };
            let code = match &d["code"] {
                Value::Null => String::new(),
                Value::String(code) => format!(" [{code}]"),
                code => format!(" [{code}]"),
            };
            let source = d["source"].as_str().map(|s| format!(" ({s})")).unwrap_or_default();
            let message = d["message"].as_str().unwrap_or_default().replace('\n', " ");
            format!("{shown}:{}:{column}: {severity}: {message}{code}{source}", line + 1)
        })
        .collect();
    out.sort_by_key(|l| !l.contains(": error: "));
    out.join("\n")
}

/// The `lsp` tool.
pub struct LspTool {
    manager: Arc<Manager>,
    description: String,
}

impl LspTool {
    pub fn new(manager: Arc<Manager>) -> Self {
        let mut languages: Vec<String> = manager.configs.values().flat_map(|c| c.extensions.keys().map(|e| format!(".{e}"))).collect();
        languages.sort();
        languages.dedup();
        let description = format!(
            "Ask a language server about code, for files ending in {}. Operations: `definition` and \
             `implementation` (where the symbol at `line`/`character` is defined or implemented), \
             `references` (where it is used), `hover` (its type and documentation), `symbols` (the file's \
             outline), `workspace_symbols` (symbols matching `query` anywhere in the project), and \
             `diagnostics` (the errors and warnings in the file as it is on disk; check them after \
             changing a file). Lines and characters count from 1, as `read` shows lines. More precise \
             than `grep` for following a symbol; the first call may take a while as the server starts.",
            languages.join(", ")
        );
        Self { manager, description }
    }
}

#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Definition,
    Implementation,
    References,
    Hover,
    Symbols,
    WorkspaceSymbols,
    Diagnostics,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    operation: Operation,
    path: String,
    line: Option<u32>,
    character: Option<u32>,
    query: Option<String>,
}

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["definition", "implementation", "references", "hover", "symbols", "workspace_symbols", "diagnostics"]
                },
                "path": {"type": "string", "description": "The file; for workspace_symbols, any file in the language."},
                "line": {"type": "integer", "description": "1-based line of the symbol (definition, implementation, references, hover)."},
                "character": {"type": "integer", "description": "1-based column of the symbol on that line."},
                "query": {"type": "string", "description": "What to look for (workspace_symbols)."}
            },
            "required": ["operation", "path"],
            "additionalProperties": false
        })
    }

    /// It reads files and asks the server about them.
    fn read_only(&self) -> bool {
        true
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let path = PathBuf::from(&input.path);
        if let Some(sandbox) = crate::security::sandbox::active()
            && let Err(e) = sandbox.check_read(&path)
        {
            return ToolOutput::error(e);
        }
        match self.ask(&input, &path).await {
            Ok(text) => ToolOutput::ok(spill::shorten(&text, MAX_OUTPUT_BYTES, "lsp")),
            Err(e) => ToolOutput::error(e),
        }
    }
}

impl LspTool {
    async fn ask(&self, input: &Input, path: &Path) -> Result<String, String> {
        if !path.is_file() {
            return Err(format!("{} is not a file", path.display()));
        }
        let (server, language) = self.manager.server_for(path).await.map_err(|e| format!("{e} (log: {})", self.log_hint(path)))?;
        let reports = server.diagnostics.reports.load(Ordering::SeqCst);
        let (uri, text, changed) = server.sync(path, &language).await?;
        let document = json!({"uri": uri});
        let position = || {
            let (Some(line), Some(character)) = (input.line, input.character) else {
                return Err("this operation needs `line` and `character`".to_string());
            };
            let line_text = text.lines().nth(line.saturating_sub(1) as usize).ok_or(format!("{} has no line {line}", path.display()))?;
            Ok(json!({"line": line.saturating_sub(1), "character": utf16_column(line_text, character)}))
        };
        let at = |position: Value| json!({"textDocument": document, "position": position});
        Ok(match input.operation {
            Operation::Definition => format_locations(&server.request("textDocument/definition", at(position()?)).await?, "definition"),
            Operation::Implementation => {
                format_locations(&server.request("textDocument/implementation", at(position()?)).await?, "implementation")
            }
            Operation::References => {
                let mut params = at(position()?);
                params["context"] = json!({"includeDeclaration": true});
                format_locations(&server.request("textDocument/references", params).await?, "references")
            }
            Operation::Hover => {
                let result = server.request("textDocument/hover", at(position()?)).await?;
                let text = hover_text(&result["contents"]);
                if text.trim().is_empty() { "No hover information".into() } else { text }
            }
            Operation::Symbols => {
                let result = server.request("textDocument/documentSymbol", json!({"textDocument": document})).await?;
                let mut lines = Vec::new();
                format_symbols(&result, &mut lines, 0);
                if lines.is_empty() { "No symbols found".into() } else { lines.join("\n") }
            }
            Operation::WorkspaceSymbols => {
                let query = input.query.clone().ok_or("workspace_symbols needs `query`")?;
                format_workspace_symbols(&server.request("workspace/symbol", json!({"query": query})).await?)
            }
            Operation::Diagnostics => match server.diagnostics(&uri, reports, changed).await {
                Some(list) => format_diagnostics(path, &list, &text),
                None => format!("The language server reported nothing on {} yet; try again shortly", display_path(path)),
            },
        })
    }

    /// Where the log of the server for `path` is, for errors.
    fn log_hint(&self, path: &Path) -> String {
        let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
        let name = self.manager.configs.iter().find(|(_, c)| c.extensions.contains_key(&ext)).map(|(n, _)| n.as_str());
        name.map_or_else(|| "none".to_string(), |n| log_path(n).display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_configs() {
        let config = ServerConfig::from_json(&json!({
            "command": "gopls", "args": ["serve"], "extensionToLanguage": {".go": "go"},
            "transport": "stdio", "startupTimeout": 1000, "settings": {"gopls": {"staticcheck": true}}
        }))
        .unwrap();
        assert_eq!(config.extensions, BTreeMap::from([("go".to_string(), "go".to_string())]));
        assert_eq!(config.settings["gopls"]["staticcheck"], true);
        assert!(ServerConfig::from_json(&json!({"command": "x", "extensionToLanguage": {}})).is_err());
        assert!(ServerConfig::from_json(&json!({"command": "x", "extensionToLanguage": {".x": "x"}, "transport": "socket"})).is_err());
        assert!(command_exists("sh") && !command_exists("no-such-command-here"));
    }

    #[tokio::test]
    async fn frames_messages() {
        let mut buffer = Vec::new();
        write_message(&mut buffer, &json!({"id": 1})).await.unwrap();
        write_message(&mut buffer, &json!({"id": "ü"})).await.unwrap();
        assert!(buffer.starts_with(b"Content-Length: 8\r\n\r\n{\"id\":1}"));
        let mut reader = BufReader::new(&buffer[..]);
        assert_eq!(read_message(&mut reader).await.unwrap(), Some(json!({"id": 1})));
        assert_eq!(read_message(&mut reader).await.unwrap(), Some(json!({"id": "ü"})));
        assert_eq!(read_message(&mut reader).await.unwrap(), None);
    }

    #[test]
    fn uris_and_columns() {
        let path = Path::new("/tmp/a dir/ü.c");
        assert_eq!(path_uri(path), "file:///tmp/a%20dir/%C3%BC.c");
        assert_eq!(uri_path(&path_uri(path)), path);
        // `😀` is two UTF-16 units.
        assert_eq!(utf16_column("a😀b", 3), 3);
        assert_eq!(char_column("a😀b", 3), 3);
        assert_eq!(char_column("ab", 9), 3);
    }

    #[test]
    fn formats_answers() {
        assert_eq!(hover_text(&json!({"kind": "markdown", "value": "`int x`"})), "`int x`");
        assert_eq!(hover_text(&json!([{"language": "c", "value": "int x"}, "doc"])), "```c\nint x\n```\n\ndoc");
        let mut lines = Vec::new();
        let symbols = json!([{"name": "S", "kind": 23, "selectionRange": {"start": {"line": 0}}, "children": [
            {"name": "f", "kind": 8, "detail": "int", "selectionRange": {"start": {"line": 1}}}
        ]}]);
        format_symbols(&symbols, &mut lines, 0);
        assert_eq!(lines, ["struct S (line 1)", "  field f — int (line 2)"]);
        let link = json!([{"targetUri": "file:///nowhere/x.c", "targetSelectionRange": {"start": {"line": 4, "character": 2}}}]);
        assert_eq!(format_locations(&link, "definition"), "/nowhere/x.c:5:3");
        assert_eq!(format_locations(&Value::Null, "definition"), "No definition found");
        let diagnostics = [
            json!({"range": {"start": {"line": 0, "character": 1}}, "severity": 2, "message": "unused", "source": "clang"}),
            json!({"range": {"start": {"line": 1, "character": 0}}, "severity": 1, "message": "bad", "code": "E1"}),
        ];
        let text = format_diagnostics(Path::new("/nowhere/x.c"), &diagnostics, "ab\ncd\n");
        assert_eq!(text, "/nowhere/x.c:2:1: error: bad [E1]\n/nowhere/x.c:1:2: warning: unused (clang)");
    }

    /// Against clangd, where it is installed.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn asks_a_real_server() {
        if !command_exists("clangd") {
            eprintln!("clangd not found; skipped");
            return;
        }
        let _guard = crate::util::lock_cwd();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.c");
        let source = "int add(int a, int b) { return a + b; }\nint main(void) { return add(1, 2); }\nint f(void) { return missing; }\n";
        std::fs::write(&file, source).unwrap();
        let config = ServerConfig::from_json(&json!({"command": "clangd", "extensionToLanguage": {".c": "c"}})).unwrap();
        let manager = Arc::new(Manager::new(BTreeMap::from([("clangd".to_string(), config)])));
        let tool = LspTool::new(manager.clone());
        let path = file.display().to_string();
        let run = async |input: Value| tool.run(input).await;

        let out = run(json!({"operation": "diagnostics", "path": path})).await;
        assert!(!out.is_error && out.content.contains("main.c:3:22: error:") && out.content.contains("missing"), "{}", out.content);
        let out = run(json!({"operation": "definition", "path": path, "line": 2, "character": 26})).await;
        assert!(out.content.contains("main.c:1:5: int add"), "{}", out.content);
        let out = run(json!({"operation": "references", "path": path, "line": 1, "character": 6})).await;
        assert_eq!(out.content.lines().count(), 2, "{}", out.content);
        let out = run(json!({"operation": "hover", "path": path, "line": 1, "character": 5})).await;
        assert!(out.content.contains("int add(int a, int b)"), "{}", out.content);
        let out = run(json!({"operation": "symbols", "path": path})).await;
        assert!(out.content.contains("function add") && out.content.contains("function main"), "{}", out.content);

        // A change on disk reaches the server.
        std::fs::write(&file, "int main(void) { return 0; }\n").unwrap();
        let out = run(json!({"operation": "diagnostics", "path": path})).await;
        assert!(out.content.starts_with("No problems reported"), "{}", out.content);
        let out = run(json!({"operation": "hover", "path": path})).await;
        assert!(out.is_error && out.content.contains("needs `line`"));
        let out = run(json!({"operation": "symbols", "path": dir.path().join("x.rs")})).await;
        assert!(out.is_error);
        manager.shutdown().await;
    }
}
