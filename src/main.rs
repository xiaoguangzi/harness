mod agent;
mod api;
mod tools;

use std::io::{self, BufRead, Write};

use anyhow::Result;
use serde_json::Value;

use agent::{Agent, Observer};
use api::{Client, Config};
use tools::ToolRegistry;

/// Prints agent progress to the terminal.
struct Terminal;

impl Observer for Terminal {
    fn on_text(&mut self, text: &str) {
        println!("{text}");
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
         commands with the `bash` tool and read files with the `read` tool. \
         The working directory is {cwd}. Work step by step, verify your changes, \
         and finish with a short summary of what you did."
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_env()?;
    let mut agent = Agent::new(Client::new(config)?, ToolRegistry::with_defaults(), system_prompt());
    let mut term = Terminal;

    // One-shot mode: `harness "do something"`.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        return agent.run_turn(&args.join(" "), &mut term).await;
    }

    println!("harness ({}) — type a request, or /exit to quit", agent.model());
    let stdin = io::stdin();
    loop {
        print!("\x1b[1m> \x1b[0m");
        io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        match line {
            "" => continue,
            "/exit" | "/quit" => break,
            _ => {
                if let Err(e) = agent.run_turn(line, &mut term).await {
                    eprintln!("\x1b[31merror: {e:#}\x1b[0m");
                }
            }
        }
    }
    Ok(())
}
