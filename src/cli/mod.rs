//! Terminal frontend: the interactive REPL and one-shot mode.

mod diff;
mod keys;
mod markdown;
mod menu;
mod output;
mod repl;
mod text;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use rustyline::error::ReadlineError;
use rustyline::{Cmd, KeyCode, KeyEvent, Modifiers};
use serde_json::Value;

use crate::agent::Agent;
use crate::llm;
use crate::mcp;
use crate::mentions;
use crate::session::{self, Recorder, Resume, Store};
use crate::skills::Skill;
use crate::subagent::AgentDef;
use crate::tools::FileTracker;
pub use output::Output;
use output::{Terminal, TurnEnd};
use menu::Command;
use repl::{ReplHelper, Wake};

/// Built-in commands and what they do, as the menu and `/help` show them.
const BUILTINS: [(&str, &str); 13] = [
    ("/help", "show commands and keys"),
    ("/model", "show the model, or switch with /model <provider>/<model>"),
    ("/effort", "show the reasoning effort, or set it with /effort <level>"),
    ("/clear", "start a new conversation"),
    ("/resume", "pick up an earlier conversation from this directory"),
    ("/tools", "list the tools the model can call"),
    ("/skills", "list available skills"),
    ("/agents", "list subagent types and their tools"),
    ("/tasks", "list the subagents this conversation started"),
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
                let mut result = submit(agent, &line, &prompt, files, &out, &mut term, recorder.as_mut()).await;
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
                    result = run_shown(agent, Vec::new(), None, &out, &mut term, recorder.as_mut()).await;
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
    let helper = ReplHelper::new(commands, out.styled(), wake.clone());
    helper.bind_keys(&mut editor);
    editor.set_helper(Some(helper));
    // Alt+Enter inserts a newline, like a trailing `\`.
    editor.bind_sequence(KeyEvent(KeyCode::Enter, Modifiers::ALT), Cmd::Newline);
    let history = history_path();
    if let Some(path) = &history {
        let _ = editor.load_history(path);
    }

    let mut interrupted = false;
    loop {
        // A background subagent reported back: the model answers first.
        if agent.inbox().wants_turn() {
            let _ = run_shown(agent, Vec::new(), None, &out, &mut term, recorder.as_mut()).await;
            out.line("");
            continue;
        }
        // Reading a line blocks; let the runtime move other tasks (such as
        // MCP connections) off this worker meanwhile.
        if let Some(helper) = editor.helper() {
            helper.new_line();
        }
        out.hold(true);
        let read = tokio::task::block_in_place(|| editor.readline("❯ "));
        let woken = wake.as_ref().is_some_and(|w| w.finish_reading()) && matches!(read, Err(ReadlineError::Interrupted));
        if woken {
            // Interrupted for a subagent's report: take the prompt back.
            print!("\x1b[1A\r\x1b[K");
        }
        // What subagents printed meanwhile.
        out.hold(false);
        let line = match read {
            Ok(line) => line,
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
        let mut line = repl::join_continuations(line.trim());
        if let Some(command) = editor.helper().and_then(ReplHelper::take_accepted) {
            // Enter ran the command selected in the menu; show it in place
            // of the prefix that was typed.
            if out.styled() {
                print!("\x1b[1A\r\x1b[K{}{command}\n", out.paint("1;35", "❯ "));
            }
            line = command;
        }
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
            Input::Tasks => print_tasks(&agent.subagents().list()),
            Input::Stop(None) => {
                let stopped = agent.subagents().stop_all();
                out.line(out.paint("2", &format!("stopped {stopped} subagent{}", if stopped == 1 { "" } else { "s" })));
            }
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
                let _ = submit(agent, &line, &prompt, files, &out, &mut term, recorder.as_mut()).await;
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

/// Attaches the files `@`-mentioned in `line` and runs the turn.
async fn submit(
    agent: &mut Agent,
    line: &str,
    prompt: &str,
    files: &FileTracker,
    out: &Output,
    term: &mut Terminal,
    recorder: Option<&mut Recorder>,
) -> Result<(), TurnEnd> {
    let first = agent.messages().is_empty();
    let expanded = mentions::expand(line, files);
    for a in &expanded.attachments {
        out.line(out.paint("2", &format!("  + {}", a.label)));
    }
    for w in &expanded.warnings {
        out.line(out.paint("33", &format!("  ! {w}")));
    }
    run_shown(agent, expanded.into_content(prompt), first.then_some(line), out, term, recorder).await
}

/// Runs a turn until it ends or Ctrl-C or Esc interrupts it; empty `content`
/// runs one for what waits in the agent's inbox. The turn's progress and
/// outcome are shown, and the conversation is saved after it, titled
/// `title` if it is new.
async fn run_shown(
    agent: &mut Agent,
    content: Vec<Value>,
    title: Option<&str>,
    out: &Output,
    term: &mut Terminal,
    recorder: Option<&mut Recorder>,
) -> Result<(), TurnEnd> {
    term.begin_turn();
    let mut esc = keys::EscWatch::start();
    // Dropping the turn cancels it: running commands are killed, and the
    // agent repairs its history at the start of the next turn.
    let result = tokio::select! {
        result = agent.run_turn(content, &mut *term) => result.map_err(TurnEnd::Failed),
        _ = tokio::signal::ctrl_c() => Err(TurnEnd::Interrupted),
        _ = esc.pressed() => Err(TurnEnd::Interrupted),
    };
    drop(esc);
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

/// Shows that `running` background subagents are still at work until one
/// reports back; false if the user interrupted the wait.
async fn wait_for_reports(agent: &Agent, running: usize, out: &Output) -> bool {
    let plural = if running == 1 { "" } else { "s" };
    out.set_status(Some(&format!("Waiting for {running} background subagent{plural}…")));
    let mut esc = keys::EscWatch::start();
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
    out.line(out.paint("2", "  / for commands · @ for files · Esc or Ctrl-C to interrupt"));
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
        println!("the sandbox is off; start with --sandbox, HARNESS_SANDBOX=1 or \"enabled\": true in ~/.harness/sandbox.json");
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

fn print_tasks(subagents: &[Value]) {
    if subagents.is_empty() {
        println!("no subagents in this conversation");
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
    println!("\x1b[90mstop one with /stop <id>, or every running one with /stop\x1b[0m");
}

fn print_mcp(mcp: &mcp::Manager) {
    if mcp.statuses.is_empty() {
        println!("no MCP servers configured (.mcp.json, ~/.harness/mcp.json or HARNESS_MCP_CONFIG)");
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
