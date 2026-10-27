//! Saved conversations in the terminal: picking one for `/resume`,
//! loading it and showing its end again, and saving the running one.

use anyhow::Result;
use rustyline::error::ReadlineError;
use serde_json::Value;

use super::calls::{call_lines, result_block};
use super::markdown::Markdown;
use super::{Output, dialog, input, text};
use super::style::{ACCENT, BOLD, DIM};
use crate::agent::{Agent, INTERRUPTED};
use crate::checkpoint;
use crate::llm::Message;
use crate::session::{self, Recorder};

/// Saved conversations offered by `/resume`.
const LISTED: usize = 20;
/// Turns of a resumed conversation shown again.
const REPLAYED_TURNS: usize = 10;
/// Lines of a replayed prompt.
const PROMPT_LINES: usize = 4;

/// Lists this directory's saved conversations and asks which one to resume;
/// `None` when there are none, or the user picked none.
pub(super) async fn pick(rec: &Recorder, out: &Output, boxed: bool) -> Result<Option<String>> {
    let sessions: Vec<session::Meta> =
        rec.store().list(&rec.meta().cwd).into_iter().filter(|m| m.id != rec.meta().id).take(LISTED).collect();
    if sessions.is_empty() {
        out.note("no saved conversations in this directory");
        return Ok(None);
    }
    let now = session::now();
    if boxed {
        let items = sessions.iter().map(|m| dialog::Item::new(
            if m.title.is_empty() { "(untitled)" } else { &m.title },
            format!("{} · {} {} · {}", session::ago(m.updated, now), m.messages, text::plural(m.messages, "message"), m.model),
        ).details(vec![format!("Session: {}", m.id), format!("Directory: {}", m.cwd)])).collect();
        let keys = dialog::Keys { enter: "resume", esc: "cancel" };
        let selected = dialog::choose(out, "Resume conversation", "Earlier conversations in this directory", keys, items, 0).await;
        return Ok(selected.map(|i| sessions[i].id.clone()));
    }
    let width = text::terminal_width();
    for (i, m) in sessions.iter().enumerate() {
        let when = session::ago(m.updated, now);
        let info = format!(" · {} messages · {}", m.messages, m.model);
        let title = if m.title.is_empty() { "(untitled)" } else { m.title.as_str() };
        let room = width.saturating_sub(4 + 10 + text::width(&info) + 2);
        out.line(format!(
            "{} {}  {}{}",
            out.paint(BOLD, &format!("{:>3}", i + 1)),
            out.paint(DIM, &format!("{when:>9}")),
            text::fit(&text::printable(title), room.max(20)),
            out.paint(DIM, &info),
        ));
    }
    let mut editor = rustyline::DefaultEditor::new()?;
    let handoff = out.handoff();
    let read = tokio::task::block_in_place(|| editor.readline("resume which? (number, Enter to cancel) "));
    drop(handoff);
    let answer = match read {
        Ok(answer) => answer,
        Err(ReadlineError::Interrupted | ReadlineError::Eof) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(None);
    }
    match answer.parse::<usize>() {
        Ok(n) if (1..=sessions.len()).contains(&n) => Ok(Some(sessions[n - 1].id.clone())),
        Ok(_) => anyhow::bail!("pick a number from 1 to {}", sessions.len()),
        // Anything else is taken as an id.
        Err(_) => Ok(Some(answer.to_string())),
    }
}

/// Loads a saved conversation into the agent, on the model it last used
/// when that is still configured, and continues recording it. With `show`,
/// its last turns are shown again.
pub(super) async fn resume(agent: &mut Agent, rec: &mut Recorder, id: &str, out: &Output, show: bool) -> Result<()> {
    let (meta, messages) = rec.store().load(id)?;
    show_hook_notices(out, agent.end_session("resume").await);
    agent.restore(messages);
    out.restore_tool_output(agent.messages());
    let mut notes = Vec::new();
    if let Some(dir) = checkpoint::session_dir(&meta.id)
        && let Err(e) = agent.file_history().load(dir, agent.messages())
    {
        notes.push(format!("its checkpoints were not loaded: {e:#}"));
    }
    if !meta.model.is_empty() && meta.model != agent.model() {
        match agent.client().switch(&meta.model) {
            Ok(client) => agent.set_client(client),
            Err(e) => notes.push(format!("{} is not available ({e:#}); continuing on {}", meta.model, agent.model())),
        }
    }
    rec.resume(meta);
    attach(agent, rec);
    if show {
        replay(out, agent.messages());
        let title = if rec.meta().title.is_empty() { "conversation" } else { rec.meta().title.as_str() };
        out.note(format!("resumed {} · {} · {}", text::printable(title), rec.meta().id, agent.model()));
        out.line("");
    }
    for note in notes {
        out.warn(&note);
    }
    Ok(())
}

/// Shows the end of a resumed conversation the way it looked: prompts,
/// the assistant's text, and tool calls with the start of their results.
/// Reasoning and attachments are left out.
fn replay(out: &Output, messages: &[Message]) {
    // A turn starts at a user message that is not tool results.
    let starts: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "user" && !m.content.iter().any(|b| b["type"] == "tool_result"))
        .map(|(i, _)| i)
        .collect();
    let first = starts.len().saturating_sub(REPLAYED_TURNS);
    if first > 0 {
        out.note(format!("… {first} earlier turns"));
        out.line("");
    }
    let width = text::terminal_width();
    let mut calls: std::collections::HashMap<&str, &Value> = std::collections::HashMap::new();
    // Tool lines were the last shown, so text needs a blank line first.
    let mut tools = false;
    let gap = |tools: &mut bool| {
        if std::mem::take(tools) {
            out.line("");
        }
    };
    for m in &messages[starts.get(first).copied().unwrap_or(messages.len())..] {
        if m.content.iter().any(|b| b["type"] == "text" && b["text"] == INTERRUPTED) {
            gap(&mut tools);
            out.warn("■ Interrupted");
            out.line("");
        }
        if crate::agent::is_compacted(m) {
            gap(&mut tools);
            out.note("✻ Conversation compacted");
            out.line("");
            continue;
        }
        if m.role == "user" {
            let prompt = m.content.iter().rev().filter(|b| b["type"] == "text" && !crate::agent::is_timestamp(b)).filter_map(|b| b["text"].as_str()).find(|t| *t != INTERRUPTED);
            if let Some(prompt) = prompt.filter(|_| !m.content.iter().any(|b| b["type"] == "tool_result")) {
                gap(&mut tools);
                let lines: Vec<&str> = prompt.lines().collect();
                for (i, line) in lines.iter().take(PROMPT_LINES).enumerate() {
                    let lead = if i == 0 { out.paint(ACCENT, input::PROMPT) } else { "  ".to_string() };
                    out.line(format!("{lead}{}", text::fit(&text::printable(line), width.saturating_sub(3))));
                }
                if lines.len() > PROMPT_LINES {
                    out.note(format!("  … +{} lines", lines.len() - PROMPT_LINES));
                }
                out.line("");
            }
            for b in m.content.iter().filter(|b| b["type"] == "tool_result") {
                let call = b["tool_use_id"].as_str().and_then(|id| calls.remove(id));
                let name = call.and_then(|call| call["name"].as_str()).unwrap_or("");
                if let Some(call) = call { out.lines(&call_lines(out, name, &call["input"], width)); }
                let is_error = b["is_error"].as_bool().unwrap_or(false);
                let content = match &b["content"] {
                    Value::String(s) => s.clone(),
                    Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
                    _ => String::new(),
                };
                out.lines(&result_block(out, name, &content, is_error, width));
            }
            continue;
        }
        for b in &m.content {
            match b["type"].as_str() {
                Some("text") => {
                    gap(&mut tools);
                    let text = b["text"].as_str().unwrap_or("");
                    if out.styled() {
                        let mut md = Markdown::new(width);
                        let mut lines = md.push(text);
                        lines.extend(md.finish());
                        out.lines(&lines);
                    } else {
                        out.line(text);
                    }
                    out.line("");
                }
                Some("tool_use") => {
                    if let Some(id) = b["id"].as_str() { calls.insert(id, b); }
                    tools = true;
                }
                _ => {}
            }
        }
    }
    for m in messages {
        for call in m.content.iter().filter(|b| b["type"] == "tool_use") {
            if call["id"].as_str().is_some_and(|id| calls.contains_key(id)) {
                out.lines(&call_lines(out, call["name"].as_str().unwrap_or(""), &call["input"], width));
                out.warn("  ⎿ No saved result");
            }
        }
    }
}

/// Shows what SessionEnd hooks had for the user.
pub(super) fn show_hook_notices(out: &Output, notices: Vec<String>) {
    for notice in notices {
        out.warn(format!("⚑ {}", text::printable(&notice)));
    }
}

/// Tells the agent which saved conversation it is: its hooks run for it,
/// and its checkpoints are saved with it.
pub(super) fn attach(agent: &Agent, rec: &Recorder) {
    agent.hooks().set_session(&rec.meta().id, Some(rec.file()));
    agent.file_history().set_dir(checkpoint::session_dir(&rec.meta().id));
}

/// Saves the conversation and its checkpoints.
pub(super) fn save(agent: &Agent, rec: &mut Recorder, out: &Output) {
    if let Err(e) = rec.save(agent.messages(), agent.model()).and_then(|()| agent.file_history().save()) {
        out.warn(format!("conversation not saved: {e:#}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replay_pairs_each_call_with_its_result() {
        let out = Output::capture();
        let messages = vec![Message { role: "assistant", source: None, content: vec![
            json!({"type":"tool_use","id":"a","name":"bash","input":{"command":"df -h"}}),
            json!({"type":"tool_use","id":"b","name":"bash","input":{"command":"du -sh"}}),
        ]}, Message::user(vec![
            json!({"type":"tool_result","tool_use_id":"a","content":"DF result"}),
            json!({"type":"tool_result","tool_use_id":"b","content":"DU result"}),
        ])];
        // Include a user prompt so the replay knows where the turn starts.
        let mut history = vec![Message::user(vec![json!({"type":"text","text":"disk usage"})])];
        history.extend(messages);
        replay(&out, &history);
        let lines = out.captured();
        let first = lines.iter().position(|l| l.contains("df -h")).unwrap();
        assert_eq!(&lines[first..first+4], &["● bash(df -h)", "  ⎿ DF result", "● bash(du -sh)", "  ⎿ DU result"]);
    }
}
