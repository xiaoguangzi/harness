mod agent;
mod checkpoint;
mod cli;
mod hooks;
mod llm;
mod lsp;
mod mcp;
mod mentions;
mod plugins;
mod prompt;
mod sandbox;
mod session;
mod settings;
mod setup;
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
use setup::{Env, Extensions};

const USAGE: &str = "\
Usage: harness [OPTIONS] [PROMPT...]
       harness plugin <command>   Install and manage plugins (harness plugin help)
       harness provider <command> Add LLM gateways and models (harness provider help)

With no PROMPT, starts the interactive REPL; with one, runs it and exits.

Options:
  -c, --continue      Continue this directory's latest conversation
  -r, --resume [ID]   Resume a conversation (pick from a list without ID)
      --web[=ADDR]    Start the web UI instead (default 127.0.0.1:8787)
      --sandbox       Run the model's commands in an OS sandbox
      --no-sandbox    Turn the sandbox off even if configured
      --settings FILE Read settings from FILE instead
      --plugin-dir DIR Load the plugin in DIR as well (repeatable)
      --check-plugins Load the plugins, report what loaded and what not, and exit
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
    if matches!(args.first().map(String::as_str), Some("plugin" | "plugins")) {
        let lines = tokio::task::block_in_place(|| plugins::manage::run(&args[1..], &mut plugins::manage::TerminalAsk))?;
        for line in lines {
            println!("{line}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    if matches!(args.first().map(String::as_str), Some("provider" | "providers")) {
        let lines = llm::manage::run(&args[1..], &mut plugins::manage::TerminalAsk)?;
        for line in lines {
            println!("{line}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    let (plugin_dirs, args) = plugins::parse_flag(args)?;
    let mut settings = Settings::load()?;
    for file in settings::legacy_files() {
        eprintln!("\x1b[33m{} is no longer read; move its settings into ~/.harness/settings.json\x1b[0m", file.display());
    }
    if args.iter().any(|a| a == "--check-plugins") {
        if args.len() > 1 {
            anyhow::bail!("--check-plugins takes no other arguments");
        }
        return Ok(setup::check_plugins(&settings, &plugin_dirs).await);
    }
    tools::bash::hide_env(settings.credential_vars());
    let (sandbox_flag, args) = sandbox::parse_flag(args);
    sandbox::init(sandbox_flag, &settings.sandbox).await?;
    // MCP servers connect in the background: the REPL waits for them only
    // once the first prompt is in.
    let user_mcp = mcp::Servers::connect(&settings.mcp_servers, &[]);
    let mut ext = Extensions::load(&mut settings, &plugin_dirs, &[]);
    for w in &ext.warnings {
        eprintln!("\x1b[33m{w}\x1b[0m");
    }
    ext.activate();
    setup::auto_update(std::mem::take(&mut ext.auto_update));
    let web_addr = web::parse_flag(&args)?;
    // The web UI can fix a missing or broken provider setup, so it starts
    // without one.
    let providers = match Providers::from_settings(&settings) {
        Err(e) if web_addr.is_none() => return Err(e),
        loaded => loaded,
    };
    let out = cli::Output::stdout();
    // Web tasks show their subagents' progress themselves; this is for the
    // terminal, and for subagents started from the web UI's tool page.
    let reporter: subagent::Reporter = {
        let out = out.clone();
        Arc::new(move |event: &Value| out.subagent_event(event))
    };
    let mut env = Env::new(ext, user_mcp, reporter, plugin_dirs);

    if let Some(addr) = web_addr {
        for w in env.wait_mcp().await {
            eprintln!("\x1b[33m{w}\x1b[0m");
        }
        let mcp_status = env
            .mcp_statuses()
            .map(|s| match &s.outcome {
                Ok(tools) => serde_json::json!({"name": s.name, "tools": tools, "connect_ms": s.took.as_millis() as u64}),
                Err(e) => serde_json::json!({"name": s.name, "error": e, "connect_ms": s.took.as_millis() as u64}),
            })
            .collect();
        let setup = web::Setup {
            providers,
            tools: env.base_tools(),
            skills: env.ext.skills.clone(),
            agent_defs: env.ext.agent_defs.clone(),
            mcp: Value::Array(mcp_status),
            plugins: env.ext.plugins.to_json(),
            reporter: env.reporter.clone(),
            forks: env.forks.clone(),
        };
        let result = web::serve(&addr, setup).await;
        env.shutdown().await;
        return result.map(|()| ExitCode::SUCCESS);
    }

    let client = Client::new(providers?)?;
    // With MCP servers still connecting, the REPL checks once they are in.
    if env.mcp_connecting().is_none() {
        for w in env.unknown_agent_tools(&client) {
            eprintln!("\x1b[33m{w}\x1b[0m");
        }
    }
    let (registry, system) = env.conversation(&client);
    let mut agent = Agent::new(client, registry, system);
    let result = cli::run(&mut agent, args, &mut env, out).await;
    // The agent's tools hold the MCP connections; release them first.
    drop(agent);
    env.shutdown().await;
    result
}
