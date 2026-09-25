//! LLM access through configurable gateways.
//!
//! The conversation is stored in Anthropic Messages shape (raw JSON content
//! blocks); each protocol backend converts it for its API and converts the
//! streamed response back:
//!
//! - [`anthropic`]: the Anthropic Messages API (`POST /v1/messages`).
//! - [`openai`]: the OpenAI Responses API (`POST /v1/responses`).
//!
//! Assistant messages remember which `provider/model` produced them, so
//! model-bound blocks (Anthropic `thinking`, OpenAI reasoning items) are
//! only ever sent back to that same model.

mod anthropic;
pub mod config;
mod http;
mod openai;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

pub use config::{Protocol, Providers};

/// One conversation turn. `content` is a list of raw content blocks.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: &'static str,
    pub content: Vec<Value>,
    /// For assistant turns: the `provider/model` that produced it.
    pub source: Option<String>,
}

impl Message {
    pub fn user(content: Vec<Value>) -> Self {
        Self { role: "user", content, source: None }
    }
}

/// Incremental output surfaced while a response streams in.
pub enum StreamEvent<'a> {
    TextDelta(&'a str),
    TextEnd,
}

/// A tool as offered to the model, independent of protocol.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// Whether the tool validates its own input strictly, so the API may
    /// stream it unbuffered (Anthropic `eager_input_streaming`).
    pub eager_input_streaming: bool,
}

/// A complete response, normalized to Anthropic vocabulary.
#[derive(Debug, Default)]
pub struct Response {
    /// Content blocks to append to the history as the assistant turn.
    pub content: Vec<Value>,
    /// `end_turn`, `tool_use`, `max_tokens`, `refusal`, ...
    pub stop_reason: Option<String>,
    pub stop_details: Option<Value>,
    /// `tool_use` id -> raw input text, for tool calls whose streamed input
    /// was not a valid JSON object. Their `input` in `content` is `{}`.
    pub invalid_tool_inputs: HashMap<String, String>,
    /// Token counts, when the gateway reports them.
    pub usage: Option<Usage>,
}

/// Token counts of one request.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Usage {
    /// Every input token, cached or not: the size of the context sent.
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// A handle on one model at one provider. Cheap to clone.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    providers: Arc<Providers>,
    spec: String,
}

impl Client {
    /// A client for the providers' default model.
    pub fn new(providers: Providers) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(300))
            .build()
            .context("building HTTP client")?;
        let spec = providers.default_model.clone();
        Ok(Self { http, providers: Arc::new(providers), spec })
    }

    /// The `provider/model` this client talks to.
    pub fn spec(&self) -> &str {
        &self.spec
    }

    pub fn providers(&self) -> &Providers {
        &self.providers
    }

    /// A client for another model. A spec without a known provider prefix
    /// is a model on this client's provider.
    pub fn switch(&self, spec: &str) -> Result<Self> {
        let provider = self.spec.split_once('/').map(|(p, _)| p);
        let spec = self.providers.resolve_spec(spec, provider)?;
        self.providers.target(&spec)?;
        Ok(Self { spec, ..self.clone() })
    }

    /// Sends a streaming request and assembles the final response, calling
    /// `on_event` as text arrives.
    pub async fn create(
        &self,
        system: &str,
        tools: &[ToolDef],
        messages: &[Message],
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<Response> {
        let target = self.providers.target(&self.spec)?;
        match target.config.protocol {
            Protocol::Anthropic => anthropic::create(&self.http, &target, system, tools, messages, on_event).await,
            Protocol::OpenAiResponses => openai::create(&self.http, &target, system, tools, messages, on_event).await,
        }
    }
}

/// Whether `message` was produced by the model `spec`: model-bound blocks
/// in it may be sent back.
fn same_source(message: &Message, spec: &str) -> bool {
    message.source.as_deref() == Some(spec)
}

/// Strictly parses streamed tool input. An empty input means no arguments.
fn parse_tool_input(raw: &str) -> Option<Value> {
    if raw.trim().is_empty() {
        return Some(serde_json::json!({}));
    }
    serde_json::from_str::<Value>(raw).ok().filter(Value::is_object)
}
