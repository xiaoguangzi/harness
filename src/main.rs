mod agent;
mod llm;
mod mcp;
mod mentions;
mod repl;
mod skills;
mod subagent;
mod tools;
mod util;

use std::io::{self, Write};
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use agent::{Agent, Observer};
use llm::{Client, Providers};
use skills::{Skill, SkillTool};
use subagent::{AgentDef, AgentTool, Permission};
use repl::ReplHelper;
use tools::{FileTracker, ToolRegistry};

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
         `skill` tool, load that skill first and follow it. For open-ended codebase \
         searches, delegate to the `explore` subagent with the `agent` tool, running \
         several in parallel for independent questions. \
         The working directory is {cwd}. Work step by step, verify your changes, \
         and finish with a short summary of what you did."
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = Client::new(Providers::load()?)?;
    let files = FileTracker::default();
    let mut registry = ToolRegistry::with_defaults(files.clone());

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

    let (agent_defs, warnings) = subagent::discover(&subagent::default_roots());
    for w in warnings {
        eprintln!("\x1b[33magent skipped: {w}\x1b[0m");
    }
    let agent_defs = Arc::new(agent_defs);
    let reporter: subagent::Reporter = Arc::new(|label: &str, tool: &str, input: &Value| {
        println!("\x1b[90m    ⎿ [{label}] {tool} {}\x1b[0m", preview(&input.to_string(), 120));
    });
    // Subagents choose from the tools registered so far, so they never get
    // the `agent` tool itself.
    let agent_tool = AgentTool::new(agent_defs.clone(), registry.clone(), client.clone(), reporter);
    for (agent, tool) in agent_tool.unknown_tools() {
        eprintln!("\x1b[33magent `{agent}` lists unknown tool `{tool}`\x1b[0m");
    }
    registry.register(agent_tool);

    let mut agent = Agent::new(client, registry, system_prompt());
    let result = run(&mut agent, &skills, &agent_defs, &mcp, &files).await;
    // The agent's tools hold the MCP connections; release them first.
    drop(agent);
    mcp.shutdown().await;
    result
}

async fn run(
    agent: &mut Agent,
    skills: &[Skill],
    agent_defs: &[AgentDef],
    mcp: &mcp::Manager,
    files: &FileTracker,
) -> Result<()> {
    let mut term = Terminal;

    // One-shot mode: `harness "do something"` or `harness /skill args`.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        let line = args.join(" ");
        return match parse_input(&line, skills) {
            Input::Prompt(prompt) => submit(agent, &line, &prompt, files, &mut term).await,
            Input::Error(e) => Err(anyhow::anyhow!(e)),
            _ => Err(anyhow::anyhow!("REPL commands are only available interactively")),
        };
    }

    println!(
        "harness ({}) — {} tools, {} skills, {} MCP servers. /help for commands, @path to attach files",
        agent.model(),
        agent.tools().names().count(),
        skills.len(),
        mcp.statuses.iter().filter(|s| s.outcome.is_ok()).count(),
    );

    let mut commands: Vec<String> =
        ["/help", "/model", "/skills", "/agents", "/mcp", "/exit"].iter().map(|c| c.to_string()).collect();
    commands.extend(agent.client().providers().listed_models().iter().map(|m| format!("/model {m}")));
    commands.extend(skills.iter().map(|s| format!("/{}", s.name)));
    let config = rustyline::Config::builder()
        .completion_type(rustyline::CompletionType::List)
        .auto_add_history(true)
        .build();
    let mut editor: rustyline::Editor<ReplHelper, rustyline::history::DefaultHistory> =
        rustyline::Editor::with_config(config)?;
    editor.set_helper(Some(ReplHelper::new(commands)));
    let history = history_path();
    if let Some(path) = &history {
        let _ = editor.load_history(path);
    }

    loop {
        // Reading a line blocks; let the runtime move other tasks (such as
        // MCP connections) off this worker meanwhile.
        let line = match tokio::task::block_in_place(|| editor.readline("> ")) {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Interrupted) => continue,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        };
        let line = line.trim();
        match parse_input(line, skills) {
            Input::Empty => {}
            Input::Exit => break,
            Input::Help => print_help(skills),
            Input::ListSkills => print_skills(skills),
            Input::ListMcp => print_mcp(mcp),
            Input::ListAgents => print_agents(agent_defs),
            Input::Model(None) => print_models(agent),
            Input::Model(Some(spec)) => match agent.client().switch(&spec) {
                Ok(client) => {
                    println!("model: {}", client.spec());
                    agent.set_client(client);
                }
                Err(e) => eprintln!("\x1b[31m{e:#}\x1b[0m"),
            },
            Input::Error(e) => eprintln!("\x1b[31m{e}\x1b[0m"),
            Input::Prompt(prompt) => {
                if let Err(e) = submit(agent, line, &prompt, files, &mut term).await {
                    eprintln!("\x1b[31merror: {e:#}\x1b[0m");
                }
            }
        }
    }
    if let Some(path) = &history {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = editor.save_history(path);
    }
    Ok(())
}

fn history_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".harness/history"))
}

/// Attaches the files `@`-mentioned in `line` and runs the turn.
async fn submit(agent: &mut Agent, line: &str, prompt: &str, files: &FileTracker, term: &mut Terminal) -> Result<()> {
    let expanded = mentions::expand(line, files);
    for a in &expanded.attachments {
        println!("\x1b[90m  + {}\x1b[0m", a.label);
    }
    for w in &expanded.warnings {
        eprintln!("\x1b[33m  ! {w}\x1b[0m");
    }
    agent.run_turn(expanded.into_content(prompt), term).await
}

#[derive(Debug, PartialEq)]
enum Input {
    Empty,
    Exit,
    Help,
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
        "skills" => Input::ListSkills,
        "mcp" => Input::ListMcp,
        "agents" => Input::ListAgents,
        "model" => Input::Model(Some(args.trim()).filter(|a| !a.is_empty()).map(String::from)),
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
    println!("/model [spec]   show the model, or switch to <provider>/<model>");
    println!("/agents         list subagent types and their tools");
    println!("/mcp            list MCP servers and their tools");
    println!("/<skill> [args] run a skill");
    println!("@path           attach a file, directory, image or PDF (Tab completes; @file#L10-20 for lines)");
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

fn print_models(agent: &Agent) {
    let providers = agent.client().providers();
    println!("current: \x1b[1m{}\x1b[0m", agent.model());
    if let Some(sub) = &providers.subagent_model {
        println!("subagents: {sub}");
    }
    for name in providers.names() {
        let protocol = match providers.protocol_of(name) {
            Some(llm::Protocol::Anthropic) => "anthropic",
            Some(llm::Protocol::OpenAiResponses) => "openai-responses",
            None => "?",
        };
        println!("provider \x1b[1m{name}\x1b[0m ({protocol})");
    }
    for m in providers.listed_models() {
        println!("  {m}");
    }
    println!("\x1b[90mswitch with /model <provider>/<model>; any model name the gateway accepts works\x1b[0m");
}

fn print_agents(defs: &[AgentDef]) {
    for d in defs {
        let access = match d.permission {
            Permission::ReadOnly => "read-only",
            Permission::Write => "write",
        };
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
