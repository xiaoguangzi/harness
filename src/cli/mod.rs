//! Terminal frontend: the interactive REPL and one-shot mode.

mod clipboard;
mod diff;
mod dialog;
mod commands;
mod input;
mod keys;
mod markdown;
mod menu;
mod model;
mod output;
mod repl;
mod rewind;
mod screen;
mod text;
mod tool_output;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use rustyline::error::ReadlineError;
use rustyline::{Cmd, KeyCode, KeyEvent, Modifiers};
use serde_json::Value;

use crate::agent::{Agent, Inbox};
use crate::checkpoint;
use crate::hooks::{Event, Hooks};
use crate::llm;
use crate::mcp;
use crate::mentions;
use crate::plugins::{self, Plugins};
use crate::session::{self, Recorder, Resume, Store};
use crate::setup::Env;
use crate::skills::Skill;
use crate::subagent::AgentDef;
use crate::tools::FileTracker;
use clipboard::{Clip, Image};
use input::{Queued, Typing};
use keys::{Key, KeyWatch};
pub use output::Output;
use output::{Terminal, TurnEnd};
use menu::Command;
use repl::ReplHelper;

/// Built-in commands and what they do, as the menu and `/help` show them.
const BUILTINS: [(&str, &str); 21] = [
    ("/help", "browse commands; Enter inserts one into the prompt"),
    ("/model", "choose main/subagent models and effort; /model <provider>/<model> switches directly"),
    ("/effort", "choose reasoning effort, or set it with /effort <level>"),
    ("/provider", "manage providers and models; /provider add|remove|model|default … changes them"),
    ("/clear", "start a new conversation"),
    ("/compact", "summarize the conversation to free context; /compact <focus> says what to keep"),
    ("/resume", "pick up an earlier conversation from this directory"),
    ("/rewind", "go back to before an earlier prompt, with the code, the conversation or both (or Esc Esc)"),
    ("/init", "have the model write AGENTS.md, or improve it; /init <notes> says what else to cover"),
    ("/usage", "show the tokens used so far and how full the context is"),
    ("/tools", "browse tool descriptions and input schemas"),
    ("/output", "inspect full tool inputs and returned output (or Ctrl+O, including while running)"),
    ("/skills", "browse skills; Enter inserts an invocation into the prompt"),
    ("/agents", "browse subagent types, models, permissions and tools"),
    ("/tasks", "browse this conversation's tasks and task actions"),
    ("/stop", "choose a running task to stop, or stop one with /stop <id>"),
    ("/mcp", "browse MCP servers, tools and connection errors"),
    ("/plugins", "manage plugins; /plugins install|uninstall|enable|disable|update|marketplace … also works"),
    ("/reload-plugins", "load plugins, skills, agents and hooks again, for the next conversation"),
    ("/sandbox", "show what the sandbox allows commands to do"),
    ("/exit", "quit (or exit, Ctrl-D)"),
];

/// Saved conversations offered by `/resume`.
const LISTED_SESSIONS: usize = 20;
/// How long the prompt waits after a turn before Notification hooks hear
/// that it does.
const IDLE_NOTICE: std::time::Duration = std::time::Duration::from_secs(60);

pub async fn run(agent: &mut Agent, args: Vec<String>, env: &mut Env, out: Output) -> Result<ExitCode> {
    let files = &env.files.clone();
    let mut skills = env.ext.skills.clone();
    let (resume, args) = session::parse_args(args)?;
    out.spawn_spinner();
    let mut term = Terminal::new(out.clone());
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let mut recorder = Store::default_location().map(|store| Recorder::new(store, cwd));
    if let Some(rec) = &recorder {
        attach_session(agent, rec);
    }
    let one_shot = !args.is_empty();

    let boxed = !one_shot && std::io::stdin().is_terminal() && out.interactive();
    let _screen = boxed.then(|| out.enter_screen());
    if !one_shot {
        print_banner(agent, &skills, env, &out);
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
            Resume::Pick => pick_session(rec, &out, boxed).await?,
        };
        if let Some(id) = id {
            resume_session(agent, rec, &id, &out, !one_shot).await?;
        }
    }

    // One-shot mode: `harness "do something"` or `harness /skill args`.
    if one_shot {
        let line = args.join(" ");
        let input = match render_skill(parse_input(&line, &skills)).await {
            Input::Skill(skill, args) => match run_fork(agent, env, &skill, &args, &out).await {
                Ok(prompt) => Input::Prompt(prompt),
                Err(e) => Input::Error(e),
            },
            input => input,
        };
        return match input {
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
                show_hook_notices(&out, agent.end_session("other").await);
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

    let commands = menu_commands(agent, &skills);
    let config = rustyline::Config::builder()
        .completion_type(rustyline::CompletionType::List)
        .auto_add_history(true)
        // Otherwise a lone Esc waits for the next key, and Esc Esc is one.
        .keyseq_timeout(Some(50))
        .build();
    let mut editor: rustyline::Editor<ReplHelper, rustyline::history::DefaultHistory> =
        rustyline::Editor::with_config(config)?;
    let helper = ReplHelper::new(commands);
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
    // Whether a turn ran since the prompt last waited a while.
    let mut answered = false;
    loop {
        // A background subagent reported back, or a message sent from the
        // input box is still on its way: the model answers first.
        if agent.inbox().wants_turn() {
            let input = boxed.then_some(TurnInput { typing: &mut typing, files });
            let _ = run_shown(agent, Vec::new(), None, &out, &mut term, recorder.as_mut(), input).await;
            remember_sent(&mut editor, &mut typing);
            out.line("");
            answered = true;
            continue;
        }
        let (line, images) = match typing.next_command() {
            // Sent from the input box while a turn ran.
            Some(command) => {
                out.lines(&prompt_lines(&out, &command));
                (command, Vec::new())
            }
            None if boxed => {
                let last = mentions::last_pasted_image(agent.messages()).max(typing.draft.last_image());
                typing.draft.note_image(last);
                let idle = std::mem::take(&mut answered).then(|| notify_when_idle(agent.hooks(), agent.model()));
                let past = editor.history().iter().cloned().collect();
                let read = editor.helper().unwrap().read_box(&out, &mut typing, agent.inbox().clone(), past).await;
                if let Some(idle) = idle { idle.abort(); }
                let (line, images) = match read {
                    Ok(Some(message)) => message,
                    Ok(None) => { interrupted = false; continue; }
                    Err(ReadlineError::Interrupted) if !interrupted => {
                        interrupted = true;
                        out.line(out.paint("2", "(press Ctrl-C again or Ctrl-D to exit)"));
                        continue;
                    }
                    Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
                    Err(e) => return Err(e.into()),
                };
                interrupted = false;
                let line = repl::join_continuations(line.trim());
                if !line.is_empty() {
                    let _ = editor.add_history_entry(&line);
                    out.lines(&prompt_lines(&out, &line));
                }
                (line, images)
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
                let idle = std::mem::take(&mut answered).then(|| notify_when_idle(agent.hooks(), agent.model()));
                let read = tokio::task::block_in_place(|| editor.readline_with_initial(&prompt, (&before, &after)));
                if let Some(idle) = idle {
                    idle.abort();
                }
                // What subagents printed meanwhile.
                out.hold(false);
                let line = match read {
                    Ok(line) => line,
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
                (line, helper.take_images())
            }
        };
        let input = render_skill(parse_input(&line, &skills)).await;
        let input = if boxed && line.split_whitespace().count() == 1 {
            let Some(input) = commands::interactive(input, agent, env, &skills, &out, &mut typing).await else { continue };
            input
        } else { input };
        match input {
            Input::Empty => {}
            Input::Exit => break,
            Input::Help => print_help(&out, &skills),
            Input::Clear => {
                show_hook_notices(&out, agent.end_session("clear").await);
                agent.clear();
                out.restore_tool_output(agent.messages());
                if apply_reload(agent, env, &mut skills, editor.helper()).await {
                    out.line(out.paint("2", "the reloaded plugins apply from here"));
                } else {
                    refresh_system(agent);
                }
                if let Some(rec) = recorder.as_mut() {
                    rec.restart();
                    attach_session(agent, rec);
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
                    None => pick_session(rec, &out, boxed).await,
                };
                let resumed = match id {
                    Ok(Some(id)) => {
                        if env.has_pending() {
                            show_hook_notices(&out, agent.end_session("resume").await);
                            apply_reload(agent, env, &mut skills, editor.helper()).await;
                        } else {
                            refresh_system(agent);
                        }
                        resume_session(agent, rec, &id, &out, true).await
                    }
                    Ok(None) => Ok(()),
                    Err(e) => Err(e),
                };
                if let Err(e) = resumed {
                    out.line(out.paint("31", &format!("{e:#}")));
                }
            }
            Input::Rewind => match rewind::run(agent, &out, &mut typing, boxed).await {
                Ok(true) => {
                    out.restore_tool_output(agent.messages());
                    if let Some(rec) = recorder.as_mut() {
                        save_session(agent, rec, &out);
                    }
                }
                Ok(false) => {}
                Err(e) => out.line(out.paint("31", &format!("{e:#}"))),
            },
            Input::ListSkills => print_skills(&out, &skills),
            Input::ListTools => print_tools(&out, agent),
            Input::ToolOutput => out.show_tool_output(boxed),
            Input::ListMcp => print_mcp(&out, env.mcp_statuses()),
            Input::Plugins { update: false } => print_plugins(&out, &env.ext.plugins),
            Input::Plugins { update: true } => update_plugins(&env.ext.plugins, &out).await,
            Input::ManagePlugins(args) => {
                let handoff = out.handoff();
                let managed = tokio::task::block_in_place(|| plugins::manage::run(&args, &mut plugins::manage::TerminalAsk));
                drop(handoff);
                match managed {
                    Ok(lines) => out.lines(&lines),
                    Err(e) => out.line(out.paint("31", &format!("{e:#}"))),
                }
                if !matches!(args.first().map(String::as_str), Some("list" | "help") | None)
                    && !matches!(args.get(..2), Some([a, b]) if a == "marketplace" && b == "list")
                {
                    out.line(out.paint("2", "run /reload-plugins to use the change"));
                }
            }
            Input::ReloadPlugins => match env.reload().await {
                Ok(lines) => {
                    for (i, line) in lines.iter().enumerate() {
                        out.line(if i == 0 { line.clone() } else { out.paint("33", line) });
                    }
                    if agent.messages().is_empty() {
                        apply_reload(agent, env, &mut skills, editor.helper()).await;
                        if let Some(rec) = recorder.as_ref() {
                            attach_session(agent, rec);
                        }
                        out.line(out.paint("2", "they apply from the next message"));
                    } else {
                        out.line(out.paint("2", "they apply to the next conversation: /clear starts one"));
                    }
                }
                Err(e) => out.line(out.paint("31", &format!("{e:#}"))),
            },
            Input::ListAgents => print_agents(&out, &env.ext.agent_defs),
            Input::Tasks => print_tasks(&out, &agent.subagents().list(), &agent.shells().list()),
            Input::Stop(None) => {
                let stopped = agent.subagents().stop_all();
                let killed = agent.shells().stop_all();
                let note = format!(
                    "stopped {stopped} {} and {killed} {}",
                    text::plural(stopped, "subagent"),
                    text::plural(killed, "background command")
                );
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
            Input::Sandbox => print_sandbox(&out),
            Input::Usage => print_usage(&out, agent),
            Input::Model(spec) => {
                if spec.is_none() && boxed {
                    if let Some(client) = model::pick(&out, agent.client()).await {
                        out.line(format!("model: {}", out.paint("1", client.spec())));
                        out.line(format!("subagents: {}", client.subagent_model().unwrap_or("follow main model")));
                        out.line(format!("effort: {}", out.paint("1", client.effort().as_str())));
                        agent.set_client(client);
                    }
                    continue;
                }
                let spec = match spec {
                    None => { print_models(&out, agent); None }
                    spec => spec,
                };
                if let Some(spec) = spec {
                    match agent.client().switch(&spec) {
                        Ok(client) => {
                            out.line(format!("model: {}", out.paint("1", client.spec())));
                            agent.set_client(client);
                        }
                        Err(e) => out.line(out.paint("31", &format!("{e:#}"))),
                    }
                }
            }
            Input::Providers(args) => {
                let handoff = out.handoff();
                let managed = tokio::task::block_in_place(|| llm::manage::run(&args, &mut plugins::manage::TerminalAsk));
                drop(handoff);
                match managed {
                    Ok(lines) => {
                        out.lines(&lines);
                        if llm::manage::changes(&args) {
                            reload_providers(agent, &skills, editor.helper(), &out);
                        }
                    }
                    Err(e) => out.line(out.paint("31", &format!("{e:#}"))),
                }
            }
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
            // A skill that runs in a subagent: its report goes to the model.
            Input::Skill(skill, args) => match run_fork(agent, env, &skill, &args, &out).await {
                Ok(prompt) => {
                    let input = boxed.then_some(TurnInput { typing: &mut typing, files });
                    let attached = attachments(&line, &images, files);
                    let _ = submit(agent, &line, &prompt, attached, &out, &mut term, recorder.as_mut(), input).await;
                    remember_sent(&mut editor, &mut typing);
                    out.line("");
                    answered = true;
                }
                Err(e) => out.line(out.paint("31", &e)),
            },
            Input::Prompt(prompt) => {
                // The turn's outcome is already shown.
                let attached = attachments(&line, &images, files);
                let input = boxed.then_some(TurnInput { typing: &mut typing, files });
                let _ = submit(agent, &line, &prompt, attached, &out, &mut term, recorder.as_mut(), input).await;
                remember_sent(&mut editor, &mut typing);
                out.line("");
                answered = true;
            }
        }
    }
    show_hook_notices(&out, agent.end_session("prompt_input_exit").await);
    if let Some(path) = &history {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = editor.save_history(path);
    }
    Ok(ExitCode::SUCCESS)
}

/// What the menu under the prompt offers.
fn menu_commands(agent: &Agent, skills: &[Skill]) -> Vec<Command> {
    let mut commands: Vec<Command> = BUILTINS.iter().map(|(name, description)| Command::new(*name, *description)).collect();
    commands.extend(
        skills.iter().filter(|s| s.user_invocable).map(|s| Command::new(format!("/{}", s.name), &s.description)),
    );
    commands.extend(
        agent.client().providers().listed_models().iter().map(|m| Command::new(format!("/model {m}"), "switch to this model")),
    );
    commands.extend(llm::Effort::ALL.iter().map(|e| Command::new(format!("/effort {}", e.as_str()), effort_help(*e))));
    commands.push(Command::new("/effort auto", "back to the configured effort"));
    commands
}

/// Gives the agent, whose conversation was just cleared or is about to be
/// replaced, what `/reload-plugins` loaded: its tools, system prompt and
/// hooks, and the prompt its commands. False if nothing was reloaded.
async fn apply_reload(agent: &mut Agent, env: &mut Env, skills: &mut Arc<Vec<Skill>>, helper: Option<&ReplHelper>) -> bool {
    if !env.apply() {
        return false;
    }
    let (tools, system) = env.conversation(agent.client());
    agent.set_tools(tools);
    agent.set_system(system);
    agent.set_hooks(Hooks::with_config(env.ext.hooks.clone()));
    *skills = env.ext.skills.clone();
    if let Some(helper) = helper {
        helper.set_commands(menu_commands(agent, skills));
    }
    env.shutdown_retired().await;
    true
}

/// Moves the agent to the providers as the settings have them now, on the
/// same model if its provider is still there.
fn reload_providers(agent: &mut Agent, skills: &[Skill], helper: Option<&ReplHelper>, out: &Output) {
    match crate::settings::Settings::load().and_then(|s| llm::Providers::from_settings(&s)) {
        Ok(providers) => {
            let client = agent.client().with_providers(providers);
            if client.spec() != agent.model() {
                out.line(format!("model: {}", out.paint("1", client.spec())));
            }
            agent.set_client(client);
            if let Some(helper) = helper {
                helper.set_commands(menu_commands(agent, skills));
            }
        }
        Err(_) => out.line(out.paint("2", "this conversation keeps the providers it had")),
    }
}

/// Builds the system prompt again for a new conversation, so it has the
/// `AGENTS.md` as it is now (`/init` may have written it).
fn refresh_system(agent: &mut Agent) {
    let system = crate::prompt::main(agent.tools());
    agent.set_system(system);
}

/// Runs the Notification hooks if the prompt still waits after
/// [`IDLE_NOTICE`]; abort the task once a line is read.
fn notify_when_idle(hooks: &Hooks, model: &str) -> tokio::task::JoinHandle<()> {
    let (hooks, model) = (hooks.clone(), model.to_string());
    tokio::spawn(async move {
        if !hooks.has(Event::Notification) {
            return;
        }
        tokio::time::sleep(IDLE_NOTICE).await;
        // On a task of its own, which reading the line does not cut short.
        tokio::spawn(async move {
            hooks.notification("idle_prompt", "harness is waiting for your input", &model).await;
        });
    })
}

/// Runs skill `skill`, which runs in a subagent, as the user typed it with
/// `args`, until it is done or Ctrl-C or Esc interrupts it; returns the
/// message handing its report to the model.
async fn run_fork(agent: &Agent, env: &Env, skill: &Skill, args: &str, out: &Output) -> Result<String, String> {
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

fn history_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".harness/history"))
}

/// Lists this directory's saved conversations and asks which one to resume;
/// `None` when there are none, or the user picked none.
async fn pick_session(rec: &Recorder, out: &Output, boxed: bool) -> Result<Option<String>> {
    let sessions: Vec<session::Meta> =
        rec.store().list(&rec.meta().cwd).into_iter().filter(|m| m.id != rec.meta().id).take(LISTED_SESSIONS).collect();
    if sessions.is_empty() {
        out.line(out.paint("2", "no saved conversations in this directory"));
        return Ok(None);
    }
    let now = session::now();
    if boxed {
        let items = sessions.iter().map(|m| dialog::Item::new(
            if m.title.is_empty() { "(untitled)" } else { &m.title },
            format!("{} · {} messages · {}", session::ago(m.updated, now), m.messages, m.model),
        ).details(vec![format!("Session: {}", m.id), format!("Directory: {}", m.cwd)])).collect();
        let selected = dialog::choose(out, "Resume conversation", "Enter resumes · Tab details · Esc cancels", items, 0).await;
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
async fn resume_session(agent: &mut Agent, rec: &mut Recorder, id: &str, out: &Output, show: bool) -> Result<()> {
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
    attach_session(agent, rec);
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
fn show_hook_notices(out: &Output, notices: Vec<String>) {
    for notice in notices {
        out.line(out.paint("33", &format!("⚑ {}", text::printable(&notice))));
    }
}

/// Tells the agent which saved conversation it is: its hooks run for it,
/// and its checkpoints are saved with it.
fn attach_session(agent: &Agent, rec: &Recorder) {
    agent.hooks().set_session(&rec.meta().id, Some(rec.file()));
    agent.file_history().set_dir(checkpoint::session_dir(&rec.meta().id));
}

/// Saves the conversation and its checkpoints.
fn save_session(agent: &Agent, rec: &mut Recorder, out: &Output) {
    if let Err(e) = rec.save(agent.messages(), agent.model()).and_then(|()| agent.file_history().save()) {
        out.line(out.paint("33", &format!("conversation not saved: {e:#}")));
    }
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
    agent.set_prompt_label(line);
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
    let (interrupt_tx, mut interrupt_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut keys = match input.as_mut() {
        Some(input) => {
            out.open_input(std::mem::take(input.typing), inbox.clone());
            let typed = out.clone();
            KeyWatch::for_prompt(move |key| {
                if typed.tool_output_key(&key) { return; }
                if key == Key::Esc { let _ = interrupt_tx.send(()); }
                else { type_key(&typed, key, &tx); }
            })
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
            Some(()) = interrupt_rx.recv() => break Err(TurnEnd::Interrupted),
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
        save_session(agent, rec, out);
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
    {
        save_session(agent, rec, out);
    }
    result
}

/// Types `key` into the input box. Enter hands what it holds to `tx`.
fn type_key(out: &Output, key: Key, tx: &tokio::sync::mpsc::UnboundedSender<(String, Vec<(usize, Image)>)>) {
    if out.tool_output_key(&key) { return; }
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

fn paste_clipboard(out: &Output) {
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

/// Shows that `running` background subagents are still at work, and what
/// they last did, until one reports back; false if the user interrupted the
/// wait.
async fn wait_for_reports(agent: &Agent, running: usize, out: &Output) -> bool {
    out.set_status(Some(&format!("Waiting for {running} background {}…", text::plural(running, "subagent"))));
    let mut esc = KeyWatch::start();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    let reported = loop {
        tokio::select! {
            _ = agent.inbox().wanted() => break true,
            _ = tokio::signal::ctrl_c() => break false,
            _ = esc.pressed() => break false,
            _ = tick.tick() => out.set_status_detail(&background_progress(&agent.subagents().list())),
        }
    };
    drop(esc);
    out.set_status(None);
    reported
}

/// The latest call of each running background subagent, for the line
/// under the status.
fn background_progress(subagents: &[Value]) -> String {
    let running = subagents.iter().filter(|s| s["status"] == "running" && s["background"] == true);
    let lines: Vec<String> = running
        .map(|s| {
            let last = s["last_call"].as_str().unwrap_or("starting");
            format!("{} › {}", text::fit(s["label"].as_str().unwrap_or(""), 32), text::printable(last))
        })
        .collect();
    lines.join(" · ")
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
    ToolOutput,
    ListMcp,
    ListAgents,
    /// `/tasks` lists this conversation's subagents.
    Tasks,
    /// `/stop` stops every running subagent; `/stop <id>` that one.
    Stop(Option<String>),
    Sandbox,
    /// `/usage` shows the tokens used and the context's size.
    Usage,
    /// `/model` shows the current model; `/model <spec>` switches.
    Model(Option<String>),
    /// `/resume` lists saved conversations to pick from; `/resume <id>`
    /// resumes that one.
    Resume(Option<String>),
    /// `/rewind` goes back to before an earlier prompt.
    Rewind,
    /// `/provider …`, as `harness provider` takes it.
    Providers(Vec<String>),
    /// `/effort` shows the effort; `/effort <level>` sets it for this
    /// session, and `/effort auto` restores the configured one.
    Effort(Option<Option<llm::Effort>>),
    /// `/plugins` lists them; `/plugins update` fetches their latest.
    Plugins { update: bool },
    /// `/plugins install …` and the like, as `harness plugin` takes them.
    ManagePlugins(Vec<String>),
    /// `/reload-plugins`.
    ReloadPlugins,
    /// `/<skill> args`, before its text is read.
    Skill(Box<Skill>, String),
    Prompt(String),
    Error(String),
}

/// Reads the skill a `/<skill> args` line runs into the prompt; one that
/// runs in a subagent, and other input, passes through.
async fn render_skill(input: Input) -> Input {
    match input {
        Input::Skill(skill, args) if skill.fork.is_none() => match skill.invocation_prompt(&args).await {
            Ok(prompt) => Input::Prompt(prompt),
            Err(e) => Input::Error(e),
        },
        input => input,
    }
}

/// Interprets a line of user input. `/<skill> args` is to load the skill's
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
        "rewind" => Input::Rewind,
        "init" => Input::Prompt(crate::prompt::init(args)),
        "skills" => Input::ListSkills,
        "tools" => Input::ListTools,
        "output" => Input::ToolOutput,
        "mcp" => Input::ListMcp,
        "plugins" | "plugin" => match args.trim() {
            "" | "list" => Input::Plugins { update: false },
            "update" => Input::Plugins { update: true },
            args => Input::ManagePlugins(args.split_whitespace().map(String::from).collect()),
        },
        "reload-plugins" => Input::ReloadPlugins,
        "agents" => Input::ListAgents,
        "tasks" => Input::Tasks,
        "stop" => Input::Stop(Some(args.trim()).filter(|a| !a.is_empty()).map(String::from)),
        "sandbox" => Input::Sandbox,
        "usage" => Input::Usage,
        "model" => Input::Model(Some(args.trim()).filter(|a| !a.is_empty()).map(String::from)),
        "provider" | "providers" => Input::Providers(args.split_whitespace().map(String::from).collect()),
        "effort" => match args.trim() {
            "" => Input::Effort(None),
            "auto" | "default" => Input::Effort(Some(None)),
            level => match llm::Effort::parse(level) {
                Ok(e) => Input::Effort(Some(Some(e))),
                Err(e) => Input::Error(e.to_string()),
            },
        },
        _ => match skills.iter().find(|s| s.name == name && s.user_invocable) {
            Some(skill) => Input::Skill(Box::new(skill.clone()), args.to_string()),
            None => Input::Error(format!("unknown command /{name} (try /help)")),
        },
    }
}

fn print_banner(agent: &Agent, skills: &[Skill], env: &Env, out: &Output) {
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    let cwd = match cwd.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => cwd,
    };
    let servers = env.mcp_statuses().filter(|s| s.outcome.is_ok()).count();
    let loaded = env.ext.plugins.list.iter().filter(|p| p.outcome.is_ok()).count();
    let plugins = match loaded {
        0 => String::new(),
        1 => " · 1 plugin".to_string(),
        n => format!(" · {n} plugins"),
    };
    let effort = out.paint("2", &format!(" · {} effort", agent.client().effort().as_str()));
    out.line(format!("{} {}{effort}", out.paint("1;35", "✻ harness"), out.paint("1", agent.model())));
    out.line(out.paint(
        "2",
        &format!(
            "  {cwd} · {} tools · {} skills · {servers} MCP servers{plugins}{}",
            agent.tools().names().count(),
            skills.len(),
            if crate::sandbox::active().is_some() { " · sandboxed" } else { "" }
        ),
    ));
    out.line(out.paint("2", "  / for commands · @ for files · Ctrl+O for tool output · Ctrl+V to paste an image · Esc or Ctrl-C to interrupt"));
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

fn help_keys() -> [(&'static str, &'static str); 10] {
    [
        ("/<skill> [args]", "run a skill"),
        ("@path", "attach a file, directory, image or PDF; @ opens a file menu (@file#L10-20 for lines)"),
        ("Ctrl+O", "inspect full tool output; Tab returns to the call list, Esc goes back/closes"),
        ("Ctrl+V", "paste an image from the clipboard (files copied in Finder or a file manager become @ mentions)"),
        ("\\ or Alt+Enter", "continue on a new line"),
        ("↑ ↓ Tab Enter", "pick from the menu shown while typing / or @"),
        ("↑", "while the model works: edit the messages queued for it"),
        ("Esc", "interrupt the running turn"),
        ("Esc Esc", "at an empty prompt: go back to before an earlier prompt (/rewind)"),
        ("Ctrl-C", "interrupt the running turn; twice at the prompt to quit"),
    ]
}

fn print_help(out: &Output, skills: &[Skill]) {
    let keys = help_keys();
    for (key, what) in BUILTINS.iter().chain(&keys) {
        out.line(format!("  {} {what}", out.paint("1", &text::pad(key, 16))));
    }
    let names: Vec<String> = skills.iter().filter(|s| s.user_invocable).map(|s| format!("/{}", s.name)).collect();
    if !names.is_empty() {
        out.line(format!("  {}", out.paint("2", &format!("skills: {}", names.join(" ")))));
    }
}

fn print_tools(out: &Output, agent: &Agent) {
    let tools = agent.tools().definitions();
    let name_width = tools.iter().map(|t| text::width(&t.name)).max().unwrap_or(0);
    let room = text::terminal_width().saturating_sub(name_width + 5);
    for tool in tools {
        let summary = tool.description.split(". ").next().unwrap_or("").trim_end_matches('.');
        out.line(format!("  {}  {}", out.paint("1", &text::pad(&tool.name, name_width)), out.paint("2", &text::fit(summary, room))));
    }
}

fn print_sandbox(out: &Output) {
    let Some(sandbox) = crate::sandbox::active() else {
        out.line("the sandbox is off; start with --sandbox, HARNESS_SANDBOX=1 or \"sandbox\": {\"enabled\": true} in ~/.harness/settings.json");
        return;
    };
    for (label, value) in sandbox.describe() {
        out.line(format!("  {} {value}", out.paint("1", &text::pad(label, 10))));
    }
}

fn print_usage(out: &Output, agent: &Agent) {
    let models = agent.usage().models();
    if models.is_empty() {
        out.line("no requests yet");
    } else {
        let width = models.iter().map(|(m, _)| text::width(m)).max().unwrap_or(0).max("model".len());
        let header = format!(
            "{}  {:>8}  {:>9}  {:>10}  {:>11}  {:>9}",
            text::pad("model", width),
            "requests",
            "input",
            "cache read",
            "cache write",
            "output"
        );
        out.line(format!("  {}", out.paint("2", &header)));
        for (model, t) in &models {
            let u = &t.usage;
            out.line(format!(
                "  {}  {:>8}  {:>9}  {:>10}  {:>11}  {:>9}",
                out.paint("1", &text::pad(model, width)),
                t.requests,
                output::tokens(u.input_tokens),
                output::tokens(u.cache_read_tokens),
                output::tokens(u.cache_write_tokens),
                output::tokens(u.output_tokens)
            ));
        }
        let input: u64 = models.iter().map(|(_, t)| t.usage.input_tokens).sum();
        let cached: u64 = models.iter().map(|(_, t)| t.usage.cache_read_tokens).sum();
        if let Some(percent) = (cached * 100).checked_div(input) {
            out.line(format!("  {}", out.paint("2", &format!("{percent}% of input tokens came from the prompt cache"))));
        }
    }
    let used = agent.context_tokens();
    let window = agent.client().context_window();
    let size = match window {
        // A model the providers do not know.
        w if w >= u64::from(u32::MAX) => String::new(),
        w => format!(" of {} ({}%)", output::tokens(w), used * 100 / w.max(1)),
    };
    out.line(format!("context: about {} tokens{size}", output::tokens(used)));
}

fn print_skills(out: &Output, skills: &[Skill]) {
    if skills.is_empty() {
        out.line(
            "no skills or commands found (looked in .harness/skills, .claude/skills, .agents/skills, \
             .harness/commands, .claude/commands and the same under ~)",
        );
    }
    for s in skills {
        out.line(format!("{}  {}", out.paint("1", &s.name), s.description));
        out.line(format!("  {}", out.paint("90", &s.dir.display().to_string())));
    }
}

fn print_models(out: &Output, agent: &Agent) {
    let providers = agent.client().providers();
    out.line(format!("current: {}", out.paint("1", agent.model())));
    out.line(format!("subagents: {}", agent.client().subagent_model().unwrap_or("follow main model")));
    out.line(format!("effort: {}", agent.client().effort().as_str()));
    for name in providers.names() {
        let protocol = providers.protocol_of(name).map_or("?", llm::Protocol::as_str);
        out.line(format!("provider {} ({protocol})", out.paint("1", name)));
    }
    for m in providers.listed_models() {
        let mark = if m == agent.model() { out.paint("32", "●") } else { " ".to_string() };
        out.line(format!(" {mark} {m}"));
    }
    out.line(out.paint("90", "switch with /model <provider>/<model>; any model name the gateway accepts works"));
    out.line(out.paint("90", "add a provider with /provider add <name>, a model with /provider model add <provider>/<model>"));
}

fn print_agents(out: &Output, defs: &[AgentDef]) {
    for d in defs {
        let access = d.permission.as_str();
        let tools = d.tools.as_ref().map_or("all permitted".to_string(), |t| t.join(", "));
        let source = d.path.as_ref().map_or("built-in".to_string(), |p| p.display().to_string());
        let model = d.model.as_ref().map_or(String::new(), |m| format!(" · model {m}"));
        out.line(format!("{}  [{access}; tools: {tools}]", out.paint("1", &d.name)));
        out.line(format!("  {}", d.description));
        out.line(format!("  {}", out.paint("90", &format!("{source}{model}"))));
    }
}

fn print_tasks(out: &Output, subagents: &[Value], shells: &[(String, String, bool)]) {
    if subagents.is_empty() && shells.is_empty() {
        out.line("no subagents or background commands in this conversation");
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
        if let Some(last) = s["last_call"].as_str().filter(|_| status == "running") {
            info.push(format!("latest {}", text::printable(last)));
        }
        out.line(format!(
            "  {}  {}  {}  {}",
            out.paint("1", s["id"].as_str().unwrap_or("")),
            out.paint(color, &text::pad(status, 9)),
            text::printable(s["label"].as_str().unwrap_or("")),
            out.paint("2", &info.join(" · "))
        ));
    }
    for (id, command, running) in shells {
        let (color, status) = if *running { ("36", "running") } else { ("2", "ended") };
        let command = text::fit(&text::printable(command.lines().next().unwrap_or("")), 80);
        out.line(format!("  {}  {}  {}", out.paint("1", id), out.paint(color, &text::pad(status, 9)), out.paint("2", &format!("$ {command}"))));
    }
    out.line(out.paint("90", "stop one with /stop <id>, or every running one with /stop"));
}

fn print_plugins(out: &Output, plugins: &Plugins) {
    if plugins.list.is_empty() {
        out.line("no plugins enabled (`enabledPlugins` in ~/.harness/settings.json, or --plugin-dir)");
    }
    for p in &plugins.list {
        let version = p.version.as_ref().map_or(String::new(), |v| format!(" {}", out.paint("2", &format!("v{v}"))));
        match &p.outcome {
            Ok(parts) => {
                let parts = if parts.is_empty() { "nothing the harness can use".to_string() } else { parts.join(", ") };
                out.line(format!("{} {}{version}  {parts}", out.paint("32", "●"), out.paint("1", &p.id)));
            }
            Err(e) => out.line(format!("{} {}  {e}", out.paint("31", "●"), out.paint("1", &p.id))),
        }
        if let Some(description) = &p.description {
            out.line(format!("    {description}"));
        }
        if let Some(root) = &p.root {
            out.line(format!("    {}", out.paint("90", &root.display().to_string())));
        }
        for warning in &p.warnings {
            out.line(format!("    {}", out.paint("33", &format!("! {warning}"))));
        }
        if !p.ignored.is_empty() {
            out.line(format!("    {}", out.paint("2", &format!("not used: {}", p.ignored.join("; ")))));
        }
    }
    out.line(out.paint("90", "manage them with /plugins install|uninstall|enable|disable|configure|marketplace … (see /plugins help)"));
}

/// Pulls the latest of every plugin and marketplace cloned from git.
async fn update_plugins(plugins: &Plugins, out: &Output) {
    let plugins = plugins.clone();
    out.line(out.paint("2", "updating plugins…"));
    let lines = match tokio::task::spawn_blocking(move || plugins.update()).await {
        Ok(lines) => lines,
        Err(e) => vec![Err(e.to_string())],
    };
    if lines.is_empty() {
        out.line(out.paint("2", "no plugins come from git repositories"));
        return;
    }
    for line in lines {
        match line {
            Ok(line) => out.line(line),
            Err(e) => out.line(out.paint("31", &e)),
        }
    }
    out.line(out.paint("2", "/reload-plugins (or a restart) uses the new versions"));
}

fn print_mcp<'a>(out: &Output, statuses: impl Iterator<Item = &'a mcp::Status>) {
    let statuses: Vec<&mcp::Status> = statuses.collect();
    if statuses.is_empty() {
        out.line("no MCP servers configured (`mcpServers` in ~/.harness/settings.json)");
    }
    for status in statuses {
        match &status.outcome {
            Ok(tools) => {
                out.line(format!("{} {} ({} tools)", out.paint("32", "●"), status.name, tools.len()));
                for t in tools {
                    out.line(format!("    {t}"));
                }
            }
            Err(e) => out.line(format!("{} {} failed: {e}", out.paint("31", "●"), status.name)),
        }
        out.line(format!("    {}", out.paint("90", &format!("log: {}", mcp::log_path(&status.name).display()))));
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

    #[tokio::test]
    async fn parses_commands() {
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
        assert_eq!(parse_input("/init", &skills), Input::Prompt(crate::prompt::init("")));
        assert_eq!(parse_input("/init cover the web UI", &skills), Input::Prompt(crate::prompt::init("cover the web UI")));
        assert_eq!(parse_input("/compact keep the API notes", &skills), Input::Compact(Some("keep the API notes".into())));
        assert_eq!(parse_input("/tools", &skills), Input::ListTools);
        assert_eq!(parse_input("/output", &skills), Input::ToolOutput);
        assert_eq!(parse_input("/tasks", &skills), Input::Tasks);
        assert_eq!(parse_input("/usage", &skills), Input::Usage);
        assert_eq!(parse_input("/stop", &skills), Input::Stop(None));
        assert_eq!(parse_input("/stop a3f9c2", &skills), Input::Stop(Some("a3f9c2".into())));
        assert_eq!(parse_input("/resume", &skills), Input::Resume(None));
        assert_eq!(parse_input("/resume 2026 ", &skills), Input::Resume(Some("2026".into())));
        assert_eq!(parse_input("fix it", &skills), Input::Prompt("fix it".into()));
        assert!(matches!(parse_input("/nope", &skills), Input::Error(_)));
        assert_eq!(parse_input("/effort", &skills), Input::Effort(None));
        assert_eq!(parse_input("/provider", &skills), Input::Providers(Vec::new()));
        assert_eq!(parse_input("/providers model add gw/m", &skills), Input::Providers(vec!["model".into(), "add".into(), "gw/m".into()]));
        assert_eq!(parse_input("/effort XHigh", &skills), Input::Effort(Some(Some(llm::Effort::XHigh))));
        assert_eq!(parse_input("/effort auto", &skills), Input::Effort(Some(None)));
        assert!(matches!(parse_input("/effort huge", &skills), Input::Error(_)));
        assert_eq!(parse_input("/plugins", &skills), Input::Plugins { update: false });
        assert_eq!(parse_input("/plugins update", &skills), Input::Plugins { update: true });
        assert_eq!(parse_input("/plugins remove x@m", &skills), Input::ManagePlugins(vec!["remove".into(), "x@m".into()]));
        assert_eq!(parse_input("/reload-plugins", &skills), Input::ReloadPlugins);
        match render_skill(parse_input("/greet  Bob ", &skills)).await {
            Input::Prompt(p) => {
                assert!(p.starts_with("The user invoked the `greet` skill:"));
                assert!(p.ends_with("Say hi.\n\nARGUMENTS: Bob"), "{p}");
            }
            other => panic!("{other:?}"),
        }
    }
}
