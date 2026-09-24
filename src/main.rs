mod agent;
mod api;
mod mcp;
mod skills;
mod tools;

use std::io::{self, BufRead, Write};
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use agent::{Agent, Observer};
use api::{Client, Config};
use skills::{Skill, SkillTool};
use tools::ToolRegistry;

/// Prints agent progress to the terminal.
struct Terminal;

impl Observer for Terminal {
    fn on_text_delta(&mut self, text: &str) {
        print!("{text}");
        let _ = io::stdout().flush();
    }

    fn on_text_end(&mut self) {
        println!();
    }

    fn on_tool_call(&mut self, name: &str, input: &Value) {
        println!("\x1b[36m● {name}\x1b[0m {}", preview(&input.to_string(), 200));
    }

    fn on_tool_result(&mut self, name: &str, content: &str, is_error: bool) {
        let color = if is_error { "31" } else { "90" };
        println!("\x1b[{color}m  ⎿ {name}: {}\x1b[0m", preview(content, 300));
    }
}

fn preview(s: &str, max_chars: usize) -> String {
    let flat = s.replace('\n', " ⏎ ");
    match flat.char_indices().nth(max_chars) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}

fn system_prompt() -> String {
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    format!(
        "You are a coding agent running in a terminal harness. You can run shell \
         commands with `bash`, search file contents with `grep`, read files with `read` \
         and create or overwrite files with `write`. Tools named `mcp__<server>__<tool>` \
         come from connected MCP servers. When a request matches a skill listed in the \
         `skill` tool, load that skill first and follow it. \
         The working directory is {cwd}. Work step by step, verify your changes, \
         and finish with a short summary of what you did."
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_env()?;
    let mut registry = ToolRegistry::with_defaults();

    let (skills, warnings) = skills::discover(&skills::default_roots());
    for w in warnings {
        eprintln!("\x1b[33mskill skipped: {w}\x1b[0m");
    }
    let skills = Arc::new(skills);
    if !skills.is_empty() {
        registry.register(SkillTool::new(skills.clone()));
    }

    let mcp_configs = mcp::load_config().unwrap_or_else(|e| {
        eprintln!("\x1b[33mMCP config ignored: {e:#}\x1b[0m");
        Default::default()
    });
    let mcp = mcp::Manager::start(&mcp_configs, &mut registry).await;
    for status in &mcp.statuses {
        if let Err(e) = &status.outcome {
            eprintln!("\x1b[33mMCP server `{}` failed: {e}\x1b[0m", status.name);
        }
    }

    let mut agent = Agent::new(Client::new(config)?, registry, system_prompt());
    let result = run(&mut agent, &skills, &mcp).await;
    // The agent's tools hold the MCP connections; release them first.
    drop(agent);
    mcp.shutdown().await;
    result
}

async fn run(agent: &mut Agent, skills: &[Skill], mcp: &mcp::Manager) -> Result<()> {
    let mut term = Terminal;

    // One-shot mode: `harness "do something"` or `harness /skill args`.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        return match parse_input(&args.join(" "), skills) {
            Input::Prompt(prompt) => agent.run_turn(&prompt, &mut term).await,
            Input::Error(e) => Err(anyhow::anyhow!(e)),
            _ => Err(anyhow::anyhow!("REPL commands are only available interactively")),
        };
    }

    println!(
        "harness ({}) — {} tools, {} skills, {} MCP servers. /help for commands",
        agent.model(),
        agent.tools().names().count(),
        skills.len(),
        mcp.statuses.iter().filter(|s| s.outcome.is_ok()).count(),
    );
    let stdin = io::stdin();
    loop {
        print!("\x1b[1m> \x1b[0m");
        io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        match parse_input(line.trim(), skills) {
            Input::Empty => {}
            Input::Exit => break,
            Input::Help => print_help(skills),
            Input::ListSkills => print_skills(skills),
            Input::ListMcp => print_mcp(mcp),
            Input::Error(e) => eprintln!("\x1b[31m{e}\x1b[0m"),
            Input::Prompt(prompt) => {
                if let Err(e) = agent.run_turn(&prompt, &mut term).await {
                    eprintln!("\x1b[31merror: {e:#}\x1b[0m");
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
enum Input {
    Empty,
    Exit,
    Help,
    ListSkills,
    ListMcp,
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
        "skills" => Input::ListSkills,
        "mcp" => Input::ListMcp,
        _ => match skills.iter().find(|s| s.name == name) {
            Some(skill) => match skill.load(Some(args)) {
                Ok(body) => Input::Prompt(format!("The user invoked the `{name}` skill:\n\n{body}")),
                Err(e) => Input::Error(format!("cannot load skill {name}: {e}")),
            },
            None => Input::Error(format!("unknown command /{name} (try /help)")),
        },
    }
}

fn print_help(skills: &[Skill]) {
    println!("/help           show this help");
    println!("/skills         list available skills");
    println!("/mcp            list MCP servers and their tools");
    println!("/<skill> [args] run a skill");
    println!("/exit           quit");
    if !skills.is_empty() {
        let names: Vec<String> = skills.iter().map(|s| format!("/{}", s.name)).collect();
        println!("skills: {}", names.join(" "));
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

    #[test]
    fn parses_commands() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("greet")).unwrap();
        std::fs::write(dir.path().join("greet/SKILL.md"), "---\ndescription: d\n---\nSay hi.\n").unwrap();
        let (skills, _) = skills::discover(&[dir.path().into()]);

        assert_eq!(parse_input("", &skills), Input::Empty);
        assert_eq!(parse_input("/exit", &skills), Input::Exit);
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
