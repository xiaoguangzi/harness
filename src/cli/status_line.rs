//! The status line under the input box: `statusLine` in the settings, in
//! Claude Code's format. Its command runs, outside the sandbox like a hook,
//! each time the prompt opens, with the session as JSON on stdin; the first
//! line it prints is shown.

use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use super::output::Output;
use crate::agent::Agent;

/// How long the command may take.
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(rename = "type")]
    _kind: Kind,
    command: String,
    /// Claude Code's spacing around the line; unused.
    #[serde(rename = "padding")]
    _padding: Option<u16>,
}

#[derive(Debug, Clone, Deserialize)]
enum Kind {
    #[serde(rename = "command")]
    Command,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Shows the status line `config` sets from now on.
pub fn set(config: Option<Config>) {
    if let Some(config) = config {
        let _ = CONFIG.set(config);
    }
}

/// Runs the status line's command in the background, for the
/// conversation `agent` holds, saved in `transcript` as `session`, and
/// shows what it prints under the input box.
pub fn refresh(agent: &Agent, session: Option<(&str, std::path::PathBuf)>, out: &Output) {
    let Some(config) = CONFIG.get() else { return };
    let input = input(agent, session);
    let (command, out) = (config.command.clone(), out.clone());
    tokio::spawn(async move {
        let line = run(&command, &input).await;
        out.set_footer(line);
    });
}

/// What the command gets on stdin.
fn input(agent: &Agent, session: Option<(&str, std::path::PathBuf)>) -> Value {
    let cwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
    let spec = agent.model();
    let window = agent.context_window();
    let used = agent.context_tokens();
    let totals = agent.usage().models().into_iter().fold((0, 0), |(input, output), (_, t)| {
        (input + t.usage.input_tokens, output + t.usage.output_tokens)
    });
    let (id, transcript) = session.map_or((None, None), |(id, path)| (Some(id.to_string()), Some(path.display().to_string())));
    json!({
        "hook_event_name": "Status",
        "session_id": id,
        "transcript_path": transcript,
        "cwd": cwd,
        "model": {"id": spec, "display_name": spec.split_once('/').map_or(spec, |(_, model)| model)},
        "workspace": {"current_dir": cwd, "project_dir": cwd},
        "version": env!("CARGO_PKG_VERSION"),
        "output_style": {"name": "default"},
        "effort": agent.client().effort().as_str(),
        "context_window": {
            "context_window_size": window,
            "used_tokens": used,
            "used_percentage": used.saturating_mul(100) / window.max(1),
        },
        "cost": {"total_input_tokens": totals.0, "total_output_tokens": totals.1},
    })
}

/// The first line `command` prints with `input` on stdin, if it does so in
/// time and succeeds.
async fn run(command: &str, input: &Value) -> Option<String> {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .env("CLAUDE_PROJECT_DIR", std::env::current_dir().unwrap_or_default())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    // A line, for scripts that `read` it.
    let text = format!("{input}\n");
    let write = async move {
        let _ = stdin.write_all(text.as_bytes()).await;
    };
    let output = tokio::time::timeout(TIMEOUT, async { tokio::join!(write, child.wait_with_output()).1 }).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).lines().map(str::trim_end).find(|l| !l.trim().is_empty()).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_code_s_format() {
        let config: Config = serde_json::from_value(json!({"type": "command", "command": "echo hi", "padding": 0})).unwrap();
        assert_eq!(config.command, "echo hi");
        assert!(serde_json::from_value::<Config>(json!({"type": "static", "command": "x"})).is_err());
        assert!(serde_json::from_value::<Config>(json!({"type": "command", "command": "x", "color": 1})).is_err());
    }

    #[tokio::test]
    async fn shows_the_first_line_the_command_prints() {
        let input = json!({"model": {"display_name": "m"}});
        let line = run(r#"sed -n 's/.*"display_name":"\([^"]*\)".*/model \1/p'; echo second"#, &input).await;
        assert_eq!(line.as_deref(), Some("model m"));
        assert_eq!(run("exit 1", &input).await, None, "a failing command shows nothing");
        assert_eq!(run("printf '\\n\\n'", &input).await, None);
    }
}
