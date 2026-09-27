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

pub use config::{Effort, Protocol, Providers};

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
    /// A fragment of the model's reasoning; empty when a reasoning block
    /// starts whose text is not shown.
    ThinkingDelta(&'a str),
    /// The model started writing a call to the named tool.
    ToolInputStart(&'a str),
    /// A fragment of the tool call's JSON input, as it streams in.
    ToolInputDelta(&'a str),
    /// Token counts of a request that failed partway, as far as the stream
    /// got; a finished request carries them in [`Response::usage`] instead.
    PartialUsage(Usage),
    /// The response broke off (`error` says why) and the request is sent
    /// again after `wait`: what streamed so far is void, and the response
    /// starts over. `attempt` counts from 1 to [`MAX_RETRIES`].
    Retry { error: &'a anyhow::Error, attempt: u32, wait: Duration },
}

/// How many times a response that broke off is requested again.
pub const MAX_RETRIES: u32 = http::MAX_RETRIES;

/// Why a response broke off, in one line: how far its stream got and the
/// underlying cause, without the layers between.
pub fn retry_reason(error: &anyhow::Error) -> String {
    match error.chain().count() {
        1 => error.to_string(),
        _ => format!("{error}: {}", error.root_cause()),
    }
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
    /// Set by `/effort`; replaces the configured effort for every model.
    effort: Option<Effort>,
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
        Ok(Self { http, providers: Arc::new(providers), spec, effort: None })
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

    /// The effort requests are sent with.
    pub fn effort(&self) -> Effort {
        self.effort
            .or_else(|| self.providers.target(&self.spec).ok().map(|t| t.effort))
            .unwrap_or(self.providers.effort)
    }

    /// The output cap requests are sent with.
    pub fn max_tokens(&self) -> u64 {
        self.providers.target(&self.spec).map_or(0, |t| t.max_tokens()).into()
    }

    /// The model's context window in tokens, as configured.
    pub fn context_window(&self) -> u64 {
        self.providers.target(&self.spec).map_or(u32::MAX, |t| t.context_window()).into()
    }

    /// Overrides the configured effort, or restores it with `None`. Kept
    /// across [`Client::switch`].
    pub fn set_effort(&mut self, effort: Option<Effort>) {
        self.effort = effort;
    }

    /// Sends a streaming request and assembles the final response, calling
    /// `on_event` as text arrives. A response that breaks off midway (the
    /// connection drops, the API reports an overload) is requested again,
    /// after a [`StreamEvent::Retry`]; nothing has acted on it yet.
    pub async fn create(
        &self,
        system: &str,
        tools: &[ToolDef],
        messages: &[Message],
        on_event: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<Response> {
        let mut target = self.providers.target(&self.spec)?;
        target.effort = self.effort.unwrap_or(target.effort);
        let mut attempt = 0;
        loop {
            let result = match target.config.protocol {
                Protocol::Anthropic => anthropic::create(&self.http, &target, system, tools, messages, on_event).await,
                Protocol::OpenAiResponses => {
                    openai::create(&self.http, &target, system, tools, messages, on_event).await
                }
            };
            match result {
                Err(error) if attempt < MAX_RETRIES && http::is_transient(&error) => {
                    attempt += 1;
                    let wait = Duration::from_secs(1 << (attempt - 1));
                    on_event(StreamEvent::Retry { error: &error, attempt, wait });
                    tokio::time::sleep(wait).await;
                }
                result => return result,
            }
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::StreamExt;
    use serde_json::json;

    use super::*;
    use crate::llm::config::tests::{file, no_env};

    /// How the mock server answers one request.
    enum Answer {
        /// Some events, then the connection breaks.
        Cut(Vec<Value>),
        /// These events, then a clean end.
        Events(Vec<Value>),
    }

    fn text_events(text: &str) -> Vec<Value> {
        vec![
            json!({"type": "message_start", "message": {"content": [], "usage": {"input_tokens": 5, "output_tokens": 0}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        ]
    }

    fn finished(text: &str) -> Vec<Value> {
        let mut events = text_events(text);
        events.push(json!({"type": "content_block_stop", "index": 0}));
        events.push(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}));
        events.push(json!({"type": "message_stop"}));
        events
    }

    /// Serves `answers` in turn over the Anthropic protocol; returns a
    /// client for it and the number of requests made.
    async fn serve(answers: Vec<Answer>) -> (Client, Arc<AtomicUsize>) {
        let answers = Arc::new(std::sync::Mutex::new(answers.into_iter()));
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move || async move {
                counter.fetch_add(1, Ordering::SeqCst);
                let (events, cut) = match answers.lock().unwrap().next().expect("an answer for each request") {
                    Answer::Cut(events) => (events, true),
                    Answer::Events(events) => (events, false),
                };
                let chunks = events.iter().map(|e| Ok(format!("data: {e}\n\n"))).collect::<Vec<_>>();
                // The break comes after the events went out.
                let cut = futures::stream::iter(cut.then_some(())).then(|()| async {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    Err(std::io::Error::other("connection lost"))
                });
                let body = axum::body::Body::from_stream(futures::stream::iter(chunks).chain(cut));
                ([("content-type", "text/event-stream")], body)
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = file(json!({"model": "mock/m", "providers": {
            "mock": {"protocol": "anthropic", "base_url": url, "api_key": "fixture-only"}
        }}));
        (Client::new(Providers::from_parts(&config, &no_env).unwrap()).unwrap(), count)
    }

    /// Runs one request; returns its result and the text streamed, with
    /// each retry shown as `<retry N>`, and each retry's reason.
    async fn run(client: &Client) -> (Result<Response>, String, Vec<String>) {
        let (mut streamed, mut reasons) = (String::new(), Vec::new());
        let result = client
            .create("sys", &[], &[Message::user(vec![json!({"type": "text", "text": "hi"})])], &mut |e| match e {
                StreamEvent::TextDelta(t) => streamed.push_str(t),
                StreamEvent::Retry { attempt, error, .. } => {
                    reasons.push(retry_reason(error));
                    streamed.push_str(&format!("<retry {attempt}>"));
                }
                _ => {}
            })
            .await;
        (result, streamed, reasons)
    }

    #[tokio::test]
    async fn a_response_that_breaks_off_is_requested_again() {
        let (client, count) = serve(vec![
            Answer::Cut(text_events("Hel")),
            Answer::Events(text_events("He")), // ends without message_stop
            Answer::Events(finished("Hello")),
        ])
        .await;
        let (result, streamed, reasons) = run(&client).await;
        let response = result.unwrap();
        assert_eq!(response.content, vec![json!({"type": "text", "text": "Hello"})]);
        assert_eq!(streamed, "Hel<retry 1>He<retry 2>Hello");
        assert_eq!(count.load(Ordering::SeqCst), 3);
        // Each reason says how far the stream got.
        assert!(reasons[0].starts_with("reading response stream after "), "{reasons:?}");
        assert!(reasons[0].contains("(3 events, the last content_block_delta/text_delta)"), "{reasons:?}");
        assert!(reasons[1].starts_with("stream ended before message_stop after "), "{reasons:?}");
    }

    #[tokio::test]
    async fn only_passing_stream_errors_are_retried() {
        let error = |kind: &str| {
            let mut events = text_events("x");
            events.push(json!({"type": "error", "error": {"type": kind, "message": "m"}}));
            Answer::Events(events)
        };
        let (client, count) = serve(vec![error("invalid_request_error")]).await;
        let (result, streamed, _) = run(&client).await;
        assert!(format!("{:#}", result.unwrap_err()).contains("invalid_request_error"));
        assert_eq!(streamed, "x");
        assert_eq!(count.load(Ordering::SeqCst), 1);

        let (client, count) = serve(vec![error("overloaded_error"), Answer::Events(finished("ok"))]).await;
        let (result, streamed, reasons) = run(&client).await;
        assert_eq!(result.unwrap().content, vec![json!({"type": "text", "text": "ok"})]);
        assert!(reasons[0].starts_with("API stream error: "), "{reasons:?}");
        assert_eq!(streamed, "x<retry 1>ok");
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}
