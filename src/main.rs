mod agent;
mod cli;
mod llm;
mod mcp;
mod mentions;
mod prompt;
mod sandbox;
mod session;
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
use skills::SkillTool;
use subagent::AgentTool;
use tools::{FileTracker, ToolRegistry};

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (sandbox_flag, args) = sandbox::parse_flag(args);
    sandbox::init(sandbox_flag).await?;
    let web_addr = web::parse_flag(&args)?;
    // The web UI can fix a missing or broken providers config, so it starts
    // without one.
    let providers = match Providers::load() {
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
    let out = cli::Output::stdout();
    let reporter: subagent::Reporter = {
        let out = out.clone();
        Arc::new(move |label: &str, tool: &str, input: &Value| {
            if !web::forward_subagent(label, tool, input) {
                out.subagent_call(label, tool, input);
            }
        })
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

    let system = prompt::main(&registry);
    let mut agent = Agent::new(client, registry, system);
    let result = cli::run(&mut agent, args, &skills, &agent_defs, &mcp, &files, out).await;
    // The agent's tools hold the MCP connections; release them first.
    drop(agent);
    mcp.shutdown().await;
    result
}
