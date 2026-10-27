//! `/rewind`, or Esc twice at an empty prompt: going back to before an
//! earlier prompt, with the conversation, the code or both as they were
//! then.

use anyhow::{Result, bail};
use rustyline::error::ReadlineError;

use super::dialog::{Item, Keys, choose};
use super::input::Typing;
use super::{Output, diff, text};
use super::style::{BOLD, DIM, GREEN, RED};
use crate::agent::Agent;
use crate::checkpoint::{self, Change};
use crate::session;

/// Prompts offered, the most recent first.
const LISTED: usize = 20;
/// Files listed by name among those restoring the code would change.
const LISTED_FILES: usize = 10;

/// What going back puts back.
#[derive(Clone, Copy, PartialEq)]
enum Restore {
    Both,
    Conversation,
    Code,
}

const CHOICES: [(Restore, &str); 3] = [
    (Restore::Both, "restore code and conversation"),
    (Restore::Conversation, "restore conversation"),
    (Restore::Code, "restore code"),
];

/// Lists the conversation's prompts, asks which to go back to before and
/// what to put back, and does it. A prompt taken out of the conversation
/// goes back into the input box. Returns whether anything changed.
pub(super) async fn run(agent: &mut Agent, out: &Output, typing: &mut Typing, boxed: bool) -> Result<bool> {
    let checkpoints = agent.file_history().checkpoints();
    if checkpoints.is_empty() {
        out.note("no earlier prompts to rewind to");
        return Ok(false);
    }
    let listed: Vec<_> = checkpoints.iter().rev().take(LISTED).collect();
    let now = session::now();
    let width = text::terminal_width();
    for (i, c) in listed.iter().enumerate().filter(|_| !boxed) {
        let when = session::ago(c.time, now);
        let info = match c.files.len() {
            0 => String::new(),
            n => format!(" · {n} {} changed", text::plural(n, "file")),
        };
        let room = width.saturating_sub(4 + 10 + text::width(&info) + 2);
        out.line(format!(
            "{} {}  {}{}",
            out.paint(BOLD, &format!("{:>3}", i + 1)),
            out.paint(DIM, &format!("{when:>9}")),
            text::fit(&text::printable(first_line(&c.prompt)), room.max(20)),
            out.paint(DIM, &info),
        ));
    }
    let picked = if boxed {
        let items = listed.iter().map(|c| Item::new(
            first_line(&c.prompt), match c.files.len() {
                0 => session::ago(c.time, now),
                n => format!("{} · {n} {} changed", session::ago(c.time, now), text::plural(n, "file")),
            },
        ).details(vec![c.prompt.clone()])).collect();
        let keys = Keys { enter: "continue", esc: "cancel" };
        choose(out, "Rewind conversation", "Go back to before which prompt? What to restore comes next", keys, items, 0).await.map(|i| i + 1)
    } else { ask(out, "rewind to before which prompt? (number, Enter to cancel) ", listed.len())? };
    let Some(n) = picked else {
        return Ok(false);
    };
    let checkpoint = listed[n - 1];

    let changes = agent.file_history().changes(checkpoint.message);
    if !boxed { match &changes {
        Ok(changes) if changes.is_empty() => out.note("  the code is as it was then"),
        Ok(changes) => out.lines(&change_lines(out, changes)),
        Err(e) => out.warn(format!("  the code cannot be restored: {e:#}")),
    } }
    for (i, (_, label)) in CHOICES.iter().enumerate().filter(|_| !boxed) {
        out.line(format!("{} {label}", out.paint(BOLD, &format!("{:>3}", i + 1))));
    }
    let picked = if boxed {
        restore_choice(out, &changes).await
    } else { ask(out, "restore what? (number, Enter to cancel) ", CHOICES.len())?.map(|choice| CHOICES[choice - 1].0) };
    let Some(restore) = picked else {
        return Ok(false);
    };

    let mut done = Vec::new();
    if restore != Restore::Conversation {
        let changes = changes?;
        checkpoint::restore(&changes)?;
        done.push(format!("restored {} {}", changes.len(), text::plural(changes.len(), "file")));
    }
    if restore != Restore::Code
        && let Some(prompt) = agent.rewind(checkpoint.message)
    {
        typing.draft.set_text(&prompt);
        done.push("rewound the conversation; the prompt is back in the input box".to_string());
    }
    out.note(done.join(" · "));
    Ok(true)
}

/// Asks for a number from 1 to `max`; `None` if none was given.
fn ask(out: &Output, question: &str, max: usize) -> Result<Option<usize>> {
    let mut editor = rustyline::DefaultEditor::new()?;
    let handoff = out.handoff();
    let read = tokio::task::block_in_place(|| editor.readline(&out.paint(DIM, question)));
    drop(handoff);
    let answer = match read {
        Ok(answer) => answer,
        Err(ReadlineError::Interrupted | ReadlineError::Eof) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match answer.trim() {
        "" => Ok(None),
        answer => match answer.parse::<usize>() {
            Ok(n) if (1..=max).contains(&n) => Ok(Some(n)),
            _ => bail!("pick a number from 1 to {max}"),
        },
    }
}

/// What restoring the code would do to each file.
fn change_lines(out: &Output, changes: &[Change]) -> Vec<String> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut lines = vec![format!("  the code goes back in {} {}:", changes.len(), text::plural(changes.len(), "file"))];
    for change in changes.iter().take(LISTED_FILES) {
        let path = change.path.strip_prefix(&cwd).unwrap_or(&change.path);
        lines.push(format!("    {}  {}", text::printable(&path.display().to_string()), what_changes(out, change)));
    }
    if changes.len() > LISTED_FILES {
        lines.push(out.paint(DIM, &format!("    … and {} more", changes.len() - LISTED_FILES)));
    }
    lines
}

/// Asks what to restore in a panel. The code choices are offered only when
/// there are changes to undo and they can be undone.
async fn restore_choice(out: &Output, changes: &Result<Vec<Change>>) -> Option<Restore> {
    let (subtitle, files) = match changes {
        Ok(changes) if changes.is_empty() => ("The code is as it was then, so only the conversation goes back".to_string(), None),
        Ok(changes) => {
            let files = format!("{} {} go back", changes.len(), text::plural(changes.len(), "file"));
            ("Tab lists the files that go back".to_string(), Some((files, change_lines(out, changes))))
        }
        Err(e) => (format!("The code cannot be restored: {e:#}"), None),
    };
    let mut offered = Vec::new();
    for (restore, label) in CHOICES {
        let item = match (restore, &files) {
            (Restore::Conversation, _) => Item::new(label, "Files stay as they are"),
            (Restore::Both, Some((files, lines))) => Item::new(label, format!("{files}, and the conversation")).details(lines.clone()),
            (Restore::Code, Some((files, lines))) => Item::new(label, format!("{files}; the conversation stays")).details(lines.clone()),
            (_, None) => continue,
        };
        offered.push((restore, item));
    }
    let items = offered.iter().map(|(_, item)| item.clone()).collect();
    let i = choose(out, "Restore what?", &subtitle, Keys { enter: "restore", esc: "cancel" }, items, 0).await?;
    Some(offered[i].0)
}

/// `+3 -1`, `removed` or `recreated`, for a file going back.
fn what_changes(out: &Output, change: &Change) -> String {
    let (Some(before), Some(now)) = (&change.before, &change.now) else {
        return out.paint(DIM, if change.before.is_none() { "removed" } else { "recreated" });
    };
    match (std::str::from_utf8(now), std::str::from_utf8(before)) {
        (Ok(now), Ok(before)) => {
            let (added, removed) = diff::line_counts(now, before);
            format!("{} {}", out.paint(GREEN, &format!("+{added}")), out.paint(RED, &format!("-{removed}")))
        }
        _ => out.paint(DIM, "binary"),
    }
}

fn first_line(text: &str) -> &str {
    text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}
