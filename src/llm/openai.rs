//! OpenAI Responses API backend (`POST /v1/responses`, streaming,
//! stateless with `store: false`).
//!
//! The Anthropic-shaped history is converted to Responses input items:
//!
//! | history block                      | Responses input item                      |
//! |------------------------------------|-------------------------------------------|
//! | user `text` / `image` / `document` | user message: `input_text` / `input_image` / `input_file` |
//! | assistant `text`                   | assistant message with `output_text`      |
//! | assistant `tool_use`               | `function_call`                           |
//! | user `tool_result`                 | `function_call_output` (images follow in a user message) |
//! | `openai_reasoning` (same model)    | the original `reasoning` item             |
//! | `thinking`                         | dropped (Anthropic-only)                  |
//!
//! Output items come back the other way: `message` text becomes `text`
//! blocks, `function_call` becomes `tool_use`, and a `reasoning` item that
//! carries `encrypted_content` is kept as an `openai_reasoning` block so it
//! can be replayed to the same model on the next request.

use anyhow::Result;
use serde_json::{Value, json};

use super::config::Target;
use super::{Message, Response, StreamEvent, ToolDef, Usage, http, parse_tool_input, same_source};

pub(super) async fn create(
    http_client: &reqwest::Client,
    target: &Target<'_>,
    system: &str,
    tools: &[ToolDef],
    messages: &[Message],
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
) -> Result<Response> {
    let body = build_body(target, system, tools, messages);
    let resp = http::post_with_retries(http_client, &target.url("responses"), &target.headers(), &body).await?;

    let mut acc = Accumulator::default();
    http::read_sse(resp, "response.completed", |event| acc.apply(&event, on_event)).await?;
    Ok(acc.finish())
}

fn build_body(target: &Target<'_>, system: &str, tools: &[ToolDef], messages: &[Message]) -> Value {
    let tools: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
                "strict": false,
            })
        })
        .collect();
    let mut body = json!({
        "model": target.model,
        "instructions": system,
        "input": to_input(messages, &target.spec()),
        "tools": tools,
        "parallel_tool_calls": true,
        "max_output_tokens": target.max_tokens(),
        "stream": true,
        // Stateless: the full history is sent every time, with reasoning
        // carried as encrypted items.
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "reasoning": {"effort": target.effort.as_str()},
    });
    target.apply_extra_body(&mut body);
    body
}

/// Converts the history into Responses input items.
fn to_input(messages: &[Message], spec: &str) -> Vec<Value> {
    let mut input = Vec::new();
    for message in messages {
        match message.role {
            "assistant" => assistant_items(message, same_source(message, spec), &mut input),
            _ => user_items(message, &mut input),
        }
    }
    input
}

fn assistant_items(message: &Message, own: bool, input: &mut Vec<Value>) {
    let mut text = Vec::new();
    let flush = |text: &mut Vec<Value>, input: &mut Vec<Value>| {
        if !text.is_empty() {
            input.push(json!({"type": "message", "role": "assistant", "content": std::mem::take(text)}));
        }
    };
    for block in &message.content {
        match block["type"].as_str().unwrap_or("") {
            "text" => text.push(json!({"type": "output_text", "text": block["text"]})),
            "tool_use" => {
                flush(&mut text, input);
                input.push(json!({
                    "type": "function_call",
                    "call_id": block["id"],
                    "name": block["name"],
                    "arguments": block["input"].to_string(),
                }));
            }
            "openai_reasoning" if own => {
                flush(&mut text, input);
                input.push(block["item"].clone());
            }
            _ => {} // thinking, fallback markers, other models' reasoning
        }
    }
    flush(&mut text, input);
}

fn user_items(message: &Message, input: &mut Vec<Value>) {
    let mut parts = Vec::new();
    let mut result_images = Vec::new();
    for block in &message.content {
        match block["type"].as_str().unwrap_or("") {
            "tool_result" => {
                let (text, images) = split_tool_result(&block["content"]);
                let output = if block["is_error"] == true { format!("Error: {text}") } else { text };
                input.push(json!({"type": "function_call_output", "call_id": block["tool_use_id"], "output": output}));
                result_images.extend(images);
            }
            _ => parts.extend(user_part(block)),
        }
    }
    // Function call outputs are plain text for broad gateway support; any
    // images a tool returned follow in a user message.
    if !result_images.is_empty() {
        let mut content = vec![json!({"type": "input_text", "text": "Images returned by the tool calls above:"})];
        content.extend(result_images);
        input.push(json!({"role": "user", "content": content}));
    }
    if !parts.is_empty() {
        input.push(json!({"role": "user", "content": parts}));
    }
}

fn user_part(block: &Value) -> Option<Value> {
    match block["type"].as_str().unwrap_or("") {
        "text" => Some(json!({"type": "input_text", "text": block["text"]})),
        "image" => image_part(block),
        "document" if block["source"]["type"] == "base64" => {
            let src = &block["source"];
            Some(json!({
                "type": "input_file",
                "filename": block["title"].as_str().unwrap_or("document.pdf"),
                "file_data": format!("data:{};base64,{}", src["media_type"].as_str().unwrap_or("application/pdf"), src["data"].as_str().unwrap_or("")),
            }))
        }
        _ => None,
    }
}

fn image_part(block: &Value) -> Option<Value> {
    let src = &block["source"];
    let url = match src["type"].as_str()? {
        "base64" => format!("data:{};base64,{}", src["media_type"].as_str()?, src["data"].as_str()?),
        "url" => src["url"].as_str()?.to_string(),
        _ => return None,
    };
    Some(json!({"type": "input_image", "image_url": url}))
}

/// Splits `tool_result` content (a string or a list of blocks) into text
/// and `input_image` parts.
fn split_tool_result(content: &Value) -> (String, Vec<Value>) {
    match content {
        Value::String(s) => (s.clone(), Vec::new()),
        Value::Array(blocks) => {
            let text: Vec<&str> = blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
            let images = blocks.iter().filter(|b| b["type"] == "image").filter_map(image_part).collect();
            (text.join("\n"), images)
        }
        _ => (String::new(), Vec::new()),
    }
}

/// Collects output items from the event stream.
#[derive(Default)]
struct Accumulator {
    items: Vec<Value>,
    /// The final `response` object, from `response.completed` or
    /// `response.incomplete`.
    final_response: Option<Value>,
    /// Whether text has streamed since the last completed message item.
    text_open: bool,
}

impl Accumulator {
    /// Applies one event; returns true once the response is finished.
    fn apply(&mut self, event: &Value, on_event: &mut (dyn FnMut(StreamEvent) + Send)) -> Result<bool> {
        match event["type"].as_str().unwrap_or("") {
            "response.output_text.delta" => {
                let delta = event["delta"].as_str().unwrap_or("");
                if !delta.is_empty() {
                    self.text_open = true;
                    on_event(StreamEvent::TextDelta(delta));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                on_event(StreamEvent::ThinkingDelta(event["delta"].as_str().unwrap_or("")));
            }
            "response.function_call_arguments.delta" => {
                on_event(StreamEvent::ToolInputDelta(event["delta"].as_str().unwrap_or("")));
            }
            "response.output_item.added" => {
                let item = &event["item"];
                match item["type"].as_str().unwrap_or("") {
                    "reasoning" => on_event(StreamEvent::ThinkingDelta("")),
                    "function_call" => on_event(StreamEvent::ToolInputStart(item["name"].as_str().unwrap_or(""))),
                    _ => {}
                }
            }
            "response.output_item.done" => {
                let item = event["item"].clone();
                if item["type"] == "message" {
                    self.end_text(&item, on_event);
                }
                self.items.push(item);
            }
            "response.completed" | "response.incomplete" => {
                let response = event["response"].clone();
                if std::mem::take(&mut self.text_open) {
                    on_event(StreamEvent::TextEnd);
                } else if self.items.is_empty() {
                    // Nothing streamed: show the text of the final output,
                    // which `finish` falls back to.
                    for item in response["output"].as_array().into_iter().flatten().filter(|i| i["type"] == "message") {
                        self.end_text(item, on_event);
                    }
                }
                self.final_response = Some(response);
                return Ok(true);
            }
            "response.failed" => {
                let error = &event["response"]["error"];
                let kind = error["code"].as_str().unwrap_or("");
                return Err(http::stream_error(format!("response failed: {error}"), kind));
            }
            "error" => {
                let error = event.get("error").unwrap_or(event);
                let kind = ["type", "code"].iter().find_map(|k| error[k].as_str().filter(|&s| s != "error"));
                return Err(http::stream_error(format!("API stream error: {error}"), kind.unwrap_or("")));
            }
            _ => {}
        }
        Ok(false)
    }

    /// Ends a message's streamed text, or shows its text at once if none
    /// streamed (some gateways send only whole items).
    fn end_text(&mut self, message: &Value, on_event: &mut (dyn FnMut(StreamEvent) + Send)) {
        if !std::mem::take(&mut self.text_open) {
            let text = message_texts(message).concat();
            if text.is_empty() {
                return;
            }
            on_event(StreamEvent::TextDelta(&text));
        }
        on_event(StreamEvent::TextEnd);
    }

    fn finish(mut self) -> Response {
        let final_response = self.final_response.unwrap_or_default();
        // Gateways that skip `output_item.done` still send the full output
        // on completion.
        if self.items.is_empty()
            && let Some(output) = final_response["output"].as_array()
        {
            self.items = output.clone();
        }

        let mut response = Response::default();
        let usage = &final_response["usage"];
        if usage.is_object() {
            response.usage = Some(Usage {
                input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
                output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
            });
        }
        let mut has_calls = false;
        for item in &self.items {
            match item["type"].as_str().unwrap_or("") {
                "message" => {
                    for text in message_texts(item) {
                        response.content.push(json!({"type": "text", "text": text}));
                    }
                }
                "function_call" => {
                    has_calls = true;
                    let id = item["call_id"].as_str().unwrap_or("").to_string();
                    let raw = item["arguments"].as_str().unwrap_or("");
                    let input = parse_tool_input(raw).unwrap_or_else(|| {
                        response.invalid_tool_inputs.insert(id.clone(), raw.to_string());
                        json!({})
                    });
                    response.content.push(json!({"type": "tool_use", "id": id, "name": item["name"], "input": input}));
                }
                "reasoning" if item["encrypted_content"].is_string() => {
                    response.content.push(json!({"type": "openai_reasoning", "item": item}));
                }
                _ => {}
            }
        }

        let incomplete = final_response["incomplete_details"]["reason"].as_str();
        response.stop_reason = Some(
            match incomplete {
                Some("max_output_tokens") => "max_tokens",
                Some("content_filter") => "refusal",
                _ if has_calls => "tool_use",
                _ => "end_turn",
            }
            .to_string(),
        );
        if incomplete == Some("content_filter") {
            response.stop_details = Some(final_response["incomplete_details"].clone());
        }
        response
    }
}

/// The text parts of a `message` output item.
fn message_texts(message: &Value) -> Vec<String> {
    message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| match part["type"].as_str().unwrap_or("") {
            "output_text" => Some(part["text"].as_str().unwrap_or("").to_string()),
            "refusal" => Some(format!("[refusal] {}", part["refusal"].as_str().unwrap_or(""))),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Providers;
    use crate::llm::config::tests::{file, no_env};

    fn providers() -> Providers {
        Providers::from_parts(
            &file(json!({
                "model": "gw/gpt-5",
                "providers": {"gw": {"protocol": "openai-responses", "base_url": "https://gw.example.com/v1", "api_key": "k",
                    "extra_body": {"include": null, "reasoning": {"effort": "high"}}}}
            })),
            &no_env,
        )
        .unwrap()
    }

    fn run(events: &[Value]) -> (Response, String) {
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
    fn converts_history_to_input_items() {
        let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc"});
        let messages = vec![
            Message::user(vec![
                json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAA"}}),
                json!({"type": "text", "text": "看图"}),
            ]),
            Message {
                role: "assistant",
                content: vec![
                    json!({"type": "openai_reasoning", "item": reasoning}),
                    json!({"type": "text", "text": "Let me check."}),
                    json!({"type": "tool_use", "id": "call_1", "name": "grep", "input": {"pattern": "x"}}),
                ],
                source: Some("gw/gpt-5".into()),
            },
            Message::user(vec![json!({"type": "tool_result", "tool_use_id": "call_1", "is_error": true, "content": [
                {"type": "text", "text": "boom"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "BBB"}},
            ]})]),
            Message {
                role: "assistant",
                content: vec![
                    json!({"type": "thinking", "thinking": "", "signature": "s"}),
                    json!({"type": "openai_reasoning", "item": {"type": "reasoning", "id": "rs_other"}}),
                    json!({"type": "text", "text": "From another model."}),
                ],
                source: Some("other/claude".into()),
            },
        ];
        assert_eq!(
            to_input(&messages, "gw/gpt-5"),
            vec![
                json!({"role": "user", "content": [
                    {"type": "input_image", "image_url": "data:image/png;base64,AAA"},
                    {"type": "input_text", "text": "看图"},
                ]}),
                reasoning,
                json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Let me check."}]}),
                json!({"type": "function_call", "call_id": "call_1", "name": "grep", "arguments": "{\"pattern\":\"x\"}"}),
                json!({"type": "function_call_output", "call_id": "call_1", "output": "Error: boom"}),
                json!({"role": "user", "content": [
                    {"type": "input_text", "text": "Images returned by the tool calls above:"},
                    {"type": "input_image", "image_url": "data:image/png;base64,BBB"},
                ]}),
                json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "From another model."}]}),
            ]
        );
    }

    #[test]
    fn builds_body_with_tools_and_extra_body() {
        let p = providers();
        let target = p.target("gw/gpt-5").unwrap();
        let tools = vec![ToolDef {
            name: "grep".into(),
            description: "search".into(),
            input_schema: json!({"type": "object"}),
            eager_input_streaming: true,
        }];
        let body = build_body(&target, "sys", &tools, &[Message::user(vec![json!({"type": "text", "text": "hi"})])]);
        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["store"], false);
        assert_eq!(body["reasoning"], json!({"effort": "high"}));
        assert!(body.get("include").is_none(), "extra_body null removes a default");

        // The configured effort is sent as is; extra_body has the last word.
        let target = Target { effort: crate::llm::Effort::Low, ..target };
        let body = build_body(&target, "sys", &tools, &[]);
        assert_eq!(body["reasoning"], json!({"effort": "high"}), "extra_body wins");
        let mut bare = p.provider("gw").unwrap().clone();
        bare.extra_body = None;
        let target = Target { config: &bare, effort: crate::llm::Effort::Max, ..target };
        assert_eq!(build_body(&target, "sys", &tools, &[])["reasoning"], json!({"effort": "max"}));
        assert_eq!(
            body["tools"],
            json!([{"type": "function", "name": "grep", "description": "search", "parameters": {"type": "object"}, "strict": false}])
        );
    }

    #[test]
    fn assembles_streamed_output() {
        let (resp, streamed) = run(&[
            json!({"type": "response.created", "response": {}}),
            json!({"type": "response.output_item.done", "output_index": 0,
                   "item": {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc"}}),
            json!({"type": "response.output_text.delta", "delta": "你"}),
            json!({"type": "response.output_text.delta", "delta": "好"}),
            json!({"type": "response.output_item.done", "output_index": 1,
                   "item": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "你好"}]}}),
            json!({"type": "response.function_call_arguments.delta", "delta": "{\"pat"}),
            json!({"type": "response.output_item.done", "output_index": 2,
                   "item": {"type": "function_call", "call_id": "c1", "name": "grep", "arguments": "{\"pattern\": \"x\"}"}}),
            json!({"type": "response.output_item.done", "output_index": 3,
                   "item": {"type": "function_call", "call_id": "c2", "name": "write", "arguments": "{\"path\": "}}),
            json!({"type": "response.completed", "response": {"status": "completed",
                   "usage": {"input_tokens": 120, "output_tokens": 30}}}),
        ]);
        assert_eq!(streamed, "你好|");
        assert_eq!(resp.usage, Some(Usage { input_tokens: 120, output_tokens: 30 }));
        assert_eq!(resp.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(
            resp.content,
            vec![
                json!({"type": "openai_reasoning", "item": {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc"}}),
                json!({"type": "text", "text": "你好"}),
                json!({"type": "tool_use", "id": "c1", "name": "grep", "input": {"pattern": "x"}}),
                json!({"type": "tool_use", "id": "c2", "name": "write", "input": {}}),
            ]
        );
        assert_eq!(resp.invalid_tool_inputs.get("c2").map(String::as_str), Some("{\"path\": "));
    }

    #[test]
    fn maps_incomplete_and_uses_final_output_fallback() {
        let (resp, streamed) = run(&[json!({"type": "response.incomplete", "response": {
            "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "partial"}]},
                       {"type": "reasoning", "id": "rs_x", "summary": []}]
        }})]);
        assert_eq!(resp.stop_reason.as_deref(), Some("max_tokens"));
        // Reasoning without encrypted content cannot be replayed statelessly.
        assert_eq!(resp.content, vec![json!({"type": "text", "text": "partial"})]);
        assert_eq!(streamed, "partial|", "text that never streamed is shown at the end");

        let (resp, _) = run(&[json!({"type": "response.incomplete", "response": {"incomplete_details": {"reason": "content_filter"}}})]);
        assert_eq!(resp.stop_reason.as_deref(), Some("refusal"));
    }

    #[test]
    fn shows_whole_items_that_did_not_stream() {
        let message = json!({"type": "message", "content": [{"type": "output_text", "text": "hi"}]});
        let (_, streamed) = run(&[
            json!({"type": "response.output_item.done", "item": message}),
            json!({"type": "response.completed", "response": {"output": [message]}}),
        ]);
        assert_eq!(streamed, "hi|", "shown once, from the item");

        // Streamed text is not shown again from the final output.
        let (_, streamed) = run(&[
            json!({"type": "response.output_text.delta", "delta": "hi"}),
            json!({"type": "response.completed", "response": {"output": [message]}}),
        ]);
        assert_eq!(streamed, "hi|");
    }

    #[test]
    fn failed_response_is_an_error() {
        let mut acc = Accumulator::default();
        let err = acc
            .apply(&json!({"type": "response.failed", "response": {"error": {"message": "bad"}}}), &mut |_| {})
            .unwrap_err();
        assert!(err.to_string().contains("bad"));
    }
}
