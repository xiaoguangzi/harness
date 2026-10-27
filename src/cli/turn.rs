//! Running a turn in the terminal: its output, the input box under it while
//! it runs, and what is typed there meanwhile.

use std::sync::Arc;

use serde_json::Value;

use super::clipboard::{self, Clip, Image};
use super::commands::{self, View};
use super::input::{self, Queued, Typing};
use super::keys::{Key, KeyWatch};
use super::menu::Menu;
use super::output::{Terminal, TurnEnd};
use super::parse::parse_input;
use super::{Output, repl, sessions};
use crate::agent::{Agent, Inbox};
use crate::mentions;
use crate::session::Recorder;
use crate::setup::Env;
use crate::skills::Skill;
use crate::tools::FileTracker;

/// The input box under a running turn, and what sending from it needs.
pub(super) struct TurnInput<'a> {
    pub typing: &'a mut Typing,
    pub files: &'a FileTracker,
    /// For the commands that run beside the turn.
    pub env: &'a Env,
    pub skills: &'a [Skill],
    pub menu: Option<Arc<Menu>>,
}

/// Shows what is attached to `line`, the prompt as typed, and returns the
/// content of the turn for it, with `prompt` (what `line` expanded to), and
/// the title the conversation takes from it if it is new.
pub(super) fn prompt_content<'l>(
    agent: &mut Agent,
    line: &'l str,
    prompt: &str,
    expanded: mentions::Expanded,
    out: &Output,
) -> (Vec<Value>, Option<&'l str>) {
    let title = agent.messages().is_empty().then_some(line);
    agent.set_prompt_label(line);
    out.lines(&attachment_lines(out, &expanded));
    (expanded.into_content(prompt), title)
}

/// Runs a turn until it ends or Ctrl-C or Esc interrupts it; empty `content`
/// runs one for what waits in the agent's inbox. The turn's progress and
/// outcome are shown, and the conversation is saved after it, titled
/// `title` if it is new. With `input`, the input box stays under the
/// output meanwhile, with the command menu; if the turn is cut short, the
/// messages sent from it that the model never got go back into it.
/// Commands that [`commands::runs_during_turn`] sent from it run at once,
/// one at a time; a panel one opens outlives the turn until it is closed.
pub(super) async fn run_shown(
    agent: &mut Agent,
    content: Vec<Value>,
    title: Option<&str>,
    out: &Output,
    term: &mut Terminal,
    recorder: Option<&mut Recorder>,
    mut input: Option<TurnInput<'_>>,
) -> Result<(), TurnEnd> {
    term.begin_turn();
    let prompt = content.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
    if !prompt.is_empty() { out.transcript_text("❯ You", &prompt, false); }
    let inbox = agent.inbox().clone();
    let (tx, mut sent) = tokio::sync::mpsc::unbounded_channel();
    let (interrupt_tx, mut interrupt_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut _routed = None;
    let mut keys = match input.as_mut() {
        Some(input) => {
            out.open_input(std::mem::take(input.typing), inbox.clone());
            if let Some(menu) = &input.menu { menu.forget_files(); }
            out.set_menu(input.menu.clone());
            _routed = Some(out.route_keys());
            let (typed, menu) = (out.clone(), input.menu.clone());
            KeyWatch::for_prompt(move |key| {
                if typed.question_key(&key) || typed.panel_key(&key) || typed.tool_output_key(&key) { return; }
                if key == Key::Esc { let _ = interrupt_tx.send(()); }
                else { type_key(&typed, menu.as_deref(), key, &tx); }
            })
        }
        None => KeyWatch::start(),
    };
    let mut view = View::of(agent);
    view.running = true;
    let mut command: Option<std::pin::Pin<Box<dyn Future<Output = ()> + '_>>> = None;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    // Dropping the turn cancels it: running commands are killed, and the
    // agent repairs its history at the start of the next turn.
    let mut turn = Box::pin(agent.run_turn(content, &mut *term));
    let result = loop {
        tokio::select! {
            result = &mut turn => break result.map_err(TurnEnd::Failed),
            _ = &mut ctrl_c => break Err(TurnEnd::Interrupted),
            _ = keys.pressed() => break Err(TurnEnd::Interrupted),
            Some(()) = interrupt_rx.recv() => break Err(TurnEnd::Interrupted),
            Some((text, images)) = sent.recv() => {
                let Some(input) = &input else { continue };
                match text.starts_with('/').then(|| parse_input(&text, input.skills)) {
                    Some(parsed) if command.is_none() && commands::runs_during_turn(&parsed) => {
                        out.lines(&prompt_lines(out, &text));
                        out.edit_input(|typing| typing.sent.push(text.clone()));
                        let panel = text.split_whitespace().count() == 1;
                        let (view, out) = (view.clone(), out.clone());
                        command = Some(Box::pin(commands::during_turn(parsed, panel, view, input.env, input.skills, out)));
                    }
                    _ => send(out, &inbox, input.files, text, images),
                }
            }
            () = async { command.as_mut().unwrap().await }, if command.is_some() => command = None,
        }
    };
    drop(turn);
    if let Some(command) = command {
        command.await;
    }
    drop(keys);
    if let Some(input) = input {
        if input.menu.is_some() { out.set_menu(None); }
        *input.typing = out.close_input();
        if result.is_err() {
            input.typing.recall(&inbox);
            // What the model got before the interrupt is in its history.
            input.typing.queued.clear();
        }
    }
    term.end_turn(&result);
    if let Some(rec) = recorder {
        if let Some(title) = title {
            rec.set_title(title);
        }
        agent.settle();
        sessions::save(agent, rec, out);
    }
    result
}

/// Compacts the conversation until done or interrupted, showing it like a
/// turn, and saves the result.
pub(super) async fn compact_shown(
    agent: &mut Agent,
    focus: Option<&str>,
    out: &Output,
    term: &mut Terminal,
    recorder: Option<&mut Recorder>,
) -> Result<(), TurnEnd> {
    term.begin_turn();
    let mut keys = KeyWatch::start();
    let result = tokio::select! {
        result = agent.compact(focus, &mut *term) => result.map_err(TurnEnd::Failed),
        _ = tokio::signal::ctrl_c() => Err(TurnEnd::Interrupted),
        _ = keys.pressed() => Err(TurnEnd::Interrupted),
    };
    drop(keys);
    term.end_turn(&result);
    if let Some(rec) = recorder
        && result.is_ok()
    {
        sessions::save(agent, rec, out);
    }
    result
}

/// Runs skill `skill`, which runs in a subagent, as the user typed it with
/// `args`, until it is done or Ctrl-C or Esc interrupts it; returns the
/// message handing its report to the model.
pub(super) async fn run_fork(agent: &Agent, env: &Env, skill: &Skill, args: &str, out: &Output) -> Result<String, String> {
    out.set_status(Some(&format!("Running /{} in a subagent…", skill.name)));
    let mut keys = KeyWatch::start();
    let run = env.forks.run_for_user(skill, args, agent.subagents().clone(), agent.client().clone());
    let result = tokio::select! {
        result = run => result,
        _ = tokio::signal::ctrl_c() => Err("interrupted".into()),
        _ = keys.pressed() => Err("interrupted".into()),
    };
    drop(keys);
    out.set_status(None);
    result
}

/// The files `@`-mentioned in `line`, and the pasted `images` whose
/// placeholders are still in it.
pub(super) fn attachments(line: &str, images: &[(usize, Image)], files: &FileTracker) -> mentions::Expanded {
    let mut expanded = mentions::expand(line, files);
    for (n, image) in images.iter().filter(|(n, _)| line.contains(&mentions::image_placeholder(*n))) {
        expanded.attachments.push(mentions::pasted_image(*n, &image.media_type, &image.data));
    }
    expanded
}

/// `text` the way the conversation shows a prompt: behind `❯`, with the
/// lines after the first indented.
pub(super) fn prompt_lines(out: &Output, text: &str) -> Vec<String> {
    let lead = out.paint(input::PROMPT_SGR, input::PROMPT);
    text.lines().enumerate().map(|(i, line)| format!("{}{line}", if i == 0 { &lead } else { "  " })).collect()
}

/// Lines saying what is attached to a prompt, and what could not be.
fn attachment_lines(out: &Output, expanded: &mentions::Expanded) -> Vec<String> {
    let attached = expanded.attachments.iter().map(|a| out.paint("2", &format!("  + {}", a.label)));
    attached.chain(expanded.warnings.iter().map(|w| out.paint("33", &format!("  ! {w}")))).collect()
}

/// Types `key` into the input box, or acts on `menu` under it. Enter hands
/// what the box holds to `tx`.
fn type_key(
    out: &Output,
    menu: Option<&Menu>,
    key: Key,
    tx: &tokio::sync::mpsc::UnboundedSender<(String, Vec<(usize, Image)>)>,
) {
    if out.tool_output_key(&key) { return; }
    if let Some(menu) = menu
        && out.edit_input(|typing| repl::menu_key(menu, &mut typing.draft, &key)) == Some(true)
    {
        return;
    }
    match key {
        Key::Enter => {
            if let Some(Some(message)) = out.edit_input(|typing| typing.draft.submit()) {
                let _ = tx.send(message);
            }
        }
        Key::Up if out.edit_queued() => {}
        Key::Position(_) => {},
        Key::PasteImage => paste_clipboard(out),
        key => {
            out.edit_input(|typing| typing.draft.edit(key));
        }
    }
}

pub(super) fn paste_clipboard(out: &Output) {
    match clipboard::read() {
        Ok(Some(Clip::Image(image))) => { out.edit_input(|typing| typing.draft.paste_image(image)); }
        Ok(Some(Clip::Files(paths))) => {
            let cwd = std::env::current_dir().unwrap_or_default();
            out.edit_input(|typing| typing.draft.paste(&repl::file_mentions(&paths, &cwd)));
        }
        Ok(None) => {
            let key = if cfg!(target_os = "macos") { "Cmd+V" } else { "Ctrl+Shift+V" };
            out.input_notice(format!("no image in the clipboard (text pastes with {key})"));
        }
        Err(e) => out.input_notice(format!("! {e}")),
    }
}

/// Sends a line typed into the input box while a turn runs. A message goes
/// to the model with its next request (or makes it carry on rather than
/// end the turn); a command (`/command`, `!command`) waits for the turn to
/// end. Until then it is listed above the box.
fn send(out: &Output, inbox: &Inbox, files: &FileTracker, text: String, images: Vec<(usize, Image)>) {
    let queued = if text.starts_with(['/', '!']) || matches!(text.as_str(), "exit" | "quit") {
        Queued { text, images, id: None, shown: Vec::new() }
    } else {
        let expanded = attachments(&text, &images, files);
        let mut shown = prompt_lines(out, &text);
        shown.extend(attachment_lines(out, &expanded));
        let id = inbox.push_content(expanded.into_content(&text), true);
        Queued { text, images, id: Some(id), shown }
    };
    out.edit_input(|typing| {
        typing.sent.push(queued.text.clone());
        typing.queued.push(queued);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attaches_pasted_images_still_in_the_line() {
        let image = |n: usize| (n, Image { media_type: "image/png".into(), data: vec![n as u8] });
        let images = [image(1), image(2), image(10)];
        let attached = attachments("compare [Image #2] with [Image #10]", &images, &FileTracker::default());
        let labels: Vec<&str> = attached.attachments.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(labels, vec!["[Image #2] (image, 1 B)", "[Image #10] (image, 1 B)"]);
    }
}
