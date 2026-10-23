//! Anthropic Messages API backend (`POST /v1/messages`, streaming).
//!
//! Content blocks pass through as raw JSON, so assistant turns (including
//! `thinking` blocks) are echoed back to the API unchanged.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Mutex;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::config::{CacheTtl, Target};
use super::{Message, Response, StreamEvent, ToolDef, http, parse_tool_input, references, references_as_text, same_source};

const API_VERSION: &str = "2023-06-01";
/// The server-side tool search tool, which earlier versions used: its
/// calls in saved conversations are left out.
const SERVER_TOOL_SEARCH: &str = "tool_search_tool_bm25";
/// Providers whose requests failed with deferred tools and went through
/// with every tool in full.
static NO_TOOL_SEARCH: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
/// Server-side refusal fallback, `fallbacks: "default"` form.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

pub(super) async fn create(
    http_client: &reqwest::Client,
    target: &Target<'_>,
    system: &str,
    tools: &[ToolDef],
    messages: &[Message],
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
) -> Result<Response> {
    let defer = defers(target) && tools.iter().any(|t| t.defer_loading);
    let body = build_body(target, system, tools, messages, defer);
    let mut headers = target.headers();
    headers.push(("anthropic-version".into(), API_VERSION.into()));
    if target.fallbacks() {
        headers.push(("anthropic-beta".into(), FALLBACK_BETA.into()));
    }
    let url = target.url("messages");
    let resp = match http::post(http_client, &url, &headers, &body, target.gzip()).await {
        // Perhaps the model cannot defer tools: every tool in full then, and
        // so from now on with this provider if that works.
        Err(e) if defer && matches!(http::status(&e), Some(400 | 422)) => {
            let body = build_body(target, system, tools, messages, false);
            let resp = http::post(http_client, &url, &headers, &body, target.gzip()).await?;
            NO_TOOL_SEARCH.lock().unwrap().insert(target.config.base_url.clone());
            resp
        }
        resp => resp?,
    };

    let mut acc = Accumulator::default();
    let result = http::read_sse(resp, "message_stop", |event| acc.apply(&event, on_event)).await;
    if result.is_err() {
        // The request still used tokens: report what message_start told us.
        if let Some(usage) = acc.response.usage.filter(|u| u.input_tokens > 0) {
            on_event(StreamEvent::PartialUsage(usage));
        }
        result?;
    }
    let mut response = acc.finish();
    response.content = content_for_history(&response.content);
    Ok(response)
}

/// Whether requests to `target` may defer tools, for the model to load
/// with `tool_search`: when the provider has `tool_search` on and has not
/// refused it.
pub(super) fn defers(target: &Target<'_>) -> bool {
    target.tool_search() && !NO_TOOL_SEARCH.lock().unwrap().contains(&target.config.base_url)
}

/// The request body. With `defer`, the tools marked so are sent with
/// `defer_loading`; without it, every tool goes in full and what
/// `tool_search` loaded is told as text.
fn build_body(target: &Target<'_>, system: &str, tools: &[ToolDef], messages: &[Message], defer: bool) -> Value {
    let spec = target.spec();
    let eager = target.eager_input_streaming();
    let names: HashSet<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    let tools: Vec<Value> = tools
        .iter()
        .map(|t| {
            let mut def = json!({"name": t.name, "description": t.description, "input_schema": t.input_schema});
            if eager && t.eager_input_streaming {
                def["eager_input_streaming"] = json!(true);
            }
            if defer && t.defer_loading {
                def["defer_loading"] = json!(true);
            }
            def
        })
        .collect();
    let media_inside = target.tool_result_media();
    let messages: Vec<Value> = messages
        .iter()
        .filter_map(|m| {
            let mut content = history_blocks(m, &spec);
            for block in content.iter_mut().filter(|b| b["type"] == "tool_result") {
                block["content"] = tool_references(&block["content"], defer.then_some(&names));
            }
            if !media_inside {
                content = lift_tool_media(content);
            }
            (!content.is_empty()).then(|| json!({"role": m.role, "content": content}))
        })
        .collect();

    let mut body = json!({
        "model": target.model,
        "max_tokens": target.max_tokens(),
        "system": system,
        "tools": tools,
        "messages": messages,
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": target.effort.as_str()},
        "stream": true,
    });
    if let Some(ttl) = target.cache_ttl() {
        // Automatic prompt caching: the growing conversation prefix is
        // re-read from cache on every loop iteration.
        body["cache_control"] = match ttl {
            CacheTtl::FiveMinutes => json!({"type": "ephemeral"}),
            CacheTtl::OneHour => json!({"type": "ephemeral", "ttl": "1h"}),
        };
    }
    if target.fallbacks() {
        // On a safety decline, the API re-runs the request on a fallback
        // model chosen by refusal category.
        body["fallbacks"] = json!("default");
    }
    target.apply_extra_body(&mut body);
    body
}

/// A message's blocks as sent to `spec`: OpenAI reasoning items never go to
/// Anthropic, thinking blocks only go back to the model that wrote them,
/// and server-side tool searches, from earlier versions, not at all.
fn history_blocks(message: &Message, spec: &str) -> Vec<Value> {
    let own = same_source(message, spec);
    message
        .content
        .iter()
        .filter(|b| match b["type"].as_str().unwrap_or("") {
            "openai_reasoning" => false,
            "thinking" | "redacted_thinking" => own,
            "server_tool_use" if b["name"] == SERVER_TOOL_SEARCH => false,
            "tool_search_tool_result" => false,
            _ => true,
        })
        .cloned()
        .collect()
}

/// A tool result's `content` with its `tool_reference` blocks as the
/// request takes them: as they are if it defers tools and has the tool
/// (`names`), otherwise as text. The API refuses a reference to a tool it
/// was not sent, such as one of an MCP server that is gone.
fn tool_references(content: &Value, names: Option<&HashSet<&str>>) -> Value {
    let Some(names) = names else { return references_as_text(content) };
    let missing: Vec<&str> = references(content).into_iter().filter(|n| !names.contains(n)).collect();
    if missing.is_empty() {
        return content.clone();
    }
    let kept: Vec<Value> = content.as_array().into_iter().flatten()
        .filter(|b| b["type"] != "tool_reference" || b["tool_name"].as_str().is_some_and(|n| names.contains(n)))
        .cloned()
        .collect();
    if kept.is_empty() {
        return Value::String(format!("Loaded {}, which are no longer available.", missing.join(", ")));
    }
    Value::Array(kept)
}

/// Moves the images and PDFs out of a user message's tool results, to
/// follow the last result as blocks of their own: many gateways drop them
/// from tool results but pass them in a user message.
fn lift_tool_media(blocks: Vec<Value>) -> Vec<Value> {
    let mut lifted = Vec::new();
    let mut blocks: Vec<Value> = blocks
        .into_iter()
        .map(|mut block| {
            if block["type"] == "tool_result"
                && let Some(items) = block["content"].as_array()
                && items.iter().any(is_media)
            {
                let (media, mut text): (Vec<Value>, Vec<Value>) = items.iter().cloned().partition(is_media);
                text.push(json!({"type": "text", "text": "(The image or PDF itself follows the tool results.)"}));
                lifted.extend(media);
                block["content"] = Value::Array(text);
            }
            block
        })
        .collect();
    if !lifted.is_empty() {
        let at = blocks.iter().rposition(|b| b["type"] == "tool_result").map_or(0, |i| i + 1);
        let mut follow = vec![json!({"type": "text", "text": "Files returned by the tool calls above:"})];
        follow.extend(lifted);
        blocks.splice(at..at, follow);
    }
    blocks
}

fn is_media(block: &Value) -> bool {
    matches!(block["type"].as_str(), Some("image" | "document"))
}

/// Folds stream events into complete content blocks.
#[derive(Default)]
struct Accumulator {
    response: Response,
    /// Block index -> accumulated `input_json_delta` fragments.
    partial_json: HashMap<usize, String>,
    /// Whether text has streamed since the last `TextEnd`. Consecutive text
    /// blocks (split by citations, or by gateways that open a block per
    /// chunk) are one run of text, ended by a tool call or the message end.
    text_open: bool,
}

impl Accumulator {
    /// Applies one event; returns true on `message_stop`.
    fn apply(&mut self, event: &Value, on_event: &mut (dyn FnMut(StreamEvent) + Send)) -> Result<bool> {
        let content = &mut self.response.content;
        match event["type"].as_str().unwrap_or("") {
            "content_block_start" => {
                let index = event["index"].as_u64().unwrap_or(content.len() as u64) as usize;
                if content.len() <= index {
                    content.resize(index + 1, Value::Null);
                }
                content[index] = event["content_block"].clone();
                let kind = content[index]["type"].as_str().unwrap_or("");
                if !matches!(kind, "text" | "thinking" | "redacted_thinking") && std::mem::take(&mut self.text_open) {
                    on_event(StreamEvent::TextEnd);
                }
                match kind {
                    "thinking" | "redacted_thinking" => on_event(StreamEvent::ThinkingDelta("")),
                    "tool_use" => on_event(StreamEvent::ToolInputStart(content[index]["name"].as_str().unwrap_or(""))),
                    _ => {}
                }
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
                        self.text_open = true;
                        on_event(StreamEvent::TextDelta(text));
                    }
                    "thinking_delta" => {
                        let thinking = delta["thinking"].as_str().unwrap_or("");
                        append_str(block, "thinking", thinking);
                        on_event(StreamEvent::ThinkingDelta(thinking));
                    }
                    "signature_delta" => block["signature"] = delta["signature"].clone(),
                    "input_json_delta" => {
                        let json = delta["partial_json"].as_str().unwrap_or("");
                        self.partial_json.entry(index).or_default().push_str(json);
                        on_event(StreamEvent::ToolInputDelta(json));
                    }
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
                if block["type"] == "server_tool_use" {
                    // A server tool's input, which the API has run already.
                    if let Some(input) = self.partial_json.remove(&index).as_deref().and_then(parse_tool_input) {
                        block["input"] = input;
                    }
                } else if block["type"] == "tool_use" {
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
            }
            // Usage comes at message_start and, cumulatively, at
            // message_delta; some gateways only fill in the latter.
            "message_start" => self.apply_usage(&event["message"]["usage"]),
            "message_delta" => {
                self.apply_usage(&event["usage"]);
                let delta = &event["delta"];
                if let Some(reason) = delta["stop_reason"].as_str() {
                    self.response.stop_reason = Some(reason.to_string());
                }
                if !delta["stop_details"].is_null() {
                    self.response.stop_details = Some(delta["stop_details"].clone());
                }
            }
            "message_stop" => {
                if std::mem::take(&mut self.text_open) {
                    on_event(StreamEvent::TextEnd);
                }
                return Ok(true);
            }
            "error" => {
                let error = &event["error"];
                let kind = error["type"].as_str().unwrap_or("");
                return Err(http::stream_error(format!("API stream error: {error}"), kind));
            }
            _ => {} // ping, and future event types
        }
        Ok(false)
    }

    fn apply_usage(&mut self, usage: &Value) {
        if !usage.is_object() {
            return;
        }
        let input: Vec<u64> = ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"]
            .iter()
            .filter_map(|k| usage[k].as_u64())
            .collect();
        let total = self.response.usage.get_or_insert_default();
        if input.iter().sum::<u64>() > 0 {
            total.input_tokens = input.iter().sum();
            total.cache_write_tokens = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            total.cache_read_tokens = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
        }
        if let Some(output) = usage["output_tokens"].as_u64() {
            total.output_tokens = output;
        }
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

/// Prepares response content to be echoed back as the assistant turn.
///
/// After a mid-output refusal fallback, blocks before the last `fallback`
/// marker belong to the declined attempt: only its `text` blocks may be sent
/// back. Everything after the marker (and every block when there is no
/// marker) is kept unchanged.
fn content_for_history(content: &[Value]) -> Vec<Value> {
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
    use crate::llm::Usage;
    use crate::llm::Providers;
    use crate::llm::config::tests::{file, no_env};

    #[test]
    fn tool_result_media_follows_the_results_on_gateways() {
        let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAA"}});
        let results = vec![
            json!({"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "shot.png"}, image]}),
            json!({"type": "tool_result", "tool_use_id": "t2", "content": "ok"}),
            json!({"type": "text", "text": "a message that arrived"}),
        ];
        let lifted = lift_tool_media(results.clone());
        assert_eq!(lifted[0]["content"][0], json!({"type": "text", "text": "shot.png"}));
        assert!(lifted[0]["content"][1]["text"].as_str().unwrap().contains("follows the tool results"));
        assert_eq!(lifted[1], results[1]);
        assert_eq!(lifted[2]["text"], "Files returned by the tool calls above:");
        assert_eq!(lifted[3], image);
        assert_eq!(lifted[4], results[2]);
        assert_eq!(lift_tool_media(vec![results[1].clone()]), vec![results[1].clone()], "nothing to move");

        let config = |url: &str| {
            file(json!({"model": "p/m", "providers": {"p": {"protocol": "anthropic", "base_url": url, "api_key": "k"}}}))
        };
        let messages = vec![Message::user(results)];
        for (url, inside) in [("https://gw.example.com", false), ("https://api.anthropic.com", true)] {
            let providers = Providers::from_parts(&config(url), &no_env).unwrap();
            let body = build_body(&providers.target("p/m").unwrap(), "sys", &[], &messages, false);
            let first = &body["messages"][0]["content"][0]["content"];
            assert_eq!(first[1]["type"], if inside { "image" } else { "text" }, "{url}");
        }
    }

    fn tool(name: &str, defer_loading: bool) -> ToolDef {
        ToolDef { name: name.into(), description: "d".into(), input_schema: json!({"type": "object"}), eager_input_streaming: false, defer_loading }
    }

    #[test]
    fn defers_marked_tools_and_passes_references() {
        let p = Providers::from_parts(
            &file(json!({"model": "gw/m", "providers": {
                "gw": {"protocol": "anthropic", "base_url": "https://gw.example.com", "tool_search": true},
                "off": {"protocol": "anthropic", "base_url": "https://gw2.example.com"}}})),
            &no_env,
        )
        .unwrap();
        let target = p.target("gw/m").unwrap();
        assert!(defers(&target));
        assert!(!defers(&p.target("off/m").unwrap()), "off for gateways by default");

        let tools = vec![tool("tool_search", false), tool("mcp__gh__issues", true)];
        let loaded = |names: &[&str]| -> Value { names.iter().map(|n| json!({"type": "tool_reference", "tool_name": n})).collect() };
        let old_search = json!({"type": "server_tool_use", "id": "s1", "name": SERVER_TOOL_SEARCH, "input": {"query": "issues"}});
        let old_found = json!({"type": "tool_search_tool_result", "tool_use_id": "s1", "content": {}});
        let call = json!({"type": "tool_use", "id": "t1", "name": "tool_search", "input": {"query": "issues"}});
        let messages = vec![
            Message::user(vec![json!({"type": "text", "text": "list issues"})]),
            Message { role: "assistant", content: vec![old_search, old_found, call], source: Some("gw/m".into()) },
            Message::user(vec![
                json!({"type": "tool_result", "tool_use_id": "t1", "content": loaded(&["mcp__gh__issues", "mcp__gone__x"])}),
                json!({"type": "tool_result", "tool_use_id": "t2", "content": loaded(&["mcp__gone__x"])}),
            ]),
        ];
        let body = build_body(&target, "sys", &tools, &messages, true);
        assert_eq!(body["tools"][0].get("defer_loading"), None);
        assert_eq!(body["tools"][1]["defer_loading"], true);
        assert_eq!(body["tools"].as_array().unwrap().len(), 2, "no server-side search tool");
        assert_eq!(body["messages"][1]["content"].as_array().unwrap().len(), 1, "old server-side searches are left out");
        let results = &body["messages"][2]["content"];
        assert_eq!(results[0]["content"], loaded(&["mcp__gh__issues"]), "references to tools not sent are dropped");
        assert_eq!(results[1]["content"], "Loaded mcp__gone__x, which are no longer available.");

        let body = build_body(&target, "sys", &tools, &messages, false);
        assert_eq!(body["tools"][1].get("defer_loading"), None, "every tool in full");
        assert_eq!(body["messages"][2]["content"][0]["content"], "Loaded mcp__gh__issues, mcp__gone__x; they can be called now.");
    }

    #[test]
    fn references_stay_in_tool_results_on_gateways() {
        let results = vec![json!({"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "tool_reference", "tool_name": "x"}]})];
        assert_eq!(lift_tool_media(results.clone()), results);
    }

    #[tokio::test]
    async fn sends_every_tool_in_full_where_deferring_is_rejected() {
        use std::sync::Arc;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                let deferred = body["tools"].as_array().unwrap().iter().any(|t| t["defer_loading"] == true);
                record.lock().unwrap().push(deferred);
                async move {
                    if deferred {
                        return axum::http::Response::builder().status(400).body(axum::body::Body::from("defer_loading: Extra inputs are not permitted")).unwrap();
                    }
                    let events = [
                        json!({"type": "message_start", "message": {"content": []}}),
                        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
                        json!({"type": "message_stop"}),
                    ];
                    let sse: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
                    axum::http::Response::builder().header("content-type", "text/event-stream").body(axum::body::Body::from(sse)).unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = file(json!({"model": "gw/m", "providers": {"gw": {"protocol": "anthropic", "base_url": url, "tool_search": true}}}));
        let providers = Providers::from_parts(&config, &no_env).unwrap();
        let target = providers.target("gw/m").unwrap();
        let tools = vec![tool("tool_search", false), tool("mcp__big__x", true)];
        let http = reqwest::Client::new();
        let messages = [Message::user(vec![json!({"type": "text", "text": "hi"})])];
        for _ in 0..2 {
            create(&http, &target, "sys", &tools, &messages, &mut |_| {}).await.unwrap();
        }
        assert_eq!(*seen.lock().unwrap(), [true, false, false], "tried once, then in full");
        assert!(!defers(&target));
    }

    #[test]
    fn builds_body_per_provider_and_filters_foreign_blocks() {
        let p = Providers::from_parts(
            &file(json!({
                "model": "gw/claude-sonnet-4-5",
                "providers": {"gw": {"protocol": "anthropic", "base_url": "https://gw.example.com", "api_key": "k",
                    "models": {"claude-sonnet-4-5": {"extra_body": {"thinking": null}}}}}
            })),
            &no_env,
        )
        .unwrap();
        let target = p.target("gw/claude-sonnet-4-5").unwrap();
        let tools = vec![ToolDef {
            name: "write".into(),
            description: "d".into(),
            input_schema: json!({"type": "object"}),
            eager_input_streaming: true,
            defer_loading: false,
        }];
        let thinking = json!({"type": "thinking", "thinking": "", "signature": "s"});
        let messages = vec![
            Message::user(vec![json!({"type": "text", "text": "hi"})]),
            Message {
                role: "assistant",
                content: vec![thinking.clone(), json!({"type": "text", "text": "from gpt"}),
                              json!({"type": "openai_reasoning", "item": {}})],
                source: Some("other/gpt-5".into()),
            },
            Message::user(vec![json!({"type": "text", "text": "again"})]),
            Message {
                role: "assistant",
                content: vec![thinking.clone(), json!({"type": "text", "text": "mine"})],
                source: Some("gw/claude-sonnet-4-5".into()),
            },
        ];
        let body = build_body(&target, "sys", &tools, &messages, false);
        assert!(body.get("fallbacks").is_none(), "gateways don't get fallbacks by default");
        assert!(body.get("thinking").is_none(), "model extra_body removed thinking");
        assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(body["tools"][0].get("eager_input_streaming"), None);
        assert_eq!(body["messages"][1]["content"], json!([{"type": "text", "text": "from gpt"}]));
        assert_eq!(body["messages"][3]["content"], json!([thinking, {"type": "text", "text": "mine"}]));
        assert_eq!(body["output_config"], json!({"effort": "high"}), "the default");

        let target = Target { effort: crate::llm::Effort::XHigh, ..target };
        let body = build_body(&target, "sys", &tools, &messages, false);
        assert_eq!(body["output_config"], json!({"effort": "xhigh"}));

        let p = Providers::from_parts(
            &file(json!({"model": "gw/m", "providers": {"gw": {"protocol": "anthropic",
                "base_url": "https://gw.example.com", "prompt_cache_ttl": "1h"}}})),
            &no_env,
        )
        .unwrap();
        let body = build_body(&p.target("gw/m").unwrap(), "sys", &tools, &messages, false);
        assert_eq!(body["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
    }

    #[test]
    fn assembles_server_tool_calls() {
        let (response, _) = run_events(&[
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "s1", "name": "web_search", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"query\": \"gh\"}"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "web_search_tool_result", "tool_use_id": "s1", "content": []}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_stop"}),
        ]);
        assert_eq!(response.content[0]["input"], json!({"query": "gh"}));
        assert_eq!(response.content[1]["type"], "web_search_tool_result");
        assert!(response.invalid_tool_inputs.is_empty());
    }

    fn run_events(events: &[Value]) -> (Response, String) {
        let mut acc = Accumulator::default();
        let mut streamed = String::new();
        let mut on_event = |e: StreamEvent| match e {
            StreamEvent::TextDelta(t) => streamed.push_str(t),
            StreamEvent::TextEnd => streamed.push('|'),
            _ => {}
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
            json!({"type": "message_start", "message": {"content": [], "usage":
                {"input_tokens": 10, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 5, "output_tokens": 1}}}),
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
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 42}}),
            json!({"type": "message_stop"}),
        ]);
        assert_eq!(streamed, "Hello|");
        assert_eq!(
            resp.usage,
            Some(Usage { input_tokens: 115, cache_read_tokens: 100, cache_write_tokens: 5, output_tokens: 42 })
        );
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
    fn text_split_across_blocks_is_one_run() {
        let empty_thinking = json!({"type": "thinking", "thinking": ""});
        let (_, streamed) = run_events(&[
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "R"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": empty_thinking}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": "ust"}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "message_stop"}),
        ]);
        assert_eq!(streamed, "Rust|");
    }

    #[test]
    fn reads_usage_reported_at_the_end() {
        let (resp, _) = run_events(&[
            json!({"type": "message_start", "message": {"content": [], "usage": {"input_tokens": 0, "output_tokens": 0}}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"input_tokens": 816, "output_tokens": 46}}),
        ]);
        assert_eq!(resp.usage, Some(Usage { input_tokens: 816, output_tokens: 46, ..Usage::default() }));
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
