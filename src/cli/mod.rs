//! Terminal frontend: the interactive REPL and one-shot mode.

mod markdown;
mod output;
mod repl;
mod text;

use std::process::ExitCode;

use anyhow::Result;
use rustyline::error::ReadlineError;
use rustyline::{Cmd, KeyCode, KeyEvent, Modifiers};

use crate::agent::Agent;
use crate::llm;
use crate::mcp;
use crate::mentions;
use crate::skills::Skill;
use crate::subagent::AgentDef;
use crate::tools::FileTracker;
pub use output::Output;
use output::{Terminal, TurnEnd};
use repl::ReplHelper;

pub async fn run(
    agent: &mut Agent,
    args: Vec<String>,
    skills: &[Skill],
    agent_defs: &[AgentDef],
    mcp: &mcp::Manager,
    files: &FileTracker,
    out: Output,
) -> Result<ExitCode> {
    out.spawn_spinner();
    let mut term = Terminal::new(out.clone());

    // One-shot mode: `harness "do something"` or `harness /skill args`.
    if !args.is_empty() {
        let line = args.join(" ");
        return match parse_input(&line, skills) {
            // The turn's outcome is already shown.
            Input::Prompt(prompt) => Ok(match submit(agent, &line, &prompt, files, &out, &mut term).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(TurnEnd::Interrupted) => ExitCode::from(130),
                Err(TurnEnd::Failed(_)) => ExitCode::FAILURE,
            }),
            Input::Error(e) => Err(anyhow::anyhow!(e)),
            _ => Err(anyhow::anyhow!("REPL commands are only available interactively")),
        };
    }

    print_banner(agent, skills, mcp, &out);

    let mut commands: Vec<String> = ["/help", "/model", "/clear", "/skills", "/agents", "/mcp", "/exit"]
        .iter()
        .map(|c| c.to_string())
        .collect();
    commands.extend(agent.client().providers().listed_models().iter().map(|m| format!("/model {m}")));
    commands.extend(skills.iter().map(|s| format!("/{}", s.name)));
    let config = rustyline::Config::builder()
        .completion_type(rustyline::CompletionType::List)
        .auto_add_history(true)
        .build();
    let mut editor: rustyline::Editor<ReplHelper, rustyline::history::DefaultHistory> =
        rustyline::Editor::with_config(config)?;
    editor.set_helper(Some(ReplHelper::new(commands, out.styled())));
    // Alt+Enter inserts a newline, like a trailing `\`.
    editor.bind_sequence(KeyEvent(KeyCode::Enter, Modifiers::ALT), Cmd::Newline);
    let history = history_path();
    if let Some(path) = &history {
        let _ = editor.load_history(path);
    }

    let mut interrupted = false;
    loop {
        // Reading a line blocks; let the runtime move other tasks (such as
        // MCP connections) off this worker meanwhile.
        let line = match tokio::task::block_in_place(|| editor.readline("❯ ")) {
            Ok(line) => line,
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
        match parse_input(&line, skills) {
            Input::Empty => {}
            Input::Exit => break,
            Input::Help => print_help(skills),
            Input::Clear => {
                agent.clear();
                out.line(out.paint("2", "conversation cleared"));
            }
            Input::ListSkills => print_skills(skills),
            Input::ListMcp => print_mcp(mcp),
            Input::ListAgents => print_agents(agent_defs),
            Input::Model(None) => print_models(agent),
            Input::Model(Some(spec)) => match agent.client().switch(&spec) {
                Ok(client) => {
                    out.line(format!("model: {}", out.paint("1", client.spec())));
                    agent.set_client(client);
                }
                Err(e) => out.line(out.paint("31", &format!("{e:#}"))),
            },
            Input::Error(e) => out.line(out.paint("31", &e)),
            Input::Prompt(prompt) => {
                // The turn's outcome is already shown.
                let _ = submit(agent, &line, &prompt, files, &out, &mut term).await;
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

/// Attaches the files `@`-mentioned in `line` and runs the turn until it
/// ends or Ctrl-C interrupts it. The turn's progress and outcome are shown.
async fn submit(
    agent: &mut Agent,
    line: &str,
    prompt: &str,
    files: &FileTracker,
    out: &Output,
    term: &mut Terminal,
) -> Result<(), TurnEnd> {
    let expanded = mentions::expand(line, files);
    for a in &expanded.attachments {
        out.line(out.paint("2", &format!("  + {}", a.label)));
    }
    for w in &expanded.warnings {
        out.line(out.paint("33", &format!("  ! {w}")));
    }
    term.begin_turn();
    // Dropping the turn cancels it: running commands are killed, and the
    // agent repairs its history at the start of the next turn.
    let result = tokio::select! {
        result = agent.run_turn(expanded.into_content(prompt), &mut *term) => result.map_err(TurnEnd::Failed),
        _ = tokio::signal::ctrl_c() => Err(TurnEnd::Interrupted),
    };
    term.end_turn(&result);
    result
}

#[derive(Debug, PartialEq)]
enum Input {
    Empty,
    Exit,
    Help,
    Clear,
    ListSkills,
    ListMcp,
    ListAgents,
    /// `/model` shows the current model; `/model <spec>` switches.
    Model(Option<String>),
    Prompt(String),
    Error(String),
}

/// Interprets a line of user input. `/<skill> args` loads the skill's
/// instructions directly into the prompt, saving the model a tool call.
fn parse_input(line: &str, skills: &[Skill]) -> Input {
    let Some(command) = line.strip_prefix('/') else {
        return if line.is_empty() { Input::Empty } else { Input::Prompt(line.to_string()) };
    };
    let (name, args) = command.split_once(char::is_whitespace).unwrap_or((command, ""));
    match name {
        "exit" | "quit" => Input::Exit,
        "help" => Input::Help,
        "clear" => Input::Clear,
        "skills" => Input::ListSkills,
        "mcp" => Input::ListMcp,
        "agents" => Input::ListAgents,
        "model" => Input::Model(Some(args.trim()).filter(|a| !a.is_empty()).map(String::from)),
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
    out.line(format!("{} {}", out.paint("1;35", "✻ harness"), out.paint("1", agent.model())));
    out.line(out.paint(
        "2",
        &format!(
            "  {cwd} · {} tools · {} skills · {servers} MCP servers",
            agent.tools().names().count(),
            skills.len()
        ),
    ));
    out.line(out.paint("2", "  /help for commands · @path attaches a file · \\ or Alt+Enter for a new line · Ctrl-C interrupts"));
    out.line("");
}

fn print_help(skills: &[Skill]) {
    let rows = [
        ("/help", "show this help"),
        ("/model [spec]", "show the model, or switch to <provider>/<model>"),
        ("/clear", "start a new conversation"),
        ("/skills", "list available skills"),
        ("/agents", "list subagent types and their tools"),
        ("/mcp", "list MCP servers and their tools"),
        ("/<skill> [args]", "run a skill"),
        ("/exit", "quit (or Ctrl-D)"),
        ("@path", "attach a file, directory, image or PDF (Tab completes; @file#L10-20 for lines)"),
        ("\\ or Alt+Enter", "continue on a new line"),
        ("Ctrl-C", "interrupt the running turn"),
    ];
    for (key, what) in rows {
        println!("  \x1b[1m{key:<16}\x1b[0m {what}");
    }
    if !skills.is_empty() {
        let names: Vec<String> = skills.iter().map(|s| format!("/{}", s.name)).collect();
        println!("  \x1b[2mskills: {}\x1b[0m", names.join(" "));
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
        assert_eq!(parse_input("/clear", &skills), Input::Clear);
        assert_eq!(parse_input("fix it", &skills), Input::Prompt("fix it".into()));
        assert!(matches!(parse_input("/nope", &skills), Input::Error(_)));
        match parse_input("/greet  Bob ", &skills) {
            Input::Prompt(p) => {
                assert!(p.starts_with("The user invoked the `greet` skill:"));
                assert!(p.ends_with("Say hi.\n\nARGUMENTS: Bob"), "{p}");
            }
            other => panic!("{other:?}"),
        }
    }
}
