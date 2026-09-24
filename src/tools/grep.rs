use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{Tool, ToolOutput, parse_input, truncate};

const TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_HEAD_LIMIT: usize = 250;
const MAX_OUTPUT_BYTES: usize = 30_000;

/// Searches file contents by shelling out to ugrep
/// (https://github.com/Genivia/ugrep).
pub struct GrepTool;

#[derive(Deserialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    Content,
    #[default]
    FilesWithMatches,
    Count,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    #[serde(default)]
    output_mode: OutputMode,
    #[serde(default)]
    case_insensitive: bool,
    context: Option<u32>,
    head_limit: Option<usize>,
}

impl Input {
    fn ugrep_args(&self) -> Vec<String> {
        let mut args: Vec<String> = [
            "--recursive",
            "--with-filename",
            "--ignore-files", // honour .gitignore
            "--ignore-binary",
            "--no-messages", // skip unreadable files silently
            "--color=never",
            "--sort",        // stable output order
            "--perl-regexp", // same syntax whatever the platform grep is
        ]
        .map(String::from)
        .to_vec();
        match self.output_mode {
            OutputMode::Content => {
                args.push("--line-number".into());
                if let Some(n) = self.context {
                    args.push(format!("--context={n}"));
                }
            }
            OutputMode::FilesWithMatches => args.push("--files-with-matches".into()),
            OutputMode::Count => args.push("--count".into()),
        }
        if self.case_insensitive {
            args.push("--ignore-case".into());
        }
        if let Some(glob) = &self.glob {
            args.push(format!("--glob={glob}"));
        }
        args.push("-e".into());
        args.push(self.pattern.clone());
        args.push("--".into());
        args.push(self.path.clone().unwrap_or_else(|| ".".into()));
        args
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a Perl-compatible regular expression (powered by ugrep). \
         Searches recursively, honours .gitignore and skips binary files. \
         `output_mode` is `files_with_matches` (default: matching file paths), `content` \
         (matching lines as path:line:text, with optional `context` lines) or `count` \
         (matches per file). Output is limited to `head_limit` lines (default 250). \
         Prefer this over running grep through `bash`."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Perl-compatible regular expression to search for."
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search (default: working directory)."
                },
                "glob": {
                    "type": "string",
                    "description": "Only search files matching this glob, e.g. `*.rs` or `src/**/*.{ts,tsx}`."
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["files_with_matches", "content", "count"],
                    "description": "What to return (default files_with_matches)."
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Match case-insensitively (default false)."
                },
                "context": {
                    "type": "integer",
                    "description": "Lines of context around each match; content mode only."
                },
                "head_limit": {
                    "type": "integer",
                    "description": "Maximum number of output lines (default 250)."
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };

        let child = Command::new("ugrep")
            .args(input.ugrep_args())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let child = match child {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolOutput::error(
                    "ugrep is not installed. Install it (e.g. `apt install ugrep` or \
                     `brew install ugrep`) to enable the grep tool.",
                );
            }
            Err(e) => return ToolOutput::error(format!("Failed to run ugrep: {e}")),
        };
        let output = match tokio::time::timeout(TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return ToolOutput::error(format!("Failed to run ugrep: {e}")),
            Err(_) => return ToolOutput::error("Search timed out; narrow `path` or `glob`"),
        };

        // ugrep exits 0 on matches, 1 on no matches, 2 on errors.
        match output.status.code() {
            Some(0) => {}
            Some(1) => return ToolOutput::ok("No matches found"),
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return ToolOutput::error(format!("ugrep failed: {}", stderr.trim()));
            }
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let limit = input.head_limit.unwrap_or(DEFAULT_HEAD_LIMIT).max(1);
        let total = stdout.lines().count();
        let mut text: String = stdout.lines().take(limit).collect::<Vec<_>>().join("\n");
        if total > limit {
            text.push_str(&format!("\n... [showing {limit} of {total} lines; raise head_limit or narrow the search]"));
        }
        ToolOutput::ok(truncate(&text, MAX_OUTPUT_BYTES))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(p.join("a.rs"), "fn main() {}\nfn helper() {}\n").unwrap();
        std::fs::write(p.join("b.txt"), "Main thing\n").unwrap();
        std::fs::write(p.join("ignored.rs"), "fn main() {}\n").unwrap();
        std::fs::write(p.join(".gitignore"), "ignored.rs\n").unwrap();
        dir
    }

    // These tests need ugrep on PATH.
    async fn grep(input: Value) -> ToolOutput {
        GrepTool.run(input).await
    }

    #[tokio::test]
    async fn lists_matching_files_and_honours_gitignore() {
        let dir = fixture();
        let out = grep(json!({"pattern": "fn main", "path": dir.path()})).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("a.rs"));
        assert!(!out.content.contains("ignored.rs"), "{}", out.content);
        assert!(!out.content.contains("b.txt"));
    }

    #[tokio::test]
    async fn content_mode_with_line_numbers_and_glob() {
        let dir = fixture();
        let out = grep(json!({
            "pattern": "helper", "path": dir.path(), "glob": "*.rs", "output_mode": "content"
        }))
        .await;
        assert!(out.content.ends_with("a.rs:2:fn helper() {}"), "{}", out.content);
    }

    #[tokio::test]
    async fn case_insensitive_and_count() {
        let dir = fixture();
        let out = grep(json!({
            "pattern": "main", "path": dir.path(), "glob": "*.txt",
            "case_insensitive": true, "output_mode": "count"
        }))
        .await;
        assert!(out.content.ends_with("b.txt:1"), "{}", out.content);
    }

    #[tokio::test]
    async fn no_matches_is_not_an_error() {
        let dir = fixture();
        let out = grep(json!({"pattern": "zzz_nothing", "path": dir.path()})).await;
        assert_eq!(out, ToolOutput::ok("No matches found"));
    }

    #[tokio::test]
    async fn invalid_regex_is_an_error() {
        let dir = fixture();
        let out = grep(json!({"pattern": "(unclosed", "path": dir.path()})).await;
        assert!(out.is_error, "{}", out.content);
    }

    #[tokio::test]
    async fn head_limit_truncates() {
        let dir = fixture();
        let out = grep(json!({"pattern": "fn", "path": dir.path(), "output_mode": "content", "head_limit": 1})).await;
        assert!(out.content.contains("showing 1 of 2 lines"), "{}", out.content);
    }
}
