//! The agent loop: send the conversation, run any requested tools, send the
//! results back, and repeat until the model ends its turn.

use anyhow::{Result, bail};
use futures::future::join_all;
use serde_json::{Value, json};

use crate::api::{Client, Message, content_for_history};
use crate::tools::ToolRegistry;

const MAX_ITERATIONS: usize = 50;

/// Receives progress events so the frontend decides how to display them.
pub trait Observer {
    fn on_text(&mut self, text: &str);
    fn on_tool_call(&mut self, name: &str, input: &Value);
    fn on_tool_result(&mut self, name: &str, content: &str, is_error: bool);
}

pub struct Agent {
    client: Client,
    tools: ToolRegistry,
    system: String,
    messages: Vec<Message>,
}

impl Agent {
    pub fn new(client: Client, tools: ToolRegistry, system: impl Into<String>) -> Self {
        Self { client, tools, system: system.into(), messages: Vec::new() }
    }

    pub fn model(&self) -> &str {
        self.client.model()
    }

    /// Runs one user turn to completion. The history only ever grows by
    /// appending, so earlier turns (and their thinking blocks) stay intact.
    pub async fn run_turn(&mut self, prompt: &str, observer: &mut dyn Observer) -> Result<()> {
        self.messages.push(Message::user_text(prompt));
        let definitions = self.tools.definitions();

        for _ in 0..MAX_ITERATIONS {
            let response = self.client.create(&self.system, &definitions, &self.messages).await?;
            let stop_reason = response.stop_reason.as_deref().unwrap_or("");

            if stop_reason == "refusal" {
                // Nothing usable to append; drop the prompt so the history
                // stays a valid alternation of user/assistant turns.
                self.rollback_turn();
                let detail = response.stop_details.map(|d| d.to_string()).unwrap_or_default();
                bail!("the model declined this request {detail}");
            }

            let content = content_for_history(&response.content);
            for block in &content {
                if block["type"] == "text" {
                    observer.on_text(block["text"].as_str().unwrap_or(""));
                }
            }
            let tool_uses: Vec<Value> =
                content.iter().filter(|b| b["type"] == "tool_use").cloned().collect();
            self.messages.push(Message { role: "assistant", content });

            match stop_reason {
                "tool_use" => {}
                "max_tokens" if !tool_uses.is_empty() => {
                    // A tool_use block cut off by max_tokens may carry partial
                    // input; don't execute it.
                    self.rollback_turn();
                    bail!("response hit max_tokens while writing a tool call; raise HARNESS_MAX_TOKENS");
                }
                "max_tokens" => {
                    observer.on_text("\n[stopped: max_tokens reached]");
                    return Ok(());
                }
                _ => return Ok(()),
            }

            for t in &tool_uses {
                observer.on_tool_call(t["name"].as_str().unwrap_or(""), &t["input"]);
            }
            // Run all tool calls from this response concurrently, then send
            // every result back in a single user message.
            let outputs = join_all(tool_uses.iter().map(|t| {
                self.tools.run(t["name"].as_str().unwrap_or(""), t["input"].clone())
            }))
            .await;

            let mut results = Vec::with_capacity(outputs.len());
            for (t, out) in tool_uses.iter().zip(outputs) {
                observer.on_tool_result(t["name"].as_str().unwrap_or(""), &out.content, out.is_error);
                results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": t["id"],
                    "content": out.content,
                    "is_error": out.is_error,
                }));
            }
            self.messages.push(Message { role: "user", content: results });
        }

        bail!("stopped after {MAX_ITERATIONS} iterations without the model finishing")
    }

    /// Drops everything back to (and including) the last plain user prompt.
    fn rollback_turn(&mut self) {
        while let Some(m) = self.messages.pop() {
            let is_prompt = m.role == "user" && m.content.iter().all(|b| b["type"] == "text");
            if is_prompt {
                break;
            }
        }
    }
}
