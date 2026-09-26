mod agent;
mod cli;
mod llm;
mod mcp;
mod mentions;
mod prompt;
mod sandbox;
mod session;
mod settings;
mod skills;
mod subagent;
mod tools;
mod util;
mod web;

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use agent::Agent;
use llm::{Client, Providers};
use settings::Settings;
use skills::SkillTool;
use subagent::AgentTool;
use tools::{FileTracker, ToolRegistry};

const USAGE: &str = "\
Usage: harness [OPTIONS] [PROMPT...]

With no PROMPT, starts the interactive REPL; with one, runs it and exits.

Options:
  -c, --continue      Continue this directory's latest conversation
  -r, --resume [ID]   Resume a conversation (pick from a list without ID)
      --web[=ADDR]    Start the web UI instead (default 127.0.0.1:8787)
      --sandbox       Run the model's commands in an OS sandbox
      --no-sandbox    Turn the sandbox off even if configured
      --settings FILE Read settings from FILE instead
  -h, --help          Print this help
  -V, --version       Print the version

Settings: ~/.harness/settings.json (or $HARNESS_SETTINGS)
";

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("-h" | "--help")) {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    if matches!(args.first().map(String::as_str), Some("-V" | "--version")) {
        println!("harness {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }
    let (settings_path, args) = settings::parse_flag(args)?;
    settings::set_path(settings_path);
    let settings = Settings::load()?;
    for file in settings::legacy_files() {
        eprintln!("\x1b[33m{} is no longer read; move its settings into ~/.harness/settings.json\x1b[0m", file.display());
    }
    let (sandbox_flag, args) = sandbox::parse_flag(args);
    sandbox::init(sandbox_flag, &settings.sandbox).await?;
    let web_addr = web::parse_flag(&args)?;
    // The web UI can fix a missing or broken provider setup, so it starts
    // without one.
    let providers = match Providers::from_settings(&settings) {
        Err(e) if web_addr.is_none() => return Err(e),
        loaded => loaded,
    };
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

    let mcp = mcp::Manager::start(&settings.mcp_servers, &mut registry).await;
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
    let out = cli::Output::stdout();
    // Web tasks show their subagents' progress themselves; this is for the
    // terminal, and for subagents started from the web UI's tool page.
    let reporter: subagent::Reporter = {
        let out = out.clone();
        Arc::new(move |event: &Value| out.subagent_event(event))
    };

    if let Some(addr) = web_addr {
        let mcp_status = mcp
            .statuses
            .iter()
            .map(|s| match &s.outcome {
                Ok(tools) => serde_json::json!({"name": s.name, "tools": tools}),
                Err(e) => serde_json::json!({"name": s.name, "error": e}),
            })
            .collect();
        let setup = web::Setup {
            providers,
            tools: registry,
            skills,
            agent_defs,
            mcp: Value::Array(mcp_status),
            reporter,
        };
        let result = web::serve(&addr, setup).await;
        mcp.shutdown().await;
        return result.map(|()| ExitCode::SUCCESS);
    }

    let client = Client::new(providers?)?;
    // Subagents choose from the tools registered so far, so they never get
    // the `agent` tool itself.
    let agent_tool = AgentTool::new(agent_defs.clone(), registry.clone(), client.clone(), reporter);
    for (agent, tool) in agent_tool.unknown_tools() {
        eprintln!("\x1b[33magent `{agent}` lists unknown tool `{tool}`\x1b[0m");
    }
    registry.register(agent_tool);
    registry.register(subagent::MessageAgentTool);
    registry.register(subagent::StopAgentTool);

    let system = prompt::main(&registry);
    let mut agent = Agent::new(client, registry, system);
    let result = cli::run(&mut agent, args, &skills, &agent_defs, &mcp, &files, out).await;
    // The agent's tools hold the MCP connections; release them first.
    drop(agent);
    mcp.shutdown().await;
    result
}
