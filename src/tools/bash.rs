use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{Tool, ToolOutput, parse_input, truncate};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_OUTPUT_BYTES: usize = 30_000;

/// Runs a command with `bash -c` in the harness working directory.
pub struct BashTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    command: String,
    timeout_ms: Option<u64>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command with `bash -c` and return its stdout, stderr and exit code. \
         Each call runs in a fresh process started in the working directory, so `cd` and \
         environment changes do not persist between calls. Output longer than 30000 bytes \
         is truncated. Use this for builds, tests, git, searching (grep, find) and other \
         shell work; use the `read`, `write` and `grep` tools for viewing, writing and searching files."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to run."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default 120000, max 600000)."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let timeout_ms = input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS);

        let child = Command::new("bash")
            .arg("-c")
            .arg(&input.command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let child = match child {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("Failed to spawn bash: {e}")),
        };

        let output = match tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            child.wait_with_output(),
        )
        .await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return ToolOutput::error(format!("Failed to run command: {e}")),
            // Dropping the future drops the child, which kills it (kill_on_drop).
            Err(_) => return ToolOutput::error(format!("Command timed out after {timeout_ms} ms")),
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut text = String::new();
        if !stdout.is_empty() {
            text.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str("[stderr]\n");
            text.push_str(&stderr);
        }
        let text = truncate(&text, MAX_OUTPUT_BYTES);

        match output.status.code() {
            Some(0) => ToolOutput::ok(if text.is_empty() { "(no output)".into() } else { text }),
            Some(code) => ToolOutput::error(format!("{text}\n[exit code {code}]")),
            None => ToolOutput::error(format!("{text}\n[terminated by signal]")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_stdout() {
        let out = BashTool.run(json!({"command": "echo hello"})).await;
        assert_eq!(out, ToolOutput::ok("hello\n"));
    }

    #[tokio::test]
    async fn nonzero_exit_is_error() {
        let out = BashTool.run(json!({"command": "echo oops >&2; exit 3"})).await;
        assert!(out.is_error);
        assert!(out.content.contains("[stderr]\noops"));
        assert!(out.content.contains("[exit code 3]"));
    }

    #[tokio::test]
    async fn times_out() {
        let out = BashTool.run(json!({"command": "sleep 5", "timeout_ms": 100})).await;
        assert!(out.is_error);
        assert!(out.content.contains("timed out"));
    }

    #[tokio::test]
    async fn rejects_bad_input() {
        let out = BashTool.run(json!({"command": "ls", "extra": 1})).await;
        assert!(out.is_error);
        assert!(out.content.starts_with("Invalid input"));
    }
}
