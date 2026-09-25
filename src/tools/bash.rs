use std::pin::pin;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::{Tool, ToolOutput, parse_input, truncate_counting};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_OUTPUT_BYTES: usize = 30_000;
/// How long to keep reading output once bash has exited.
const PIPE_GRACE: Duration = Duration::from_millis(200);

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
         is truncated. Use this for builds, tests, git, listing files (ls, find) and other \
         shell work. Do not use it to read, write or search file contents: no `cat`, `head`, \
         `tail`, `sed`, `awk`, `echo >`, heredocs, `grep` or `rg` for that. Use the `read` tool \
         to view files, `write` to create or change them, and the `grep` tool to search."
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
            // Its own process group, so its children can be killed with it.
            .process_group(0)
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("Failed to spawn bash: {e}")),
        };
        // On a timeout, or when this call is dropped (the turn was
        // cancelled), everything the command started goes too.
        let mut group = KillGroup(child.id());
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let (mut out, mut err) = (Capture::default(), Capture::default());

        let waited = {
            let mut reading = pin!(async { tokio::join!(out.fill(stdout), err.fill(stderr)) });
            let mut read_done = false;
            let waited = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
                loop {
                    tokio::select! {
                        _ = &mut reading, if !read_done => read_done = true,
                        status = child.wait() => {
                            // Jobs it left running in the background are
                            // the command's intent; leave them be.
                            if status.is_ok() {
                                group.0 = None;
                            }
                            break status;
                        }
                    }
                }
            })
            .await;
            // A background job can hold the pipes open after bash exits, so
            // only wait a moment for the rest of the output.
            if waited.is_ok() && !read_done {
                let _ = tokio::time::timeout(PIPE_GRACE, reading).await;
            }
            waited
        };

        let mut text = String::from_utf8_lossy(&out.kept).into_owned();
        if !err.kept.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str("[stderr]\n");
            text.push_str(&String::from_utf8_lossy(&err.kept));
        }
        let text = truncate_counting(&text, MAX_OUTPUT_BYTES, out.dropped + err.dropped);

        let status = match waited {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => return ToolOutput::error(format!("Failed to run command: {e}")),
            Err(_) if text.is_empty() => return ToolOutput::error(format!("Command timed out after {timeout_ms} ms")),
            Err(_) => return ToolOutput::error(format!("{text}\n[command timed out after {timeout_ms} ms]")),
        };
        match status.code() {
            Some(0) => ToolOutput::ok(if text.is_empty() { "(no output)".into() } else { text }),
            Some(code) => ToolOutput::error(format!("{text}\n[exit code {code}]")),
            None => ToolOutput::error(format!("{text}\n[terminated by signal]")),
        }
    }
}

/// The first `MAX_OUTPUT_BYTES` of a stream, and how many bytes came after.
#[derive(Default)]
struct Capture {
    kept: Vec<u8>,
    dropped: usize,
}

impl Capture {
    /// Reads to the end, only counting what is past the cap: the stream is
    /// still drained, so the command never blocks on a full pipe.
    async fn fill(&mut self, stream: Option<impl AsyncRead + Unpin>) {
        let Some(mut stream) = stream else { return };
        let mut buf = [0u8; 8192];
        loop {
            let n = match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let keep = n.min(MAX_OUTPUT_BYTES - self.kept.len());
            self.kept.extend_from_slice(&buf[..keep]);
            self.dropped += n - keep;
        }
    }
}

/// Kills a process group with SIGKILL when dropped, unless its id was
/// cleared.
struct KillGroup(Option<u32>);

impl Drop for KillGroup {
    fn drop(&mut self) {
        if let Some(id) = self.0 {
            // SAFETY: killpg only sends a signal.
            unsafe { libc::killpg(id as libc::pid_t, libc::SIGKILL) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the tool in the harness's working directory, which the
    /// `mentions` tests change for a moment. Each test has its own
    /// single-threaded runtime, so holding the lock across the await is fine.
    #[allow(clippy::await_holding_lock)]
    async fn run(input: Value) -> ToolOutput {
        let _guard = crate::util::lock_cwd();
        BashTool.run(input).await
    }

    #[tokio::test]
    async fn captures_stdout() {
        let out = run(json!({"command": "echo hello"})).await;
        assert_eq!(out, ToolOutput::ok("hello\n"));
    }

    #[tokio::test]
    async fn nonzero_exit_is_error() {
        let out = run(json!({"command": "echo oops >&2; exit 3"})).await;
        assert!(out.is_error);
        assert!(out.content.contains("[stderr]\noops"));
        assert!(out.content.contains("[exit code 3]"));
    }

    #[tokio::test]
    async fn times_out() {
        let out = run(json!({"command": "sleep 5", "timeout_ms": 100})).await;
        assert!(out.is_error);
        assert!(out.content.contains("timed out"));
    }

    fn alive(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[tokio::test]
    async fn timeout_kills_child_processes_and_keeps_output() {
        let out = run(json!({"command": "sleep 30 & echo $!; wait", "timeout_ms": 300})).await;
        assert!(out.is_error);
        assert!(out.content.ends_with("[command timed out after 300 ms]"), "{}", out.content);
        let pid: libc::pid_t = out.content.lines().next().unwrap().parse().unwrap();
        // The orphaned `sleep` is reaped by init shortly after the kill.
        for _ in 0..50 {
            if !alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("sleep {pid} survived the timeout");
    }

    #[tokio::test]
    async fn background_job_does_not_hold_the_call() {
        let started = std::time::Instant::now();
        let out = run(json!({"command": "sleep 5 & echo $!"})).await;
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid: libc::pid_t = out.content.trim().parse().unwrap();
        assert!(alive(pid), "a finished command leaves its background jobs running");
        // SAFETY: only signals the test's own `sleep`.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    #[tokio::test]
    async fn caps_output_while_reading() {
        let out = run(json!({"command": "head -c 1000000 /dev/zero | tr '\\0' a"})).await;
        assert!(!out.is_error);
        assert_eq!(out.content, format!("{}\n... [truncated 970000 bytes]", "a".repeat(MAX_OUTPUT_BYTES)));
    }

    #[tokio::test]
    async fn rejects_bad_input() {
        let out = run(json!({"command": "ls", "extra": 1})).await;
        assert!(out.is_error);
        assert!(out.content.starts_with("Invalid input"));
    }
}
