//! Saved conversations in the terminal: picking one for `/resume`,
//! loading it, and saving the running one.

use anyhow::Result;
use rustyline::error::ReadlineError;

use super::{Output, dialog, output, text};
use crate::agent::Agent;
use crate::checkpoint;
use crate::session::{self, Recorder};

/// Saved conversations offered by `/resume`.
const LISTED: usize = 20;

/// Lists this directory's saved conversations and asks which one to resume;
/// `None` when there are none, or the user picked none.
pub(super) async fn pick(rec: &Recorder, out: &Output, boxed: bool) -> Result<Option<String>> {
    let sessions: Vec<session::Meta> =
        rec.store().list(&rec.meta().cwd).into_iter().filter(|m| m.id != rec.meta().id).take(LISTED).collect();
    if sessions.is_empty() {
        out.line(out.paint("2", "no saved conversations in this directory"));
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
            out.paint("1", &format!("{:>3}", i + 1)),
            out.paint("2", &format!("{when:>9}")),
            text::fit(&text::printable(title), room.max(20)),
            out.paint("2", &info),
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
        output::replay(out, agent.messages());
        let title = if rec.meta().title.is_empty() { "conversation" } else { rec.meta().title.as_str() };
        out.line(out.paint("2", &format!("resumed {} · {} · {}", text::printable(title), rec.meta().id, agent.model())));
        out.line("");
    }
    for note in notes {
        out.line(out.paint("33", &note));
    }
    Ok(())
}

/// Shows what SessionEnd hooks had for the user.
pub(super) fn show_hook_notices(out: &Output, notices: Vec<String>) {
    for notice in notices {
        out.line(out.paint("33", &format!("⚑ {}", text::printable(&notice))));
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
        out.line(out.paint("33", &format!("conversation not saved: {e:#}")));
    }
}
