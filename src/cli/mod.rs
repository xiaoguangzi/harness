//! Terminal frontend: the interactive REPL and one-shot mode.

mod clipboard;
mod diff;
mod input;
mod keys;
mod markdown;
mod menu;
mod output;
mod repl;
mod text;

use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use rustyline::error::ReadlineError;
use rustyline::{Cmd, KeyCode, KeyEvent, Modifiers};
use serde_json::Value;

use crate::agent::{Agent, Inbox};
use crate::llm;
use crate::mcp;
use crate::mentions;
use crate::session::{self, Recorder, Resume, Store};
use crate::skills::Skill;
use crate::subagent::AgentDef;
use crate::tools::FileTracker;
use clipboard::{Clip, Image};
use input::{Queued, Typing};
use keys::{Key, KeyWatch};
pub use output::Output;
use output::{Terminal, TurnEnd};
use menu::Command;
use repl::{ReplHelper, Wake};

/// Built-in commands and what they do, as the menu and `/help` show them.
const BUILTINS: [(&str, &str); 14] = [
    ("/help", "show commands and keys"),
    ("/model", "show the model, or switch with /model <provider>/<model>"),
    ("/effort", "show the reasoning effort, or set it with /effort <level>"),
    ("/clear", "start a new conversation"),
    ("/compact", "summarize the conversation to free context; /compact <focus> says what to keep"),
    ("/resume", "pick up an earlier conversation from this directory"),
    ("/tools", "list the tools the model can call"),
    ("/skills", "list available skills"),
    ("/agents", "list subagent types and their tools"),
    ("/tasks", "list the subagents and background commands this conversation started"),
    ("/stop", "stop running subagents, or one with /stop <id>"),
    ("/mcp", "list MCP servers and their tools"),
    ("/sandbox", "show what the sandbox allows commands to do"),
    ("/exit", "quit (or exit, Ctrl-D)"),
];

/// Saved conversations offered by `/resume`.
const LISTED_SESSIONS: usize = 20;

pub async fn run(
    agent: &mut Agent,
    args: Vec<String>,
    skills: &[Skill],
    agent_defs: &[AgentDef],
    mcp: &mcp::Manager,
    files: &FileTracker,
    out: Output,
) -> Result<ExitCode> {
    let (resume, args) = session::parse_args(args)?;
    out.spawn_spinner();
    let mut term = Terminal::new(out.clone());
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let mut recorder = Store::default_location().map(|store| Recorder::new(store, cwd));
    let one_shot = !args.is_empty();

    if !one_shot {
        print_banner(agent, skills, mcp, &out);
    }
    if let Some(resume) = resume {
        let Some(rec) = recorder.as_mut() else { anyhow::bail!("no home directory to find saved sessions in") };
        let id = match resume {
            Resume::Id(id) => Some(id),
            Resume::Latest => match rec.store().list(&rec.meta().cwd).first() {
                Some(latest) => Some(latest.id.clone()),
                None => anyhow::bail!("no saved conversation in this directory"),
            },
            Resume::Pick if one_shot => anyhow::bail!("--resume needs a session id in one-shot mode"),
            Resume::Pick => pick_session(rec, &out)?,
        };
        if let Some(id) = id {
            resume_session(agent, rec, &id, &out, !one_shot)?;
        }
    }

    // One-shot mode: `harness "do something"` or `harness /skill args`.
    if one_shot {
        let line = args.join(" ");
        return match parse_input(&line, skills) {
            Input::Prompt(prompt) => {
                // The turn's outcome is already shown.
                let attached = attachments(&line, &[], files);
                let mut result = submit(agent, &line, &prompt, attached, &out, &mut term, recorder.as_mut(), None).await;
                // Answer background subagents' reports as they come in, and
                // exit once none is left running.
                while result.is_ok() {
                    if !agent.inbox().wants_turn() {
                        let running = agent.subagents().running_in_background();
                        if running == 0 {
                            break;
                        }
                        if !wait_for_reports(agent, running, &out).await {
                            result = Err(TurnEnd::Interrupted);
                            break;
                        }
                    }
                    result = run_shown(agent, Vec::new(), None, &out, &mut term, recorder.as_mut(), None).await;
                }
                Ok(match result {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(TurnEnd::Interrupted) => ExitCode::from(130),
                    Err(TurnEnd::Failed(_)) => ExitCode::FAILURE,
                })
            }
            Input::Error(e) => Err(anyhow::anyhow!(e)),
            _ => Err(anyhow::anyhow!("REPL commands are only available interactively")),
        };
    }

    let mut commands: Vec<Command> = BUILTINS.iter().map(|(name, description)| Command::new(*name, *description)).collect();
    commands.extend(skills.iter().map(|s| Command::new(format!("/{}", s.name), &s.description)));
    commands.extend(
        agent.client().providers().listed_models().iter().map(|m| Command::new(format!("/model {m}"), "switch to this model")),
    );
    commands.extend(llm::Effort::ALL.iter().map(|e| Command::new(format!("/effort {}", e.as_str()), effort_help(*e))));
    commands.push(Command::new("/effort auto", "back to the configured effort"));
    let config = rustyline::Config::builder()
        .completion_type(rustyline::CompletionType::List)
        .auto_add_history(true)
        .build();
    let mut editor: rustyline::Editor<ReplHelper, rustyline::history::DefaultHistory> =
        rustyline::Editor::with_config(config)?;
    // A background subagent's report may come in while the prompt waits.
    // The editor handles SIGINT only when both ends are a terminal.
    let wake = (std::io::stdin().is_terminal() && out.styled()).then(|| {
        let wake = Arc::new(Wake::new(agent.inbox()));
        let poke = wake.clone();
        agent.inbox().on_wake(move || poke.poke());
        wake
    });
    // A wake that lands just after the editor returns meets Tokio's SIGINT
    // handler, installed from here on, instead of ending the process.
    let _sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();
    // On a terminal, the prompt is drawn as an input box, which stays under
    // the output while a turn runs; what is typed there carries over to the
    // prompt after it.
    let boxed = std::io::stdin().is_terminal() && out.styled();
    let helper = ReplHelper::new(commands, boxed, wake.clone());
    helper.bind_keys(&mut editor);
    editor.set_helper(Some(helper));
    // Alt+Enter inserts a newline, like a trailing `\`.
    editor.bind_sequence(KeyEvent(KeyCode::Enter, Modifiers::ALT), Cmd::Newline);
    let history = history_path();
    if let Some(path) = &history {
        let _ = editor.load_history(path);
    }

    let mut typing = Typing::default();
    let mut interrupted = false;
    loop {
        // A background subagent reported back, or a message sent from the
        // input box is still on its way: the model answers first.
        if agent.inbox().wants_turn() {
            let input = boxed.then_some(TurnInput { typing: &mut typing, files });
            let _ = run_shown(agent, Vec::new(), None, &out, &mut term, recorder.as_mut(), input).await;
            remember_sent(&mut editor, &mut typing);
            out.line("");
            continue;
        }
        let (line, images) = match typing.next_command() {
            // Sent from the input box while a turn ran.
            Some(command) => {
                out.lines(&prompt_lines(&out, &command));
                (command, Vec::new())
            }
            None => {
                let (before, after, carried) = typing.draft.take();
                let prompt = match editor.helper() {
                    Some(helper) => {
                        let last = mentions::last_pasted_image(agent.messages()).max(typing.draft.last_image());
                        helper.new_line(last, carried);
                        helper.prompt()
                    }
                    None => input::PROMPT.to_string(),
                };
                // Reading a line blocks; let the runtime move other tasks
                // (such as MCP connections) off this worker meanwhile.
                out.hold(true);
                let read = tokio::task::block_in_place(|| editor.readline_with_initial(&prompt, (&before, &after)));
                let woken =
                    wake.as_ref().is_some_and(|w| w.finish_reading()) && matches!(read, Err(ReadlineError::Interrupted));
                // Take the input box off the screen; a line sent shows as
                // a plain prompt instead.
                if boxed && let Some(helper) = editor.helper() {
                    let shown = match &read {
                        Ok(line) => Some(line.as_str()),
                        Err(ReadlineError::Eof) => Some(""),
                        Err(_) => None,
                    };
                    print!("\x1b[{}A\r\x1b[J", helper.rows_used(&prompt, shown));
                    let _ = std::io::stdout().flush();
                }
                // What subagents printed meanwhile.
                out.hold(false);
                let line = match read {
                    Ok(line) => line,
                    // Interrupted for a subagent's report.
                    Err(ReadlineError::Interrupted) if woken => {
                        interrupted = false;
                        continue;
                    }
                    Err(ReadlineError::Interrupted) if !interrupted => {
                        interrupted = true;
                        let running = agent.subagents().running_in_background();
                        let note = match running {
                            0 => String::new(),
                            1 => "; a subagent is still running".to_string(),
                            n => format!("; {n} subagents are still running"),
                        };
                        out.line(out.paint("2", &format!("(press Ctrl-C again or Ctrl-D to exit{note})")));
                        continue;
                    }
                    Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
                    Err(e) => return Err(e.into()),
                };
                interrupted = false;
                let Some(helper) = editor.helper() else { continue };
                typing.draft.note_image(helper.last_image());
                let mut line = repl::join_continuations(line.trim());
                // Enter ran the command selected in the menu, rather than
                // the prefix typed.
                if let Some(command) = helper.take_accepted() {
                    line = command;
                }
                if boxed && !line.is_empty() {
                    out.lines(&prompt_lines(&out, &line));
                }
                (line, helper.take_images())
            }
        };
        match parse_input(&line, skills) {
            Input::Empty => {}
            Input::Exit => break,
            Input::Help => print_help(skills),
            Input::Clear => {
                agent.clear();
                if let Some(rec) = recorder.as_mut() {
                    rec.restart();
                }
                out.line(out.paint("2", "conversation cleared"));
            }
            Input::Compact(focus) => {
                if agent.messages().is_empty() {
                    out.line(out.paint("2", "nothing to compact yet"));
                    continue;
                }
                let _ = compact_shown(agent, focus.as_deref(), &out, &mut term, recorder.as_mut()).await;
                out.line("");
            }
            Input::Resume(id) => {
                let Some(rec) = recorder.as_mut() else {
                    out.line(out.paint("31", "no home directory to find saved sessions in"));
                    continue;
                };
                let id = match id {
                    Some(id) => Ok(Some(id)),
                    None => pick_session(rec, &out),
                };
                let resumed = id.and_then(|id| match id {
                    Some(id) => resume_session(agent, rec, &id, &out, true),
                    None => Ok(()),
                });
                if let Err(e) = resumed {
                    out.line(out.paint("31", &format!("{e:#}")));
                }
            }
            Input::ListSkills => print_skills(skills),
            Input::ListTools => print_tools(agent),
            Input::ListMcp => print_mcp(mcp),
            Input::ListAgents => print_agents(agent_defs),
            Input::Tasks => print_tasks(&agent.subagents().list(), &agent.shells().list()),
            Input::Stop(None) => {
                let stopped = agent.subagents().stop_all();
                let killed = agent.shells().stop_all();
                let plural = |n: usize, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
                let note = format!("stopped {} and {}", plural(stopped, "subagent"), plural(killed, "background command"));
                out.line(out.paint("2", &note));
            }
            Input::Stop(Some(id)) if id.starts_with("shell_") => match agent.shells().stop(&id) {
                Ok(true) => out.line(out.paint("2", &format!("stopped {id}"))),
                Ok(false) => out.line(out.paint("2", &format!("{id} is not running"))),
                Err(e) => out.line(out.paint("31", &e)),
            },
            Input::Stop(Some(id)) => match agent.subagents().stop(&id, true) {
                Ok(true) => out.line(out.paint("2", &format!("stopped {id}"))),
                Ok(false) => out.line(out.paint("2", &format!("{id} is not running"))),
                Err(e) => out.line(out.paint("31", &format!("{e:#}"))),
            },
            Input::Sandbox => print_sandbox(),
            Input::Model(None) => print_models(agent),
            Input::Model(Some(spec)) => match agent.client().switch(&spec) {
                Ok(client) => {
                    out.line(format!("model: {}", out.paint("1", client.spec())));
                    agent.set_client(client);
                }
                Err(e) => out.line(out.paint("31", &format!("{e:#}"))),
            },
            Input::Effort(None) => {
                out.line(format!("effort: {}", out.paint("1", agent.client().effort().as_str())));
                out.line(out.paint("2", "set with /effort low|medium|high|xhigh|max, or /effort auto"));
            }
            Input::Effort(Some(level)) => {
                let mut client = agent.client().clone();
                client.set_effort(level);
                out.line(format!("effort: {}", out.paint("1", client.effort().as_str())));
                agent.set_client(client);
            }
            Input::Error(e) => out.line(out.paint("31", &e)),
            Input::Prompt(prompt) => {
                // The turn's outcome is already shown.
                let attached = attachments(&line, &images, files);
                let input = boxed.then_some(TurnInput { typing: &mut typing, files });
                let _ = submit(agent, &line, &prompt, attached, &out, &mut term, recorder.as_mut(), input).await;
                remember_sent(&mut editor, &mut typing);
                out.line("");
            }
        }
    }
    if let Some(path) = &history {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = editor.save_history(path);
    }
    Ok(ExitCode::SUCCESS)
}

fn history_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".harness/history"))
}

/// Lists this directory's saved conversations and asks which one to resume;
/// `None` when there are none, or the user picked none.
fn pick_session(rec: &Recorder, out: &Output) -> Result<Option<String>> {
    let sessions: Vec<session::Meta> =
        rec.store().list(&rec.meta().cwd).into_iter().filter(|m| m.id != rec.meta().id).take(LISTED_SESSIONS).collect();
    if sessions.is_empty() {
        out.line(out.paint("2", "no saved conversations in this directory"));
        return Ok(None);
    }
    let now = session::now();
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
    let answer = match tokio::task::block_in_place(|| editor.readline("resume which? (number, Enter to cancel) ")) {
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
fn resume_session(agent: &mut Agent, rec: &mut Recorder, id: &str, out: &Output, show: bool) -> Result<()> {
    let (meta, messages) = rec.store().load(id)?;
    agent.restore(messages);
    let mut notes = Vec::new();
    if !meta.model.is_empty() && meta.model != agent.model() {
        match agent.client().switch(&meta.model) {
            Ok(client) => agent.set_client(client),
            Err(e) => notes.push(format!("{} is not available ({e:#}); continuing on {}", meta.model, agent.model())),
        }
    }
    rec.resume(meta);
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

/// The files `@`-mentioned in `line`, and the pasted `images` whose
/// placeholders are still in it.
fn attachments(line: &str, images: &[(usize, Image)], files: &FileTracker) -> mentions::Expanded {
    let mut expanded = mentions::expand(line, files);
    for (n, image) in images.iter().filter(|(n, _)| line.contains(&mentions::image_placeholder(*n))) {
        expanded.attachments.push(mentions::pasted_image(*n, &image.media_type, &image.data));
    }
    expanded
}

/// `text` the way the conversation shows a prompt: behind `❯`, with the
/// lines after the first indented.
fn prompt_lines(out: &Output, text: &str) -> Vec<String> {
    let lead = out.paint(input::PROMPT_SGR, input::PROMPT);
    text.lines().enumerate().map(|(i, line)| format!("{}{line}", if i == 0 { &lead } else { "  " })).collect()
}

/// Lines saying what is attached to a prompt, and what could not be.
fn attachment_lines(out: &Output, expanded: &mentions::Expanded) -> Vec<String> {
    let attached = expanded.attachments.iter().map(|a| out.paint("2", &format!("  + {}", a.label)));
    attached.chain(expanded.warnings.iter().map(|w| out.paint("33", &format!("  ! {w}")))).collect()
}

/// Shows what is attached to `line` and runs the turn.
#[allow(clippy::too_many_arguments)]
async fn submit(
    agent: &mut Agent,
    line: &str,
    prompt: &str,
    expanded: mentions::Expanded,
    out: &Output,
    term: &mut Terminal,
    recorder: Option<&mut Recorder>,
    input: Option<TurnInput<'_>>,
) -> Result<(), TurnEnd> {
    let first = agent.messages().is_empty();
    out.lines(&attachment_lines(out, &expanded));
    run_shown(agent, expanded.into_content(prompt), first.then_some(line), out, term, recorder, input).await
}

/// The input box under a running turn, and what sending from it needs.
struct TurnInput<'a> {
    typing: &'a mut Typing,
    files: &'a FileTracker,
}

/// Runs a turn until it ends or Ctrl-C or Esc interrupts it; empty `content`
/// runs one for what waits in the agent's inbox. The turn's progress and
/// outcome are shown, and the conversation is saved after it, titled
/// `title` if it is new. With `input`, the input box stays under the
/// output meanwhile; if the turn is cut short, the messages sent from it
/// that the model never got go back into it.
async fn run_shown(
    agent: &mut Agent,
    content: Vec<Value>,
    title: Option<&str>,
    out: &Output,
    term: &mut Terminal,
    recorder: Option<&mut Recorder>,
    mut input: Option<TurnInput<'_>>,
) -> Result<(), TurnEnd> {
    term.begin_turn();
    let inbox = agent.inbox().clone();
    let (tx, mut sent) = tokio::sync::mpsc::unbounded_channel();
    let mut keys = match input.as_mut() {
        Some(input) => {
            out.open_input(std::mem::take(input.typing), inbox.clone());
            let out = out.clone();
            KeyWatch::with_keys(move |key| type_key(&out, key, &tx))
        }
        None => KeyWatch::start(),
    };
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
            Some((text, images)) = sent.recv() => {
                if let Some(input) = &input {
                    send(out, &inbox, input.files, text, images);
                }
            }
        }
    };
    drop(turn);
    drop(keys);
    if let Some(input) = input {
        *input.typing = out.close_input();
        if result.is_err() {
            input.typing.recall(&inbox);
        }
    }
    term.end_turn(&result);
    if let Some(rec) = recorder {
        if let Some(title) = title {
            rec.set_title(title);
        }
        agent.settle();
        if let Err(e) = rec.save(agent.messages(), agent.model()) {
            out.line(out.paint("33", &format!("conversation not saved: {e:#}")));
        }
    }
    result
}

/// Compacts the conversation until done or interrupted, showing it like a
/// turn, and saves the result.
async fn compact_shown(
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
        && let Err(e) = rec.save(agent.messages(), agent.model())
    {
        out.line(out.paint("33", &format!("conversation not saved: {e:#}")));
    }
    result
}

/// Types `key` into the input box. Enter hands what it holds to `tx`.
fn type_key(out: &Output, key: Key, tx: &tokio::sync::mpsc::UnboundedSender<(String, Vec<(usize, Image)>)>) {
    match key {
        Key::Enter => {
            if let Some(Some(message)) = out.edit_input(|typing| typing.draft.submit()) {
                let _ = tx.send(message);
            }
        }
        Key::PasteImage => match clipboard::read() {
            Ok(Some(Clip::Image(image))) => {
                out.edit_input(|typing| typing.draft.paste_image(image));
            }
            Ok(Some(Clip::Files(paths))) => {
                let cwd = std::env::current_dir().unwrap_or_default();
                out.edit_input(|typing| typing.draft.paste(&repl::file_mentions(&paths, &cwd)));
            }
            Ok(None) | Err(_) => {}
        },
        key => {
            out.edit_input(|typing| typing.draft.edit(key));
        }
    }
}

/// Sends a line typed into the input box while a turn runs. A message goes
/// to the model with its next request (or makes it carry on rather than
/// end the turn); a command waits for the turn to end. Until then it is
/// listed above the box.
fn send(out: &Output, inbox: &Inbox, files: &FileTracker, text: String, images: Vec<(usize, Image)>) {
    let queued = if text.starts_with('/') || matches!(text.as_str(), "exit" | "quit") {
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

/// Adds the lines sent from the input box to the line editor's history.
fn remember_sent(editor: &mut rustyline::Editor<ReplHelper, rustyline::history::DefaultHistory>, typing: &mut Typing) {
    for line in typing.sent.drain(..) {
        let _ = editor.add_history_entry(line);
    }
}

/// Shows that `running` background subagents are still at work until one
/// reports back; false if the user interrupted the wait.
async fn wait_for_reports(agent: &Agent, running: usize, out: &Output) -> bool {
    let plural = if running == 1 { "" } else { "s" };
    out.set_status(Some(&format!("Waiting for {running} background subagent{plural}…")));
    let mut esc = KeyWatch::start();
    let reported = tokio::select! {
        _ = agent.inbox().wanted() => true,
        _ = tokio::signal::ctrl_c() => false,
        _ = esc.pressed() => false,
    };
    drop(esc);
    out.set_status(None);
    reported
}

#[derive(Debug, PartialEq)]
enum Input {
    Empty,
    Exit,
    Help,
    Clear,
    /// `/compact [focus]` summarizes the conversation.
    Compact(Option<String>),
    ListSkills,
    ListTools,
    ListMcp,
    ListAgents,
    /// `/tasks` lists this conversation's subagents.
    Tasks,
    /// `/stop` stops every running subagent; `/stop <id>` that one.
    Stop(Option<String>),
    Sandbox,
    /// `/model` shows the current model; `/model <spec>` switches.
    Model(Option<String>),
    /// `/resume` lists saved conversations to pick from; `/resume <id>`
    /// resumes that one.
    Resume(Option<String>),
    /// `/effort` shows the effort; `/effort <level>` sets it for this
    /// session, and `/effort auto` restores the configured one.
    Effort(Option<Option<llm::Effort>>),
    Prompt(String),
    Error(String),
}

/// Interprets a line of user input. `/<skill> args` loads the skill's
/// instructions directly into the prompt, saving the model a tool call.
fn parse_input(line: &str, skills: &[Skill]) -> Input {
    let Some(command) = line.strip_prefix('/') else {
        return match line {
            "" => Input::Empty,
            // A bare `exit` or `quit` alone on the line quits too.
            "exit" | "quit" => Input::Exit,
            _ => Input::Prompt(line.to_string()),
        };
    };
    let (name, args) = command.split_once(char::is_whitespace).unwrap_or((command, ""));
    match name {
        "exit" | "quit" => Input::Exit,
        "help" => Input::Help,
        "clear" => Input::Clear,
        "compact" => Input::Compact(Some(args.trim()).filter(|a| !a.is_empty()).map(String::from)),
        "resume" => Input::Resume(Some(args.trim()).filter(|a| !a.is_empty()).map(String::from)),
        "skills" => Input::ListSkills,
        "tools" => Input::ListTools,
        "mcp" => Input::ListMcp,
        "agents" => Input::ListAgents,
        "tasks" => Input::Tasks,
        "stop" => Input::Stop(Some(args.trim()).filter(|a| !a.is_empty()).map(String::from)),
        "sandbox" => Input::Sandbox,
        "model" => Input::Model(Some(args.trim()).filter(|a| !a.is_empty()).map(String::from)),
        "effort" => match args.trim() {
            "" => Input::Effort(None),
            "auto" | "default" => Input::Effort(Some(None)),
            level => match llm::Effort::parse(level) {
                Ok(e) => Input::Effort(Some(Some(e))),
                Err(e) => Input::Error(e.to_string()),
            },
        },
        _ => match skills.iter().find(|s| s.name == name) {
            Some(skill) => match skill.invocation_prompt(args) {
                Ok(prompt) => Input::Prompt(prompt),
                Err(e) => Input::Error(e),
            },
            None => Input::Error(format!("unknown command /{name} (try /help)")),
        },
    }
}

fn print_banner(agent: &Agent, skills: &[Skill], mcp: &mcp::Manager, out: &Output) {
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    let cwd = match cwd.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => cwd,
    };
    let servers = mcp.statuses.iter().filter(|s| s.outcome.is_ok()).count();
    let effort = out.paint("2", &format!(" · {} effort", agent.client().effort().as_str()));
    out.line(format!("{} {}{effort}", out.paint("1;35", "✻ harness"), out.paint("1", agent.model())));
    out.line(out.paint(
        "2",
        &format!(
            "  {cwd} · {} tools · {} skills · {servers} MCP servers{}",
            agent.tools().names().count(),
            skills.len(),
            if crate::sandbox::active().is_some() { " · sandboxed" } else { "" }
        ),
    ));
    out.line(out.paint("2", "  / for commands · @ for files · Ctrl+V to paste an image · Esc or Ctrl-C to interrupt"));
    out.line("");
}

fn effort_help(effort: llm::Effort) -> &'static str {
    match effort {
        llm::Effort::Low => "fastest and cheapest; simple tasks",
        llm::Effort::Medium => "balanced speed and depth",
        llm::Effort::High => "deeper thinking",
        llm::Effort::XHigh => "deeper still; suits most coding work",
        llm::Effort::Max => "deepest; when correctness matters most",
    }
}

fn print_help(skills: &[Skill]) {
    let keys = [
        ("/<skill> [args]", "run a skill"),
        ("@path", "attach a file, directory, image or PDF; @ opens a file menu (@file#L10-20 for lines)"),
        ("Ctrl+V", "paste an image from the clipboard (files copied in Finder or a file manager become @ mentions)"),
        ("\\ or Alt+Enter", "continue on a new line"),
        ("↑ ↓ Tab Enter", "pick from the menu shown while typing / or @"),
        ("Esc", "interrupt the running turn"),
        ("Ctrl-C", "interrupt the running turn; twice at the prompt to quit"),
    ];
    for (key, what) in BUILTINS.iter().chain(&keys) {
        println!("  \x1b[1m{key:<16}\x1b[0m {what}");
    }
    if !skills.is_empty() {
        let names: Vec<String> = skills.iter().map(|s| format!("/{}", s.name)).collect();
        println!("  \x1b[2mskills: {}\x1b[0m", names.join(" "));
    }
}

fn print_tools(agent: &Agent) {
    let tools = agent.tools().definitions();
    let name_width = tools.iter().map(|t| t.name.len()).max().unwrap_or(0);
    let room = text::terminal_width().saturating_sub(name_width + 5);
    for tool in tools {
        let summary = tool.description.split(". ").next().unwrap_or("").trim_end_matches('.');
        println!("  \x1b[1m{:<name_width$}\x1b[0m  \x1b[2m{}\x1b[0m", tool.name, text::fit(summary, room));
    }
}

fn print_sandbox() {
    let Some(sandbox) = crate::sandbox::active() else {
        println!("the sandbox is off; start with --sandbox, HARNESS_SANDBOX=1 or \"sandbox\": {{\"enabled\": true}} in ~/.harness/settings.json");
        return;
    };
    for (label, value) in sandbox.describe() {
        println!("  \x1b[1m{label:<10}\x1b[0m {value}");
    }
}

fn print_skills(skills: &[Skill]) {
    if skills.is_empty() {
        println!("no skills found (looked in .harness/skills, .claude/skills and the same under ~)");
    }
    for s in skills {
        println!("\x1b[1m{}\x1b[0m  {}\n  \x1b[90m{}\x1b[0m", s.name, s.description, s.dir.display());
    }
}

fn print_models(agent: &Agent) {
    let providers = agent.client().providers();
    println!("current: \x1b[1m{}\x1b[0m", agent.model());
    if let Some(sub) = &providers.subagent_model {
        println!("subagents: {sub}");
    }
    for name in providers.names() {
        let protocol = providers.protocol_of(name).map_or("?", llm::Protocol::as_str);
        println!("provider \x1b[1m{name}\x1b[0m ({protocol})");
    }
    for m in providers.listed_models() {
        let mark = if m == agent.model() { "\x1b[32m●\x1b[0m" } else { " " };
        println!(" {mark} {m}");
    }
    println!("\x1b[90mswitch with /model <provider>/<model>; any model name the gateway accepts works\x1b[0m");
}

fn print_agents(defs: &[AgentDef]) {
    for d in defs {
        let access = d.permission.as_str();
        let tools = d.tools.as_ref().map_or("all permitted".to_string(), |t| t.join(", "));
        let source = d.path.as_ref().map_or("built-in".to_string(), |p| p.display().to_string());
        println!("\x1b[1m{}\x1b[0m  [{access}; tools: {tools}]", d.name);
        println!("  {}", d.description);
        println!("  \x1b[90m{source}{}\x1b[0m", d.model.as_ref().map_or(String::new(), |m| format!(" · model {m}")));
    }
}

fn print_tasks(subagents: &[Value], shells: &[(String, String, bool)]) {
    if subagents.is_empty() && shells.is_empty() {
        println!("no subagents or background commands in this conversation");
        return;
    }
    for s in subagents {
        let status = s["status"].as_str().unwrap_or("");
        let color = match status {
            "running" => "36",
            "completed" => "32",
            "failed" => "31",
            _ => "33",
        };
        let mut info = vec![
            s["model"].as_str().unwrap_or("").to_string(),
            format!("{} tool calls", s["tool_calls"]),
            output::elapsed(s["seconds"].as_u64().unwrap_or(0)),
        ];
        if s["background"] == true {
            info.push("background".into());
        }
        println!(
            "  \x1b[1m{}\x1b[0m  \x1b[{color}m{status:<9}\x1b[0m  {}  \x1b[2m{}\x1b[0m",
            s["id"].as_str().unwrap_or(""),
            text::printable(s["label"].as_str().unwrap_or("")),
            info.join(" · ")
        );
    }
    for (id, command, running) in shells {
        let (color, status) = if *running { ("36", "running") } else { ("2", "ended") };
        let command = text::fit(&text::printable(command.lines().next().unwrap_or("")), 80);
        println!("  \x1b[1m{id}\x1b[0m  \x1b[{color}m{status:<9}\x1b[0m  \x1b[2m$ {command}\x1b[0m");
    }
    println!("\x1b[90mstop one with /stop <id>, or every running one with /stop\x1b[0m");
}

fn print_mcp(mcp: &mcp::Manager) {
    if mcp.statuses.is_empty() {
        println!("no MCP servers configured (`mcpServers` in ~/.harness/settings.json)");
    }
    for status in &mcp.statuses {
        match &status.outcome {
            Ok(tools) => {
                println!("\x1b[32m●\x1b[0m {} ({} tools)", status.name, tools.len());
                for t in tools {
                    println!("    {t}");
                }
            }
            Err(e) => println!("\x1b[31m●\x1b[0m {} failed: {e}", status.name),
        }
        println!("    \x1b[90mlog: {}\x1b[0m", mcp::log_path(&status.name).display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills;

    #[test]
    fn attaches_pasted_images_still_in_the_line() {
        let image = |n: usize| (n, Image { media_type: "image/png".into(), data: vec![n as u8] });
        let images = [image(1), image(2), image(10)];
        let attached = attachments("compare [Image #2] with [Image #10]", &images, &FileTracker::default());
        let labels: Vec<&str> = attached.attachments.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(labels, vec!["[Image #2] (image, 1 B)", "[Image #10] (image, 1 B)"]);
    }

    #[test]
    fn parses_commands() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("greet")).unwrap();
        std::fs::write(dir.path().join("greet/SKILL.md"), "---\ndescription: d\n---\nSay hi.\n").unwrap();
        let (skills, _) = skills::discover(&[dir.path().into()]);

        assert_eq!(parse_input("", &skills), Input::Empty);
        assert_eq!(parse_input("/exit", &skills), Input::Exit);
        assert_eq!(parse_input("exit", &skills), Input::Exit);
        assert_eq!(parse_input("quit", &skills), Input::Exit);
        assert_eq!(parse_input("exit now", &skills), Input::Prompt("exit now".into()));
        assert_eq!(parse_input("how do I exit vim", &skills), Input::Prompt("how do I exit vim".into()));
        assert_eq!(parse_input("/clear", &skills), Input::Clear);
        assert_eq!(parse_input("/compact", &skills), Input::Compact(None));
        assert_eq!(parse_input("/compact keep the API notes", &skills), Input::Compact(Some("keep the API notes".into())));
        assert_eq!(parse_input("/tools", &skills), Input::ListTools);
        assert_eq!(parse_input("/tasks", &skills), Input::Tasks);
        assert_eq!(parse_input("/stop", &skills), Input::Stop(None));
        assert_eq!(parse_input("/stop a3f9c2", &skills), Input::Stop(Some("a3f9c2".into())));
        assert_eq!(parse_input("/resume", &skills), Input::Resume(None));
        assert_eq!(parse_input("/resume 2026 ", &skills), Input::Resume(Some("2026".into())));
        assert_eq!(parse_input("fix it", &skills), Input::Prompt("fix it".into()));
        assert!(matches!(parse_input("/nope", &skills), Input::Error(_)));
        assert_eq!(parse_input("/effort", &skills), Input::Effort(None));
        assert_eq!(parse_input("/effort XHigh", &skills), Input::Effort(Some(Some(llm::Effort::XHigh))));
        assert_eq!(parse_input("/effort auto", &skills), Input::Effort(Some(None)));
        assert!(matches!(parse_input("/effort huge", &skills), Input::Error(_)));
        match parse_input("/greet  Bob ", &skills) {
            Input::Prompt(p) => {
                assert!(p.starts_with("The user invoked the `greet` skill:"));
                assert!(p.ends_with("Say hi.\n\nARGUMENTS: Bob"), "{p}");
            }
            other => panic!("{other:?}"),
        }
    }
}
