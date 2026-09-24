//! Minimal client for the Anthropic Messages API (`POST /v1/messages`).
//!
//! There is no official Rust SDK, so this speaks raw HTTP and parses the SSE
//! stream itself. Content blocks are kept as raw JSON so that assistant turns
//! (including `thinking` blocks) can be echoed back to the API unchanged.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use serde_json::{Value, json};

const API_VERSION: &str = "2023-06-01";
/// Server-side refusal fallback, `fallbacks: "default"` form.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MAX_RETRIES: u32 = 3;

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
    pub auth: Auth,
}

#[derive(Debug, Clone)]
pub enum Auth {
    ApiKey(String),
    Bearer(String),
}

impl Config {
    /// Reads `ANTHROPIC_API_KEY` (or `ANTHROPIC_AUTH_TOKEN`),
    /// `ANTHROPIC_BASE_URL`, `HARNESS_MODEL` and `HARNESS_MAX_TOKENS`.
    pub fn from_env() -> Result<Self> {
        let auth = match (std::env::var("ANTHROPIC_API_KEY"), std::env::var("ANTHROPIC_AUTH_TOKEN")) {
            (Ok(k), _) if !k.is_empty() => Auth::ApiKey(k),
            (_, Ok(t)) if !t.is_empty() => Auth::Bearer(t),
            _ => bail!("set ANTHROPIC_API_KEY (or ANTHROPIC_AUTH_TOKEN) to call the API"),
        };
        let max_tokens = match std::env::var("HARNESS_MAX_TOKENS") {
            Ok(v) => v.parse().context("HARNESS_MAX_TOKENS must be an integer")?,
            // Streaming avoids HTTP timeouts, so give the model room.
            Err(_) => 64_000,
        };
        Ok(Self {
            base_url: std::env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com".into())
                .trim_end_matches('/')
                .to_string(),
            model: std::env::var("HARNESS_MODEL").unwrap_or_else(|_| "claude-opus-5".into()),
            max_tokens,
            auth,
        })
    }
}

/// One conversation turn. `content` is a list of raw content blocks.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: &'static str,
    pub content: Vec<Value>,
}

impl Message {
    fn to_json(&self) -> Value {
        json!({"role": self.role, "content": self.content})
    }
}

/// Incremental output surfaced while a response streams in.
pub enum StreamEvent<'a> {
    TextDelta(&'a str),
    TextEnd,
}

#[derive(Debug, Default)]
pub struct Response {
    pub content: Vec<Value>,
    pub stop_reason: Option<String>,
    pub stop_details: Option<Value>,
    /// `tool_use` id -> raw input text, for tool calls whose streamed input
    /// was not a valid JSON object. Their `input` in `content` is `{}`.
    pub invalid_tool_inputs: HashMap<String, String>,
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    config: Config,
}

impl Client {
    pub fn new(config: Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(300))
            .build()
            .context("building HTTP client")?;
        Ok(Self { http, config })
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    /// A client for the same API that uses a different model.
    pub fn with_model(&self, model: &str) -> Self {
        let mut client = self.clone();
        client.config.model = model.to_string();
        client
    }

    /// Sends a streaming request and assembles the final response, calling
    /// `on_event` as text arrives.
    pub async fn create(
        &self,
        system: &str,
        tools: &[Value],
        messages: &[Message],
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<Response> {
        let body = json!({
            "model": self.config.model,
            "max_tokens": self.config.max_tokens,
            "system": system,
            "tools": tools,
            "messages": messages.iter().map(Message::to_json).collect::<Vec<_>>(),
            "thinking": {"type": "adaptive"},
            // Automatic prompt caching: the growing conversation prefix is
            // re-read from cache on every loop iteration.
            "cache_control": {"type": "ephemeral"},
            // On a safety decline, the API re-runs the request on a fallback
            // model chosen by refusal category.
            "fallbacks": "default",
            "stream": true,
        });

        let resp = self.send_with_retries(&body).await?;
        let mut acc = Accumulator::default();
        // Buffer raw bytes: a chunk boundary can split a multi-byte UTF-8
        // character, so only decode once a whole event has arrived.
        let mut buf: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading response stream")?;
            buf.extend(chunk.iter().filter(|&&b| b != b'\r'));
            // An SSE event ends with a blank line.
            while let Some(end) = buf.windows(2).position(|w| w == b"\n\n") {
                let raw: Vec<u8> = buf.drain(..end + 2).collect();
                let event = String::from_utf8_lossy(&raw);
                for data in event.lines().filter_map(|l| l.strip_prefix("data:")) {
                    let data: Value = serde_json::from_str(data.trim())
                        .with_context(|| format!("parsing stream event: {data}"))?;
                    if acc.apply(&data, on_event)? {
                        return Ok(acc.finish());
                    }
                }
            }
        }
        bail!("stream ended before message_stop")
    }

    /// Retries connection failures and retryable statuses; only a successful
    /// response starts streaming.
    async fn send_with_retries(&self, body: &Value) -> Result<reqwest::Response> {
        let url = format!("{}/v1/messages", self.config.base_url);
        let mut attempt = 0;
        loop {
            let mut req = self
                .http
                .post(&url)
                .header("anthropic-version", API_VERSION)
                .header("anthropic-beta", FALLBACK_BETA)
                .json(body);
            req = match &self.config.auth {
                Auth::ApiKey(k) => req.header("x-api-key", k),
                Auth::Bearer(t) => req.bearer_auth(t),
            };

            let result = req.send().await;
            let retryable = match &result {
                Ok(resp) => {
                    let s = resp.status().as_u16();
                    s == 408 || s == 409 || s == 429 || s >= 500
                }
                Err(e) => e.is_connect() || e.is_timeout(),
            };
            if retryable && attempt < MAX_RETRIES {
                attempt += 1;
                let wait = retry_after(&result).unwrap_or(Duration::from_secs(1 << attempt));
                tokio::time::sleep(wait).await;
                continue;
            }

            let resp = result.context("sending request")?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                bail!("API error {status}: {text}");
            }
            return Ok(resp);
        }
    }
}

fn retry_after(result: &reqwest::Result<reqwest::Response>) -> Option<Duration> {
    let secs = result.as_ref().ok()?.headers().get("retry-after")?.to_str().ok()?.parse().ok()?;
    Some(Duration::from_secs(secs))
}

/// Folds stream events into complete content blocks.
#[derive(Default)]
struct Accumulator {
    response: Response,
    /// Block index -> accumulated `input_json_delta` fragments.
    partial_json: HashMap<usize, String>,
}

impl Accumulator {
    /// Applies one event; returns true on `message_stop`.
    fn apply(&mut self, event: &Value, on_event: &mut dyn FnMut(StreamEvent)) -> Result<bool> {
        let content = &mut self.response.content;
        match event["type"].as_str().unwrap_or("") {
            "content_block_start" => {
                let index = event["index"].as_u64().unwrap_or(content.len() as u64) as usize;
                if content.len() <= index {
                    content.resize(index + 1, Value::Null);
                }
                content[index] = event["content_block"].clone();
            }
            "content_block_delta" => {
                let index = event["index"].as_u64().unwrap_or(0) as usize;
                let delta = &event["delta"];
                let Some(block) = content.get_mut(index) else {
                    bail!("delta for unknown content block {index}");
                };
                match delta["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let text = delta["text"].as_str().unwrap_or("");
                        append_str(block, "text", text);
                        on_event(StreamEvent::TextDelta(text));
                    }
                    "thinking_delta" => append_str(block, "thinking", delta["thinking"].as_str().unwrap_or("")),
                    "signature_delta" => block["signature"] = delta["signature"].clone(),
                    "input_json_delta" => self
                        .partial_json
                        .entry(index)
                        .or_default()
                        .push_str(delta["partial_json"].as_str().unwrap_or("")),
                    "citations_delta" => match block["citations"].as_array_mut() {
                        Some(list) => list.push(delta["citation"].clone()),
                        None => block["citations"] = json!([delta["citation"]]),
                    },
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = event["index"].as_u64().unwrap_or(0) as usize;
                let Some(block) = content.get_mut(index) else {
                    bail!("stop for unknown content block {index}");
                };
                match block["type"].as_str().unwrap_or("") {
                    "text" => on_event(StreamEvent::TextEnd),
                    "tool_use" => {
                        let raw = self.partial_json.remove(&index).unwrap_or_default();
                        match parse_tool_input(&raw) {
                            Some(input) => block["input"] = input,
                            None => {
                                block["input"] = json!({});
                                let id = block["id"].as_str().unwrap_or("").to_string();
                                self.response.invalid_tool_inputs.insert(id, raw);
                            }
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                let delta = &event["delta"];
                if let Some(reason) = delta["stop_reason"].as_str() {
                    self.response.stop_reason = Some(reason.to_string());
                }
                if !delta["stop_details"].is_null() {
                    self.response.stop_details = Some(delta["stop_details"].clone());
                }
            }
            "message_stop" => return Ok(true),
            "error" => bail!("API stream error: {}", event["error"]),
            _ => {} // message_start, ping, and future event types
        }
        Ok(false)
    }

    fn finish(mut self) -> Response {
        self.response.content.retain(|b| !b.is_null());
        self.response
    }
}

fn append_str(block: &mut Value, field: &str, text: &str) {
    let current = block[field].as_str().unwrap_or("");
    block[field] = Value::String(format!("{current}{text}"));
}

/// Strictly parses streamed tool input. An empty input means no arguments.
fn parse_tool_input(raw: &str) -> Option<Value> {
    if raw.trim().is_empty() {
        return Some(json!({}));
    }
    serde_json::from_str::<Value>(raw).ok().filter(Value::is_object)
}

/// Prepares response content to be echoed back as the assistant turn.
///
/// After a mid-output refusal fallback, blocks before the last `fallback`
/// marker belong to the declined attempt: only its `text` blocks may be sent
/// back. Everything after the marker (and every block when there is no
/// marker) is kept unchanged.
pub fn content_for_history(content: &[Value]) -> Vec<Value> {
    let boundary = content.iter().rposition(|b| b["type"] == "fallback");
    match boundary {
        None => content.to_vec(),
        Some(i) => content[..i]
            .iter()
            .filter(|b| b["type"] == "text")
            .chain(&content[i + 1..])
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_events(events: &[Value]) -> (Response, String) {
        let mut acc = Accumulator::default();
        let mut streamed = String::new();
        let mut on_event = |e: StreamEvent| match e {
            StreamEvent::TextDelta(t) => streamed.push_str(t),
            StreamEvent::TextEnd => streamed.push('|'),
        };
        for e in events {
            if acc.apply(e, &mut on_event).unwrap() {
                break;
            }
        }
        (acc.finish(), streamed)
    }

    #[test]
    fn assembles_streamed_blocks() {
        let (resp, streamed) = run_events(&[
            json!({"type": "message_start", "message": {"content": []}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Hel"}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "lo"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "t1", "name": "bash", "input": {}}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"command\": "}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "\"ls\"}"}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
            json!({"type": "message_stop"}),
        ]);
        assert_eq!(streamed, "Hello|");
        assert_eq!(resp.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(
            resp.content,
            vec![
                json!({"type": "thinking", "thinking": "", "signature": "sig"}),
                json!({"type": "text", "text": "Hello"}),
                json!({"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}}),
            ]
        );
        assert!(resp.invalid_tool_inputs.is_empty());
    }

    #[test]
    fn records_invalid_tool_input() {
        let (resp, _) = run_events(&[
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t1", "name": "write", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"path\": \"a\", \"content\": \"unterminated"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_stop"}),
        ]);
        assert_eq!(resp.content[0]["input"], json!({}));
        assert_eq!(
            resp.invalid_tool_inputs.get("t1").map(String::as_str),
            Some("{\"path\": \"a\", \"content\": \"unterminated")
        );
    }

    #[test]
    fn empty_tool_input_is_empty_object() {
        assert_eq!(parse_tool_input(""), Some(json!({})));
        assert_eq!(parse_tool_input("[1]"), None);
    }

    #[test]
    fn history_unchanged_without_fallback() {
        let content = vec![
            json!({"type": "thinking", "thinking": "", "signature": "s"}),
            json!({"type": "text", "text": "hi"}),
        ];
        assert_eq!(content_for_history(&content), content);
    }

    #[test]
    fn history_drops_declined_non_text_blocks() {
        let content = vec![
            json!({"type": "thinking", "thinking": "", "signature": "s"}),
            json!({"type": "text", "text": "partial"}),
            json!({"type": "tool_use", "id": "t0", "name": "bash", "input": {}}),
            json!({"type": "fallback", "from": {"model": "a"}, "to": {"model": "b"}}),
            json!({"type": "thinking", "thinking": "", "signature": "s2"}),
            json!({"type": "tool_use", "id": "t1", "name": "bash", "input": {}}),
        ];
        let kept = content_for_history(&content);
        assert_eq!(
            kept,
            vec![
                json!({"type": "text", "text": "partial"}),
                json!({"type": "thinking", "thinking": "", "signature": "s2"}),
                json!({"type": "tool_use", "id": "t1", "name": "bash", "input": {}}),
            ]
        );
    }
}
