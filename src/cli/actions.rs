//! REPL commands that act outside a turn: `!command` runs a command for the
//! user, `/btw` asks a side question, `/copy` copies a reply and `/export`
//! writes the conversation to a Markdown file.

use std::path::PathBuf;

use serde_json::Value;

use super::markdown::Markdown;
use super::output::{Output, call_label};
use super::text;
use crate::agent::Agent;
use crate::llm::Message;

/// Lines of a `!command`'s output shown; the model gets all of it.
const SHOWN_LINES: usize = 40;
/// A tool result in an export is cut to this many bytes.
const EXPORTED_RESULT_BYTES: usize = 4_000;

/// Runs `command` for the user (`!command`) and shows its output, which
/// goes to the model with the next message.
pub async fn shell(agent: &Agent, command: &str, out: &Output) {
    out.set_status(Some(&format!("Running {}…", text::fit(command, 60))));
    let output = tokio::select! {
        output = crate::tools::bash::run_for_user(command) => Some(output),
        _ = tokio::signal::ctrl_c() => None,
    };
    out.set_status(None);
    let Some(output) = output else {
        out.line(out.paint("33", "■ Interrupted"));
        return;
    };
    let lines: Vec<&str> = output.content.lines().collect();
    for line in lines.iter().take(SHOWN_LINES) {
        out.line(format!("  {}", out.paint(if output.is_error { "31" } else { "2" }, &text::printable(line))));
    }
    if lines.len() > SHOWN_LINES {
        out.line(out.paint("2", &format!("  … {} more lines", lines.len() - SHOWN_LINES)));
    }
    agent.inbox().push(
        format!(
            "<user-shell-command>\nThe user ran this command themselves; its output is here for context, not \
             something to act on unless they ask.\n$ {command}\n{}\n</user-shell-command>",
            output.content
        ),
        false,
    );
    out.line(out.paint("2", "  (the output goes to the model with your next message)"));
}

/// Answers a side question (`/btw`) from the conversation so far, without
/// adding it or the answer to the conversation.
pub async fn side_question(agent: &mut Agent, question: &str, out: &Output) {
    if question.trim().is_empty() {
        out.line(out.paint("33", "usage: /btw <question>"));
        return;
    }
    out.set_status(Some("Answering on the side…"));
    let mut markdown = Markdown::new(text::terminal_width().saturating_sub(4).max(20));
    let mut started = false;
    let mut show = |text: &str| {
        let lines = markdown.push(text);
        if !lines.is_empty() && !started {
            started = true;
            out.line(out.paint("2", "  btw ›"));
        }
        out.lines(&lines.iter().map(|l| format!("    {l}")).collect::<Vec<_>>());
    };
    let answer = tokio::select! {
        answer = agent.side_question(question, &mut show) => Some(answer),
        _ = tokio::signal::ctrl_c() => None,
    };
    out.set_status(None);
    let rest: Vec<String> = markdown.finish().iter().map(|l| format!("    {l}")).collect();
    match answer {
        Some(Ok(_)) => {
            if !started {
                out.line(out.paint("2", "  btw ›"));
            }
            out.lines(&rest);
            out.line(out.paint("2", "  (not kept in the conversation)"));
        }
        Some(Err(e)) => out.line(out.paint("31", &format!("/btw failed: {e:#}"))),
        None => out.line(out.paint("33", "■ Interrupted")),
    }
}

/// Copies the `n`th latest reply of the model (from 1) to the clipboard.
pub fn copy(agent: &Agent, n: usize, out: &Output) {
    let replies: Vec<String> = agent
        .messages()
        .iter()
        .filter(|m| m.role == "assistant")
        .map(|m| m.content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<String>())
        .filter(|text| !text.trim().is_empty())
        .collect();
    let Some(reply) = replies.len().checked_sub(n).and_then(|i| replies.get(i)) else {
        out.line(out.paint("33", if replies.is_empty() { "no reply to copy yet" } else { "there is no such reply" }));
        return;
    };
    match super::clipboard::copy_text(reply.trim()) {
        Ok(how) => out.line(out.paint("2", &format!("copied the reply ({} characters) through {how}", reply.trim().chars().count()))),
        Err(e) => out.line(out.paint("31", &format!("could not copy: {e}"))),
    }
}

/// Writes the conversation as Markdown to `path`, or to
/// `harness-<session>.md` in the working directory.
pub fn export(agent: &Agent, path: Option<&str>, session: Option<&str>, out: &Output) {
    if agent.messages().is_empty() {
        out.line(out.paint("33", "nothing to export yet"));
        return;
    }
    let path = match path {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(format!("harness-{}.md", session.unwrap_or("conversation"))),
    };
    match std::fs::write(&path, markdown(agent.messages())) {
        Ok(()) => out.line(out.paint("2", &format!("exported the conversation to {}", path.display()))),
        Err(e) => out.line(out.paint("31", &format!("could not write {}: {e}", path.display()))),
    }
}

/// The conversation as Markdown: the prompts and replies, and each tool
/// call with the start of its result.
fn markdown(messages: &[Message]) -> String {
    let results: std::collections::HashMap<&str, &Value> = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|b| b["type"] == "tool_result")
        .filter_map(|b| Some((b["tool_use_id"].as_str()?, b)))
        .collect();
    let mut doc = String::from("# Conversation\n");
    for message in messages {
        if crate::agent::is_compacted(message) {
            doc.push_str("\n## Summary of the earlier conversation\n");
        }
        for block in &message.content {
            match (message.role, block["type"].as_str()) {
                ("user", Some("text")) => {
                    let text = block["text"].as_str().unwrap_or_default();
                    if text.starts_with("<file ") || text.starts_with("<directory ") {
                        doc.push_str(&format!("\n_(attached: {})_\n", text.lines().next().unwrap_or_default()));
                    } else if !text.trim().is_empty() {
                        doc.push_str(&format!("\n## You\n\n{}\n", text.trim()));
                    }
                }
                ("assistant", Some("text")) => {
                    let text = block["text"].as_str().unwrap_or_default();
                    if !text.trim().is_empty() {
                        doc.push_str(&format!("\n## Assistant\n\n{}\n", text.trim()));
                    }
                }
                ("assistant", Some("tool_use")) => {
                    let name = block["name"].as_str().unwrap_or_default();
                    doc.push_str(&format!("\n**● {}**\n", call_label(name, &block["input"], 200)));
                    if let Some(result) = block["id"].as_str().and_then(|id| results.get(id)) {
                        let text = result_text(result);
                        let shown = crate::tools::truncate(text.trim_end(), EXPORTED_RESULT_BYTES);
                        let fence = if shown.contains("```") { "````" } else { "```" };
                        doc.push_str(&format!("\n{fence}\n{shown}\n{fence}\n"));
                    }
                }
                _ => {}
            }
        }
    }
    doc
}

/// A tool result's text: its content string, or its text blocks.
fn result_text(result: &Value) -> String {
    match &result["content"] {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().filter_map(|i| i["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn exports_prompts_replies_and_calls() {
        let messages = vec![
            Message::user(vec![json!({"type": "text", "text": "<file path=\"a.rs\">\n1\tfn a()\n</file>"}), json!({"type": "text", "text": "fix it"})]),
            Message {
                role: "assistant",
                content: vec![
                    json!({"type": "thinking", "thinking": "hmm", "signature": "s"}),
                    json!({"type": "text", "text": "Running the tests."}),
                    json!({"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "cargo test"}}),
                ],
                source: None,
            },
            Message::user(vec![json!({"type": "tool_result", "tool_use_id": "t1", "content": "ok ```"})]),
        ];
        let doc = markdown(&messages);
        assert!(doc.contains("_(attached: <file path=\"a.rs\">)_"), "{doc}");
        assert!(doc.contains("## You\n\nfix it\n"), "{doc}");
        assert!(doc.contains("## Assistant\n\nRunning the tests.\n"), "{doc}");
        assert!(doc.contains("**● bash(cargo test)**\n\n````\nok ```\n````\n"), "{doc}");
        assert!(!doc.contains("hmm"), "reasoning is left out");
    }
}
