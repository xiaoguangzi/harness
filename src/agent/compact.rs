//! Compaction: before the conversation outgrows the model's context window,
//! the model summarizes it, and the summary replaces the history.
//!
//! This follows Codex's local compaction: the history plus a summarization
//! prompt is sent as an ordinary request (same system prompt and tools, so
//! the cached prefix still applies), and the new history is the user's own
//! recent messages, the summary, and, as Claude Code does, the files read
//! most recently, as they are now. The assistant turns and tool results go.

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{Agent, INTERRUPTED, Observer};
use crate::llm::{Message, StreamEvent, ToolDef};

/// The conversation is compacted once the next request would leave less
/// than `max_tokens` of the context window for the response, plus this
/// share of the window for what the token estimate misses.
const MARGIN_PERCENT: u64 = 5;
/// More compactions than this in one turn means something refills the
/// context faster than compaction frees it.
const MAX_PER_TURN: u32 = 3;
/// Bytes of the user's own messages kept word for word (about 20k tokens).
const KEPT_USER_BYTES: usize = 80_000;
/// The files read most recently are attached again, each cut to this many
/// bytes (about 5k tokens), and all of them to this share of the context
/// window.
const REATTACHED_FILES: usize = 5;
const REATTACHED_FILE_BYTES: usize = 20_000;
const REATTACHED_PERCENT: u64 = 10;

const PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. The conversation above is about to be \
replaced by the summary you write now, and work will continue from it: the user's own messages are kept, the files \
you read most recently are attached again, and everything else is gone. Do not call any tools; reply with the \
summary text only.

Include:
- What the user asked for and why, with every constraint and preference they stated
- Progress so far and the key decisions made, with their reasons
- Files read, created or changed, and what matters about each (exact paths, function and type names)
- Errors hit and how they were resolved; approaches tried and dropped
- What remains to be done, and the immediate next step if work was in progress
- Any data, commands or references needed to continue

Be concise and structured, and keep paths, identifiers and error messages exact.";

/// Starts the summary in the new history; also how a compacted message is
/// recognized.
const SUMMARY_PREFIX: &str = "This conversation was compacted to fit the context window. The summary below was \
written by the model that did the work so far, and replaces the earlier turns; the user's messages from them are \
above. Build on the work already done and avoid repeating it.";

/// Starts the todo list a compaction carries over.
const TODO_PREFIX: &str = "Your todo list before the compaction (keep it current with todo_write):";
/// Starts the count of the tool calls the summary replaces.
const TOOLS_PREFIX: &str = "Tool calls made before the compaction:";

/// By default, an idle conversation is compacted from this many tokens.
const IDLE_MIN_TOKENS: u64 = 200_000;
/// How long before its prompt cache expires an idle conversation is
/// compacted, so the summary request still reads the cache.
const IDLE_BEFORE_EXPIRY: Duration = Duration::from_secs(45);

/// When an idle conversation is compacted: the settings' `idle_compaction`
/// (default on) and `idle_compaction_min_tokens`.
#[derive(Debug, Clone, Copy)]
struct IdleConfig {
    enabled: bool,
    min_tokens: u64,
}

static IDLE: std::sync::OnceLock<IdleConfig> = std::sync::OnceLock::new();

/// Sets when idle conversations are compacted, from the settings.
pub fn set_idle(enabled: Option<bool>, min_tokens: Option<u64>) {
    let _ = IDLE.set(IdleConfig { enabled: enabled.unwrap_or(true), min_tokens: min_tokens.unwrap_or(IDLE_MIN_TOKENS) });
}

fn idle_config() -> IdleConfig {
    *IDLE.get().unwrap_or(&IdleConfig { enabled: true, min_tokens: IDLE_MIN_TOKENS })
}

/// Whether `message` is the one a compaction left in place of the history.
pub fn is_compacted(message: &Message) -> bool {
    message.role == "user" && message.content.iter().any(is_summary)
}

fn is_summary(block: &Value) -> bool {
    block["type"] == "text" && block["text"].as_str().is_some_and(|t| t.starts_with(SUMMARY_PREFIX))
}

fn has_tool_results(message: &Message) -> bool {
    message.content.iter().any(|b| b["type"] == "tool_result")
}

impl Agent {
    /// Tokens the next request would carry: the size the last response
    /// reported plus an estimate of what was added since, or an estimate
    /// of everything when no response has been counted since the history
    /// last changed.
    pub(super) fn context_size(&self, definitions: &[ToolDef]) -> u64 {
        match self.context {
            Some((len, tokens)) if len <= self.messages.len() => tokens + estimate_messages(&self.messages[len..]),
            _ => {
                // Deferred tools take only their names until they are loaded.
                let tools: u64 = definitions
                    .iter()
                    .map(|d| match d.defer_loading {
                        true => estimate_text(&d.name),
                        false => estimate_text(&d.name) + estimate_text(&d.description) + estimate(&d.input_schema),
                    })
                    .sum();
                estimate_text(&self.system) + tools + estimate_messages(&self.messages)
            }
        }
    }

    fn context_limit(&self) -> u64 {
        context_limit(self.context_window(), self.client.max_tokens())
    }

    /// Compacts the conversation now (`/compact`), outside a turn. `focus`
    /// says what the summary should keep in particular. Nothing happens
    /// to a conversation with nothing to summarize.
    pub async fn compact(&mut self, focus: Option<&str>, observer: &mut dyn Observer) -> Result<()> {
        self.settle();
        let definitions = self.tools.definitions();
        self.compact_history(Trigger::Manual(focus), &definitions, observer).await.map(drop)
    }

    /// How long this conversation may wait for the user before it is
    /// compacted, or `None` if it is not to be: it must take at least the
    /// configured tokens of the context. The wait ends a little before the
    /// prompt cache would expire, so the summary request still reads it,
    /// and the user's next message no longer resends the long history
    /// uncached.
    pub fn idle_compaction_after(&self) -> Option<Duration> {
        self.idle_after(idle_config())
    }

    fn idle_after(&self, config: IdleConfig) -> Option<Duration> {
        if !config.enabled || self.messages.is_empty() || self.context_tokens() < config.min_tokens {
            return None;
        }
        let ttl = self.client.cache_ttl().unwrap_or(crate::llm::config::CacheTtl::FiveMinutes).duration();
        Some(ttl.saturating_sub(IDLE_BEFORE_EXPIRY))
    }

    /// Compacts the conversation while it waits for the user (see
    /// [`Agent::idle_compaction_after`]). Cancelling leaves it as it was.
    pub async fn compact_when_idle(&mut self, observer: &mut dyn Observer) -> Result<()> {
        self.settle();
        let definitions = self.tools.definitions();
        self.compact_history(Trigger::Idle, &definitions, observer).await.map(drop)
    }

    /// Compacts the conversation if the next request would come too close
    /// to the context window, as a turn's loop does before each request.
    pub(super) async fn compact_if_full(&mut self, definitions: &[ToolDef], observer: &mut dyn Observer) -> Result<()> {
        let limit = self.context_limit();
        if self.context_size(definitions) < limit {
            return Ok(());
        }
        if self.compactions >= MAX_PER_TURN {
            bail!(
                "the context filled up again after {MAX_PER_TURN} compactions in this turn; something the turn \
                 reads is too large for the context window"
            );
        }
        // A PreCompact hook may keep the history as it is; the request then
        // goes as it is too.
        if !self.compact_history(Trigger::Auto, definitions, observer).await? {
            return Ok(());
        }
        self.compactions += 1;
        let size = self.context_size(definitions);
        if size >= limit {
            bail!(
                "the conversation still takes about {size} tokens after compacting, over the {limit} allowed; \
                 start a new conversation, or send less at once"
            );
        }
        Ok(())
    }

    /// Asks the model for a summary of the history and replaces the history
    /// with it. A user message that has not been answered yet (a turn about
    /// to start) stays as it is, after the summary. The history changes
    /// only once the summary is in, so cancelling leaves it as it was.
    /// Returns false if a PreCompact hook blocked it.
    async fn compact_history(
        &mut self,
        trigger: Trigger<'_>,
        definitions: &[ToolDef],
        observer: &mut dyn Observer,
    ) -> Result<bool> {
        let (trigger, focus) = match trigger {
            Trigger::Manual(focus) => ("manual", focus),
            Trigger::Auto | Trigger::Idle => ("auto", None),
        };
        let pending = match self.messages.last() {
            Some(last) if last.role == "user" && !has_tool_results(last) => 1,
            _ => 0,
        };
        if self.messages.len() == pending {
            return Ok(true);
        }
        let outcome = self.hooks.pre_compact(trigger, focus.unwrap_or_default(), self.client.spec()).await;
        super::show_notices(&outcome, observer);
        if let Some(reason) = outcome.block.or(outcome.halt) {
            let reason = if reason.is_empty() { String::new() } else { format!(": {reason}") };
            observer.on_hook(&format!("A PreCompact hook kept the conversation from being compacted{reason}"));
            return Ok(false);
        }
        let history = &self.messages[..self.messages.len() - pending];
        let before = self.context_size(definitions);
        observer.on_compact_start();

        let mut prompt = PROMPT.to_string();
        if let Some(focus) = focus.map(str::trim).filter(|f| !f.is_empty()) {
            prompt.push_str(&format!("\n\nThe user asked the summary to focus on: {focus}"));
        }
        let mut request = history.to_vec();
        let prompt = json!({"type": "text", "text": prompt});
        match request.last_mut() {
            Some(last) if last.role == "user" => last.content.push(prompt),
            _ => request.push(Message::user(vec![prompt])),
        }
        // The summary is not shown as it streams.
        let (log, model) = (&self.usage, self.client.spec());
        let mut on_event = |event: StreamEvent| match event {
            StreamEvent::Retry { error, attempt, max, wait } => observer.on_retry(error, attempt, max, wait),
            StreamEvent::PartialUsage(usage) => log.record(model, &usage),
            _ => {}
        };
        let response = loop {
            match self.client.create(&self.system, definitions, &request, &mut on_event).await {
                // Too long even to summarize: leave out the oldest part.
                Err(e) if is_context_overflow(&e) && drop_oldest(&mut request) => {}
                Err(e) => return Err(e.context("compacting the conversation")),
                Ok(response) => break response,
            }
        };
        if let Some(usage) = &response.usage {
            self.usage.record(self.client.spec(), usage);
        }
        let summary: String =
            response.content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
        match response.stop_reason.as_deref() {
            Some("tool_use") => bail!("compacting the conversation: the model called a tool instead of summarizing"),
            Some("max_tokens") => bail!("compacting the conversation: the summary hit max_tokens"),
            Some("refusal") => bail!("compacting the conversation: the model declined to summarize"),
            _ if summary.trim().is_empty() => bail!("compacting the conversation: the model wrote no summary"),
            _ => {}
        }

        let mut content: Vec<Value> =
            kept_user_messages(history).into_iter().map(|text| json!({"type": "text", "text": text})).collect();
        content.push(json!({"type": "text", "text": format!("{SUMMARY_PREFIX}\n\n<summary>\n{}\n</summary>", summary.trim())}));
        if let Some(calls) = tool_counts(history) {
            content.push(json!({"type": "text", "text": format!("{TOOLS_PREFIX} {calls}.")}));
        }
        // The latest todo list goes on as it was.
        let todos = history
            .iter()
            .rev()
            .flat_map(|m| m.content.iter().rev())
            .find(|b| b["type"] == "tool_use" && b["name"] == "todo_write")
            .map(|b| crate::tools::todo::checklist(&b["input"]))
            .filter(|list| !list.is_empty());
        if let Some(list) = todos {
            content.push(json!({"type": "text", "text": format!("{TODO_PREFIX}\n{list}")}));
        }
        // The files it read last, as they are now. The model no longer sees
        // the others, so it must read them again before changing them.
        if let Some(files) = self.tools.files() {
            let mut kept = Vec::new();
            let mut blocks = Vec::new();
            // About four bytes a token.
            let mut room = (self.context_window() / 100 * REATTACHED_PERCENT * 4) as usize;
            for path in files.recent(REATTACHED_FILES) {
                if room < 1_000 {
                    break;
                }
                if let Some(block) = crate::mentions::reattach(&path, room.min(REATTACHED_FILE_BYTES), files) {
                    room = room.saturating_sub(block["text"].as_str().map_or(0, str::len));
                    blocks.push(block);
                    kept.push(path);
                }
            }
            files.retain(&kept);
            if !blocks.is_empty() {
                content.push(json!({"type": "text", "text": "The files read most recently, as they are now:"}));
                content.extend(blocks);
            }
        }
        // Only a prompt not answered yet, kept after the summary, can still
        // be rewound to.
        if self.checkpoints {
            self.history.rebase((pending == 1).then(|| self.messages.len() - 1), 1);
        }
        let loaded = super::deferred::loaded(history);
        let pending = self.messages.pop().filter(|_| pending == 1);
        self.messages = vec![Message::user(content)];
        // The deferred tools, and those loaded, went with the history.
        if let Some(notice) = self.sync_tool_search(&loaded) {
            self.messages[0].content.push(notice);
        }
        self.messages.extend(pending);
        self.context = None;
        self.session_start = Some("compact");
        // The running turn now starts after the summary; if the summary took
        // in the turn's own prompt, it has none to remove after an error.
        if self.turn_start.is_some() {
            self.turn_start = Some(1);
            self.turn_compacted |= self.messages.len() == 1;
        }
        observer.on_compact_end(before, self.context_size(definitions));
        let outcome = self.hooks.post_compact(trigger, summary.trim(), self.client.spec()).await;
        super::show_notices(&outcome, observer);
        Ok(true)
    }
}

/// What starts a compaction.
enum Trigger<'a> {
    /// `/compact`, with what to focus on.
    Manual(Option<&'a str>),
    /// The context filling up.
    Auto,
    /// The conversation waiting for the user.
    Idle,
}

/// How many input tokens a request may carry before the conversation is
/// compacted: the window less room for a full response (the answer, or
/// the summary) and a margin. Never below half the window, so a
/// `max_tokens` close to the window does not make every request compact.
fn context_limit(window: u64, max_tokens: u64) -> u64 {
    window.saturating_sub(max_tokens + window / 100 * MARGIN_PERCENT).max(window / 2)
}

/// The text of the user's own messages, oldest first, as much as fits in
/// [`KEPT_USER_BYTES`] counting from the newest; the one that crosses the
/// limit is cut. Attachments, tool results, earlier summaries and reports
/// from subagents are left out.
/// How many calls of each tool `history` has, most called first:
/// `bash ×12, read ×30`; `None` without any.
fn tool_counts(history: &[Message]) -> Option<String> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for name in history.iter().flat_map(|m| &m.content).filter(|b| b["type"] == "tool_use").filter_map(|b| b["name"].as_str()) {
        match counts.iter_mut().find(|(n, _)| *n == name) {
            Some((_, count)) => *count += 1,
            None => counts.push((name, 1)),
        }
    }
    counts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    let listed: Vec<String> = counts.iter().map(|(name, count)| format!("{name} ×{count}")).collect();
    (!listed.is_empty()).then(|| listed.join(", "))
}

fn kept_user_messages(history: &[Message]) -> Vec<String> {
    let texts = history
        .iter()
        .filter(|m| m.role == "user" && !has_tool_results(m))
        .flat_map(|m| &m.content)
        .filter(|b| !is_summary(b))
        .filter_map(|b| b["text"].as_str().filter(|_| b["type"] == "text"))
        .filter(|t| !t.is_empty() && *t != INTERRUPTED)
        .filter(|t| {
            // A compaction tells the model about the deferred tools anew.
            !["<file ", "<directory ", "<subagent-notification>", "<shell-notification>", "The files read most recently", TODO_PREFIX, TOOLS_PREFIX, super::deferred::NOTICE_START]
                .iter()
                .any(|p| t.starts_with(p))
        });
    let mut kept = Vec::new();
    let mut room = KEPT_USER_BYTES;
    for text in texts.collect::<Vec<_>>().into_iter().rev() {
        if text.len() <= room {
            room -= text.len();
            kept.push(text.to_string());
            continue;
        }
        let mut end = room;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end > 0 {
            kept.push(format!("{}\n... [cut]", &text[..end]));
        }
        break;
    }
    kept.reverse();
    kept
}

/// Removes the oldest exchange from a summarization request: up to the
/// next prompt of the user's, or else, within one long turn, the first
/// assistant response and its tool results. The last message (with the
/// summarization prompt) always stays. The model-bound blocks of the rest
/// are dropped, as their prefix changed. False if nothing can go.
fn drop_oldest(request: &mut Vec<Message>) -> bool {
    let last = request.len().saturating_sub(1);
    let next_prompt = (1..last).find(|&i| request[i].role == "user" && !has_tool_results(&request[i]));
    match next_prompt {
        Some(i) => {
            request.drain(..i);
        }
        None if request.len() >= 4 && request[1].role == "assistant" && has_tool_results(&request[2]) => {
            request.drain(1..3);
        }
        None => return false,
    }
    for m in request.iter_mut() {
        m.source = None;
    }
    true
}

/// Whether a request failed because it does not fit the context window.
fn is_context_overflow(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    ["prompt is too long", "context_length_exceeded", "context window", "maximum context length", "too many tokens"]
        .iter()
        .any(|p| text.contains(p))
}

/// Roughly how many tokens `messages` take: four bytes of text a token,
/// and a flat amount for each image or document.
fn estimate_messages(messages: &[Message]) -> u64 {
    messages.iter().flat_map(|m| &m.content).map(estimate).sum()
}

fn estimate(value: &Value) -> u64 {
    match value {
        Value::String(s) => estimate_text(s),
        Value::Array(items) => items.iter().map(estimate).sum(),
        Value::Object(o) if o.get("type").is_some_and(|t| t == "image") => 1_600,
        Value::Object(o) if o.get("type").is_some_and(|t| t == "document") => 10_000,
        Value::Object(o) => o.iter().map(|(k, v)| estimate_text(k) + estimate(v)).sum(),
        _ => 1,
    }
}

fn estimate_text(text: &str) -> u64 {
    text.len().div_ceil(4) as u64
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn text(t: &str) -> Value {
        json!({"type": "text", "text": t})
    }

    fn user(t: &str) -> Message {
        Message::user(vec![text(t)])
    }

    fn assistant(blocks: Vec<Value>) -> Message {
        Message { role: "assistant", content: blocks, source: Some("gw/m".into()) }
    }

    fn call(id: &str) -> Value {
        json!({"type": "tool_use", "id": id, "name": "bash", "input": {}})
    }

    fn results(id: &str) -> Message {
        Message::user(vec![json!({"type": "tool_result", "tool_use_id": id, "content": "ok"})])
    }

    #[test]
    fn keeps_the_users_own_messages_newest_first_within_the_budget() {
        let history = vec![
            Message::user(vec![text("<file path=\"a.rs\">\n1\tx\n</file>"), text("first")]),
            assistant(vec![call("t1")]),
            results("t1"),
            Message::user(vec![text("<subagent-notification>…</subagent-notification>"), text("second")]),
            assistant(vec![text("done")]),
        ];
        assert_eq!(kept_user_messages(&history), vec!["first", "second"]);

        let long = "x".repeat(KEPT_USER_BYTES - 10);
        let history = vec![user("oldest"), user(&"y".repeat(20)), user(&long)];
        let kept = kept_user_messages(&history);
        assert_eq!(kept.len(), 2, "the oldest does not fit");
        assert_eq!(kept[0], format!("{}\n... [cut]", "y".repeat(10)));
        assert_eq!(kept[1], long);
    }

    #[test]
    fn earlier_summaries_are_not_kept_as_user_messages() {
        let summary = format!("{SUMMARY_PREFIX}\n\n<summary>\nold\n</summary>");
        let history = vec![Message::user(vec![text("asked before"), text(&summary)]), user("asked after")];
        assert!(is_compacted(&history[0]) && !is_compacted(&history[1]));
        assert_eq!(kept_user_messages(&history), vec!["asked before", "asked after"]);
    }

    #[test]
    fn drops_the_oldest_exchange_to_fit() {
        let mut request = vec![user("one"), assistant(vec![text("a")]), user("two"), assistant(vec![text("b")]), user("sum")];
        assert!(drop_oldest(&mut request));
        assert_eq!(request.len(), 3);
        assert_eq!(request[0].content, vec![text("two")]);
        assert!(request[1].source.is_none(), "model-bound blocks no longer apply");

        // One long turn: its first round of tool calls goes.
        let mut request = vec![user("go"), assistant(vec![call("t1")]), results("t1"), assistant(vec![call("t2")]), results("t2")];
        assert!(drop_oldest(&mut request));
        assert_eq!(request.len(), 3);
        assert_eq!(request[1].content, vec![call("t2")]);

        let mut request = vec![user("go"), user("sum")];
        assert!(!drop_oldest(&mut request), "the prompt and the request itself stay");
    }

    #[test]
    fn recognizes_context_overflow() {
        assert!(is_context_overflow(&anyhow::anyhow!("HTTP 400: prompt is too long: 210000 tokens > 200000 maximum")));
        assert!(is_context_overflow(&anyhow::anyhow!("{{\"code\":\"context_length_exceeded\"}}")));
        assert!(!is_context_overflow(&anyhow::anyhow!("HTTP 529: overloaded")));
    }

    /// A gateway speaking the Anthropic streaming protocol: a summary for a
    /// summarization request, else `reply(request number)`, each reporting
    /// `input_tokens`. The request bodies go to the returned receiver.
    pub(crate) async fn gateway(
        reply: impl Fn(usize) -> (Vec<Value>, &'static str, u64) + Send + Sync + 'static,
    ) -> (crate::llm::Client, tokio::sync::mpsc::UnboundedReceiver<Value>) {
        gateway_with(json!({}), reply).await
    }

    /// [`gateway`], with `settings` added to the provider's.
    pub(crate) async fn gateway_with(
        settings: Value,
        reply: impl Fn(usize) -> (Vec<Value>, &'static str, u64) + Send + Sync + 'static,
    ) -> (crate::llm::Client, tokio::sync::mpsc::UnboundedReceiver<Value>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reply = std::sync::Arc::new(reply);
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                let last = body["messages"].as_array().and_then(|m| m.last()).cloned().unwrap_or_default();
                let summarize = last["content"].as_array().is_some_and(|c| {
                    c.iter().any(|b| b["text"].as_str().is_some_and(|t| t.starts_with("You are performing a CONTEXT")))
                });
                let (blocks, stop, input) = match summarize {
                    true => (vec![text("SUMMARY")], "end_turn", 100),
                    false => reply(count.fetch_add(1, std::sync::atomic::Ordering::SeqCst)),
                };
                let _ = tx.send(body);
                let mut events = vec![json!({"type": "message_start", "message": {"content": []}})];
                for (index, block) in blocks.into_iter().enumerate() {
                    let mut start = block.clone();
                    if block["type"] == "tool_use" {
                        start["input"] = json!({});
                    }
                    events.push(json!({"type": "content_block_start", "index": index, "content_block": start}));
                    if block["type"] == "tool_use" {
                        events.push(json!({"type": "content_block_delta", "index": index,
                            "delta": {"type": "input_json_delta", "partial_json": block["input"].to_string()}}));
                    }
                    events.push(json!({"type": "content_block_stop", "index": index}));
                }
                events.push(json!({"type": "message_delta", "delta": {"stop_reason": stop},
                    "usage": {"input_tokens": input, "output_tokens": 10}}));
                events.push(json!({"type": "message_stop"}));
                let sse: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
                async move { ([("content-type", "text/event-stream")], sse) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut provider = json!({"protocol": "anthropic", "base_url": url, "api_key": "fixture-only",
            "context_window": 10_000, "max_tokens": 1_000});
        for (key, value) in settings.as_object().into_iter().flatten() {
            provider[key] = value.clone();
        }
        let config = crate::llm::config::tests::file(json!({"model": "mock/main", "providers": {"mock": provider}}));
        let providers = crate::llm::Providers::from_parts(&config, &crate::llm::config::tests::no_env).unwrap();
        (crate::llm::Client::new(providers).unwrap(), rx)
    }

    #[derive(Default)]
    struct Seen(Vec<(u64, u64)>);

    impl Observer for Seen {
        fn on_text_delta(&mut self, _: &str) {}
        fn on_text_end(&mut self) {}
        fn on_tool_call(&mut self, _: &str, _: &Value) {}
        fn on_tool_result(&mut self, _: &str, _: &crate::tools::ToolOutput) {}
        fn on_compact_end(&mut self, before: u64, after: u64) {
            self.0.push((before, after));
        }
    }

    pub(crate) fn texts(message: &Value) -> Vec<String> {
        message["content"].as_array().unwrap().iter().filter_map(|b| b["text"].as_str().map(String::from)).collect()
    }

    #[tokio::test]
    async fn compacts_before_a_turn_that_would_not_fit() {
        let (client, mut requests) = gateway(|_| (vec![text("answer")], "end_turn", 500)).await;
        let mut agent = Agent::new(client, crate::tools::ToolRegistry::new(), "sys");
        agent.messages = vec![user("first task"), assistant(vec![text("done")])];
        agent.context = Some((2, 9_000));
        let mut seen = Seen::default();
        agent.run_turn(vec![text("next task")], &mut seen).await.unwrap();

        let summarize = requests.recv().await.unwrap();
        assert_eq!(summarize["system"], "sys", "the same system prompt");
        let sent = summarize["messages"].as_array().unwrap();
        assert_eq!(sent.len(), 3, "the history and the prompt, not the new message: {sent:?}");
        assert_eq!(texts(&sent[0]), vec!["first task"]);

        let next = requests.recv().await.unwrap();
        let sent = next["messages"].as_array().unwrap();
        assert_eq!(sent.len(), 2, "{sent:?}");
        let compacted = texts(&sent[0]);
        assert_eq!(compacted[0], "first task", "the user's own words are kept");
        assert!(compacted[1].starts_with(SUMMARY_PREFIX) && compacted[1].contains("SUMMARY"));
        assert_eq!(texts(&sent[1]), vec!["next task"], "the new message follows, whole");

        assert!(is_compacted(&agent.messages[0]));
        assert_eq!(agent.messages.len(), 3);
        assert_eq!(seen.0.len(), 1);
        assert!(seen.0[0].0 >= 9_000 && seen.0[0].1 < 1_000, "{:?}", seen.0);

        let usage = agent.usage().models();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].0, "mock/main");
        assert_eq!(usage[0].1.requests, 2, "the summary and the answer");
        assert_eq!((usage[0].1.usage.input_tokens, usage[0].1.usage.output_tokens), (600, 20));
    }

    #[tokio::test]
    async fn compacts_an_idle_conversation_before_its_cache_expires() {
        let (client, mut requests) = gateway(|_| (vec![text("done")], "end_turn", 3_000)).await;
        let mut agent = Agent::new(client, crate::tools::ToolRegistry::default(), "sys");
        let config = |min_tokens| IdleConfig { enabled: true, min_tokens };
        assert_eq!(agent.idle_after(config(1)), None, "nothing to compact");
        agent.run_turn(vec![text("go")], &mut Seen::default()).await.unwrap();
        let _ = requests.recv().await;
        assert_eq!(agent.idle_after(config(1)), Some(Duration::from_secs(255)), "before the 5-minute cache expires");
        assert_eq!(agent.idle_after(config(100_000)), None, "too short to bother");
        assert_eq!(agent.idle_after(IdleConfig { enabled: false, min_tokens: 1 }), None);
        agent.compact_when_idle(&mut Seen::default()).await.unwrap();
        assert!(is_compacted(&agent.messages[0]) && agent.messages.len() == 1);
    }

    #[tokio::test]
    async fn compacts_in_the_middle_of_a_turn_and_reattaches_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "alpha\nbeta\n").unwrap();
        let read = json!({"type": "tool_use", "id": "t1", "name": "read", "input": {"path": path}});
        let todos = json!({"type": "tool_use", "id": "t2", "name": "todo_write", "input": {"todos": [
            {"content": "Read the notes", "status": "in_progress"}, {"content": "Answer", "status": "pending"}]}});
        let (client, mut requests) = gateway(move |n| match n {
            0 => (vec![todos.clone(), read.clone()], "tool_use", 9_000),
            _ => (vec![text("finished")], "end_turn", 500),
        })
        .await;
        let files = crate::tools::FileTracker::default();
        let mut agent = Agent::new(client, crate::tools::ToolRegistry::with_defaults(files.clone()), "sys");
        let mut seen = Seen::default();
        agent.run_turn(vec![text("read the notes")], &mut seen).await.unwrap();

        let _first = requests.recv().await.unwrap();
        let summarize = requests.recv().await.unwrap();
        let last = summarize["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["content"][0]["type"], "tool_result", "the prompt goes with the tool results");
        assert!(texts(&last)[0].starts_with("You are performing"));

        let next = requests.recv().await.unwrap();
        let sent = next["messages"].as_array().unwrap();
        assert_eq!(sent.len(), 1, "the summary alone carries the turn on: {sent:?}");
        let compacted = texts(&sent[0]);
        assert_eq!(compacted[0], "read the notes");
        assert!(compacted[1].starts_with(SUMMARY_PREFIX));
        let file = compacted.iter().find(|t| t.starts_with("<file ")).expect("the file read is attached again");
        assert!(file.contains("alpha") && file.contains("beta"), "{file}");
        let todo = compacted.iter().find(|t| t.starts_with(TODO_PREFIX)).expect("the todo list goes on");
        assert!(todo.ends_with("- [ ] Read the notes (in progress)\n- [ ] Answer"), "{todo}");
        let calls = compacted.iter().find(|t| t.starts_with(TOOLS_PREFIX)).expect("the calls are counted");
        assert!(calls.ends_with(" todo_write ×1, read ×1.") || calls.ends_with(" read ×1, todo_write ×1."), "{calls}");
        assert!(!compacted[0].starts_with(TOOLS_PREFIX));

        assert_eq!(agent.messages.len(), 2);
        assert_eq!(agent.last_text(0), "finished");
        assert!(files.check_writable(&path).is_ok(), "the attached file counts as read");
    }

    #[tokio::test]
    async fn compacts_on_demand_with_a_focus() {
        let (client, mut requests) = gateway(|_| (vec![text("answer")], "end_turn", 500)).await;
        let mut agent = Agent::new(client, crate::tools::ToolRegistry::new(), "sys");
        agent.compact(Some("the API notes"), &mut Seen::default()).await.unwrap();
        assert!(agent.messages.is_empty() && requests.try_recv().is_err(), "nothing to summarize, nothing sent");

        agent.messages = vec![user("plan the API"), assistant(vec![text("planned")])];
        let mut seen = Seen::default();
        agent.compact(Some("the API notes"), &mut seen).await.unwrap();
        let sent = requests.recv().await.unwrap();
        let prompt = texts(sent["messages"].as_array().unwrap().last().unwrap()).pop().unwrap();
        assert!(prompt.ends_with("focus on: the API notes"), "{prompt}");
        assert_eq!(agent.messages.len(), 1);
        assert!(is_compacted(&agent.messages[0]));
        assert_eq!(seen.0.len(), 1);
    }

    #[tokio::test]
    async fn gives_up_when_compacting_frees_too_little() {
        let (client, _requests) = gateway(|_| (vec![text("answer")], "end_turn", 500)).await;
        let mut agent = Agent::new(client, crate::tools::ToolRegistry::new(), "sys");
        agent.messages = vec![user("first"), assistant(vec![text("done")])];
        agent.context = Some((2, 9_000));
        let huge = "word ".repeat(40_000);
        let error = agent.run_turn(vec![text(&huge)], &mut Seen::default()).await.unwrap_err();
        assert!(format!("{error:#}").contains("after compacting"), "{error:#}");
        assert!(is_compacted(&agent.messages[0]), "the summary stays");
        assert_eq!(agent.messages.len(), 1, "the message that did not fit is removed, to send again smaller");
    }

    #[test]
    fn leaves_room_for_a_full_response() {
        assert_eq!(context_limit(256_000, 64_000), 179_200);
        assert_eq!(context_limit(1_000_000, 64_000), 886_000);
        assert_eq!(context_limit(100_000, 90_000), 50_000, "half the window at least");
    }

    #[test]
    fn estimates_images_at_a_flat_size() {
        let image = json!({"type": "image", "source": {"type": "base64", "data": "A".repeat(4_000_000)}});
        assert_eq!(estimate(&image), 1_600);
        assert_eq!(estimate_text("12345678"), 2);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn compaction_hooks() {
        let _guard = crate::util::lock_cwd();
        let dir = tempfile::tempdir().unwrap();
        let (pre, post) = (dir.path().join("pre.json"), dir.path().join("post.json"));
        let (client, mut requests) = gateway(|_| (vec![text("answer")], "end_turn", 500)).await;
        let mut agent = Agent::new(client, crate::tools::ToolRegistry::new(), "sys");
        let command = |command: String| json!([{"type": "command", "command": command}]);
        agent.set_hooks(crate::hooks::tests::hooks(json!({
            "PreCompact": [
                {"matcher": "manual", "hooks": command(format!("cat > {}", pre.display()))},
                {"matcher": "auto", "hooks": command("echo 'not now' >&2; exit 2".into())}
            ],
            "PostCompact": [{"hooks": command(format!("cat > {}", post.display()))}]
        })));
        agent.messages = vec![user("plan the API"), assistant(vec![text("planned")])];
        agent.compact(Some("the API notes"), &mut Seen::default()).await.unwrap();
        requests.recv().await.unwrap();
        assert!(is_compacted(&agent.messages[0]));
        let read = |path: &std::path::Path| serde_json::from_str::<Value>(&std::fs::read_to_string(path).unwrap()).unwrap();
        let (pre, post) = (read(&pre), read(&post));
        assert_eq!((pre["trigger"].as_str(), pre["custom_instructions"].as_str()), (Some("manual"), Some("the API notes")));
        assert_eq!((post["trigger"].as_str(), post["compact_summary"].as_str()), (Some("manual"), Some("SUMMARY")));

        // Blocked: the request goes as it is.
        agent.messages = vec![user("first task"), assistant(vec![text("done")])];
        agent.context = Some((2, 9_000));
        let mut seen = Seen::default();
        agent.run_turn(vec![text("next task")], &mut seen).await.unwrap();
        let sent = requests.recv().await.unwrap();
        assert_eq!(sent["messages"].as_array().unwrap().len(), 3, "not a summary request: {sent}");
        assert!(seen.0.is_empty());
        assert_eq!(agent.messages.len(), 4);
    }
}
