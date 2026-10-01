use std::pin::pin;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

use super::{Tool, ToolOutput, parse_input, truncate_counting};
use crate::sandbox;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_OUTPUT_BYTES: usize = 30_000;
/// How long to keep reading output once bash has exited.
pub(super) const PIPE_GRACE: Duration = Duration::from_millis(200);

/// Runs a command with `bash -c` in the harness working directory, in the
/// sandbox when it is on.
pub struct BashTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    command: String,
    timeout_ms: Option<u64>,
    #[serde(default)]
    run_in_background: bool,
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
         `tail`, `sed`, `awk`, `echo >`, heredocs, `grep` or `rg` for that, and no Python, \
         Node or Perl scripts that change files. Use the `read` tool to view files, `edit` to \
         change them, `write` to create them, and the `grep` tool to search. Commands that \
         change files themselves (inline scripts that write files, `sed -i`, heredocs or \
         `echo` redirected into a file) are refused. With `run_in_background`, the command \
         keeps running (dev servers, watchers, long builds) and the call returns a shell id \
         at once: read its output with `bash_output`, stop it with `kill_shell`."
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
                    "description": "Timeout in milliseconds (default 120000, max 600000); not for background commands."
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Run the command in the background and return its shell id at once (default false)."
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
        if let Some(how) = changes_files(&input.command) {
            return ToolOutput::error(format!(
                "Refused by harness (a rule of the bash tool, not the sandbox): this command {how}. \
                 Change files with the `edit` tool, or `write` for a new file or a complete rewrite: \
                 they check that you read the file and show the user the change. Use bash for builds, \
                 tests, git and running programs."
            ));
        }
        if input.run_in_background {
            let Some(shells) = super::shells::current() else {
                return ToolOutput::error("Background commands only run within a conversation");
            };
            return match shells.start(&input.command) {
                Ok(id) => ToolOutput::ok(format!(
                    "Started in the background as {id}. Read its output with bash_output, and stop it with \
                     kill_shell. You will be told when it exits; don't poll it in a loop."
                )),
                Err(e) => ToolOutput::error(e),
            };
        }
        let timeout_ms = input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS);

        let (mut child, _sandboxed) = match spawn(&input.command) {
            Ok(spawned) => spawned,
            Err(e) => return ToolOutput::error(e),
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

/// Starts `bash -c command` with its output piped, in the sandbox when it
/// is on, in a process group of its own so its children can be killed with
/// it. Keep the guard until the process exits.
pub(super) fn spawn(command: &str) -> Result<(Child, Option<sandbox::Guard>), String> {
    let (mut cmd, guard) = match sandbox::active() {
        Some(sandbox) => match sandbox.command("bash", ["-c", command]) {
            Ok((cmd, guard)) => (cmd, Some(guard)),
            Err(e) => return Err(format!("Failed to spawn bash: {e}")),
        },
        None => {
            let mut cmd = Command::new("bash");
            cmd.arg("-c").arg(command).process_group(0);
            (cmd, None)
        }
    };
    let child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn();
    child.map(|child| (child, guard)).map_err(|e| format!("Failed to spawn bash: {e}"))
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
pub(crate) struct KillGroup(pub(crate) Option<u32>);

impl Drop for KillGroup {
    fn drop(&mut self) {
        if let Some(id) = self.0 {
            // SAFETY: killpg only sends a signal.
            unsafe { libc::killpg(id as libc::pid_t, libc::SIGKILL) };
        }
    }
}

/// How `command` changes files itself, if it does, which the model must do
/// with `edit` or `write` instead: an inline script that writes files, an
/// in-place `sed`/`perl`/`ruby`, or `echo`/`printf`/a heredoc sent into a
/// file. Commands that run programs (builds, script files) pass.
fn changes_files(command: &str) -> Option<String> {
    let shell = shell_words_only(command);
    let words: Vec<&str> = shell.split(|c: char| c.is_whitespace() || ";|&()`".contains(c)).filter(|w| !w.is_empty()).collect();
    for (i, word) in words.iter().enumerate() {
        let name = word.rsplit('/').next().unwrap_or(word);
        let args = &words[i + 1..];
        if matches!(name, "sed" | "gsed" | "perl" | "ruby")
            && let Some(flag) = args.iter().take_while(|a| a.starts_with('-')).find(|a| in_place(a))
        {
            return Some(format!("edits files in place (`{name} {flag}`)"));
        }
        if interpreter(name) && inline(args) && writes_files(command) {
            return Some(format!("writes files from an inline `{name}` script"));
        }
    }
    shell.lines().flat_map(|line| line.split([';', '&'])).find_map(sent_into_file)
}

/// `command` with quoted text and heredoc bodies blanked out, leaving the
/// words the shell itself acts on: `git commit -m "sed -i"` runs no `sed`.
fn shell_words_only(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut chars = command.chars().peekable();
    let mut quote = None;
    // Delimiters of heredocs whose bodies start after the current line.
    let mut heredocs: Vec<String> = Vec::new();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' {
                chars.next();
            }
            out.push(' ');
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                out.push(' ');
            }
            '\\' => {
                chars.next();
                out.push_str("  ");
            }
            '<' if chars.peek() == Some(&'<') => {
                chars.next();
                out.push_str("<<");
                if chars.peek() == Some(&'<') {
                    continue; // a here-string
                }
                while chars.next_if(|&c| c == '-' || c == ' ' || c == '\t').is_some() {}
                let mut delimiter = String::new();
                while let Some(c) = chars.next_if(|c| !c.is_whitespace() && !";|&<>()".contains(*c)) {
                    if !matches!(c, '\'' | '"' | '\\') {
                        delimiter.push(c);
                    }
                }
                out.push_str(&delimiter);
                heredocs.push(delimiter);
            }
            '\n' => {
                out.push('\n');
                for delimiter in heredocs.drain(..) {
                    loop {
                        let line: String = chars.by_ref().take_while(|&c| c != '\n').collect();
                        out.push('\n');
                        if line.trim() == delimiter || chars.peek().is_none() {
                            break;
                        }
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// An option that makes `sed`, `perl` or `ruby` edit files in place:
/// `-i`, `-i.bak`, `-pi`, `--in-place`.
fn in_place(option: &str) -> bool {
    if option.starts_with("--in-place") {
        return true;
    }
    let Some(flags) = option.strip_prefix('-').filter(|f| !f.starts_with('-')) else { return false };
    // Up to a backup suffix; `-I`/`-M` take a path or module name instead.
    let flags = flags.split('.').next().unwrap_or("");
    !flags.starts_with(['I', 'M', 'm']) && flags.chars().all(|c| c.is_ascii_alphanumeric()) && flags.contains('i')
}

fn interpreter(name: &str) -> bool {
    matches!(name, "node" | "nodejs" | "perl" | "ruby" | "php")
        || name.strip_prefix("python").is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

/// Whether an interpreter's arguments give it a script inline (`-c`, `-e`,
/// `-ne`) or on stdin from a heredoc, rather than a script file.
fn inline(args: &[&str]) -> bool {
    args.iter()
        .take_while(|a| a.starts_with(['-', '<']))
        .any(|a| *a == "-" || a.starts_with("<<") || (!a.starts_with("--") && a.len() > 1 && a.ends_with(['c', 'e'])))
}

/// Whether script text writes files: `open(p, 'w')`, `Path.write_text`,
/// `fs.writeFileSync` and the like.
fn writes_files(script: &str) -> bool {
    const CALLS: [&str; 9] = [
        ".write_text(",
        ".write_bytes(",
        "writeFileSync",
        "appendFileSync",
        "createWriteStream",
        "fs.writeFile",
        "File.write(",
        "IO.write(",
        "inplace=True",
    ];
    let script = script.replace('\\', "");
    CALLS.iter().any(|c| script.contains(c))
        || script.match_indices("open(").any(|(at, _)| opens_for_writing(script[..at].ends_with('.'), &script[at + 5..]))
}

/// Whether the arguments of an `open(` call name a write mode: `'w'`, `"ab"`,
/// `'>'` after a comma or `=`, or first for a method (`path.open('w')`).
fn opens_for_writing(method: bool, args: &str) -> bool {
    let mut depth = 0;
    let end = args
        .char_indices()
        .find(|&(_, c)| {
            match c {
                '(' => depth += 1,
                ')' if depth == 0 => return true,
                ')' => depth -= 1,
                _ => {}
            }
            false
        })
        .map_or(args.len(), |(i, _)| i);
    let args = &args[..end];
    let mut rest = args;
    while let Some(start) = rest.find(['\'', '"']) {
        let quote = &rest[start..start + 1];
        let Some(len) = rest[start + 1..].find(quote) else { break };
        let literal = &rest[start + 1..start + 1 + len];
        let before = rest[..start].trim_end();
        let positioned = before.ends_with([',', '=']) || (method && before.is_empty() && rest.len() == args.len());
        let mode = literal.starts_with('>')
            || literal.starts_with("+<")
            || (literal.len() <= 3
                && literal.chars().all(|c| "rwaxbt+".contains(c))
                && literal.contains(['w', 'a', 'x', '+']));
        if positioned && mode {
            return true;
        }
        rest = &rest[start + len + 2..];
    }
    false
}

/// How one shell command (a pipeline, quotes blanked) sends text into a
/// file with `echo`/`printf`, or a heredoc through `cat`/`tee`.
fn sent_into_file(pipeline: &str) -> Option<String> {
    let stages: Vec<Vec<&str>> = pipeline.split('|').map(|s| s.split_whitespace().collect()).collect();
    let first = stages.first()?;
    let name = first.iter().find(|w| !w.contains('='))?.rsplit('/').next()?;
    let heredoc = first.iter().any(|w| w.starts_with("<<"));
    if !(matches!(name, "echo" | "printf") || (name == "cat" && heredoc)) {
        return None;
    }
    let first_stage = pipeline.split('|').next().unwrap_or("");
    if redirects_into_file(first_stage) {
        return Some(format!("writes a file with `{name}` and `>`"));
    }
    let tee = stages[1..].iter().any(|s| s.iter().skip_while(|w| **w != "tee").skip(1).any(|a| !a.starts_with('-') && !a.starts_with("/dev/")));
    tee.then(|| format!("writes a file with `{name}` and `tee`"))
}

/// Whether a command redirects output into a file, not a descriptor or
/// `/dev/null`.
fn redirects_into_file(command: &str) -> bool {
    command.match_indices('>').any(|(at, _)| {
        let target = command[at + 1..].trim_start_matches(['>', '|']).trim_start();
        !target.is_empty() && !target.starts_with('&') && !target.starts_with("/dev/") && !command[..at].ends_with('>')
    })
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

    #[test]
    fn refuses_commands_that_change_files() {
        let refused = [
            "sed -i 's/a/b/' src/main.rs",
            "sed -E -i.bak 's/a|b/c/' f.txt",
            "gsed --in-place 's/a/b/' f",
            "perl -pi -e 's/a/b/' f",
            "cd src && python3 -c \"open('f.txt', 'w').write('x')\"",
            "python -c \"import pathlib; pathlib.Path('x').write_text('y')\"",
            "python3 -c \"p.open('w').write('x')\"",
            "python3 -c 'open(\"f\", mode=\"a\").write(\"x\")'",
            "python3 - <<'EOF'\nfrom pathlib import Path\np = Path('a.rs')\np.write_text(p.read_text().replace('a', 'b'))\nEOF",
            "python3 <<EOF\nwith open(\"x\", \"w\") as f:\n    f.write(\"y\")\nEOF",
            "node -e \"require('fs').writeFileSync('a', 'b')\"",
            "cat > src/x.rs <<'EOF'\nfn main() {}\nEOF",
            "cat <<EOF >> notes.md\nhi\nEOF",
            "cat <<'EOF' | tee src/x.rs\nfn main() {}\nEOF",
            "echo 'x = 1' > config.py",
            "ls && printf 'a\\n' >> list.txt",
        ];
        for command in refused {
            assert!(changes_files(command).is_some(), "not refused: {command}");
        }
        let allowed = [
            "cargo test 2>&1 | tail -5",
            "cargo build > build.log 2>&1; echo done",
            "python3 -c \"import json, sys; print(json.load(sys.stdin)['a'])\"",
            "python3 -c \"import sys; sys.stdout.write(open('README.md').read()[:100])\"",
            "python3 scripts/gen.py --out src/gen.rs",
            "python3 -m pytest -x",
            "sed -n '1,10p' f.txt",
            "perl -ne 'print if /x/' f.txt",
            "perl -Ilib -e 'print 1'",
            "ls > /dev/null 2>&1",
            "echo done >&2",
            "echo \"a > b\"",
            "git commit -m \"fix sed -i usage\"",
            "git commit -m \"$(cat <<'EOF'\nUse open(p, 'w') here, not sed -i\nEOF\n)\"",
            "cat <<'EOF' | kubectl apply -f -\nkind: Pod\nEOF",
            "grep -rn \"write_text(\" src",
        ];
        for command in allowed {
            assert_eq!(changes_files(command), None, "refused: {command}");
        }
    }

    #[tokio::test]
    async fn a_refused_command_does_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a").unwrap();
        let out = run(json!({"command": format!("sed -i.bak 's/a/b/' {}", path.display())})).await;
        assert!(out.is_error && out.content.contains("`edit`"), "{}", out.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a");
    }
}
