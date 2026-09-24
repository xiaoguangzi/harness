//! Minimal client for the Anthropic Messages API (`POST /v1/messages`).
//!
//! There is no official Rust SDK, so this speaks raw HTTP. Content blocks are
//! kept as raw JSON so that assistant turns (including `thinking` blocks) can
//! be echoed back to the API byte-for-byte unchanged.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
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
            Err(_) => 16_000,
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
    pub fn user_text(text: impl Into<String>) -> Self {
        Self { role: "user", content: vec![json!({"type": "text", "text": text.into()})] }
    }

    fn to_json(&self) -> Value {
        json!({"role": self.role, "content": self.content})
    }
}

#[derive(Debug, Deserialize)]
pub struct Response {
    pub content: Vec<Value>,
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_details: Option<Value>,
}

pub struct Client {
    http: reqwest::Client,
    config: Config,
}

impl Client {
    pub fn new(config: Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .context("building HTTP client")?;
        Ok(Self { http, config })
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    pub async fn create(&self, system: &str, tools: &[Value], messages: &[Message]) -> Result<Response> {
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
        });

        let url = format!("{}/v1/messages", self.config.base_url);
        let mut attempt = 0;
        loop {
            let mut req = self
                .http
                .post(&url)
                .header("anthropic-version", API_VERSION)
                .header("anthropic-beta", FALLBACK_BETA)
                .json(&body);
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
            let text = resp.text().await.context("reading response body")?;
            if !status.is_success() {
                bail!("API error {status}: {text}");
            }
            return serde_json::from_str(&text).with_context(|| format!("parsing response: {text}"));
        }
    }
}

fn retry_after(result: &reqwest::Result<reqwest::Response>) -> Option<Duration> {
    let secs = result.as_ref().ok()?.headers().get("retry-after")?.to_str().ok()?.parse().ok()?;
    Some(Duration::from_secs(secs))
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
