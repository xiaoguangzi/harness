use std::path::Path;
use std::time::SystemTime;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::grep::ugrep;
use super::{Tool, ToolOutput, parse_input, truncate};

/// Paths listed unless `head_limit` says otherwise, and the most it may say.
const DEFAULT_HEAD_LIMIT: usize = 100;
const MAX_HEAD_LIMIT: usize = 1_000;
const MAX_OUTPUT_BYTES: usize = 30_000;
/// The most patterns one brace pattern expands into.
const MAX_EXPANSIONS: usize = 64;
/// A regular expression no line matches (`a`, then a word boundary, then
/// `b`), so ugrep's `--files-without-match` lists every file, empty ones
/// included.
const NEVER: &str = r"a\bb";

/// Finds files by name with a glob pattern, through ugrep like `grep`, so
/// it honours .gitignore and the sandbox the same way.
pub struct GlobTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    pattern: String,
    path: Option<String>,
    head_limit: Option<usize>,
}

impl Input {
    fn ugrep_args(&self) -> Vec<String> {
        let mut args: Vec<String> =
            ["--recursive", "--ignore-files", "--no-messages", "--color=never", "--files-without-match"]
                .map(String::from)
                .to_vec();
        let root = self.path.as_deref().map(|p| p.trim_end_matches('/')).filter(|p| !p.is_empty() && *p != ".");
        let root = root.map(|r| r.strip_prefix("./").unwrap_or(r));
        for pattern in expand_braces(&self.pattern) {
            // ugrep matches a pattern with a `/` against the path as it
            // prints it, which starts with `path`; one without matches the
            // file name at any depth.
            let pattern = match root {
                Some(root) if pattern.contains('/') => {
                    format!("{}/{}", escape(root), pattern.trim_start_matches('/'))
                }
                _ => pattern,
            };
            args.push(format!("--glob={pattern}"));
        }
        args.extend(["-e".into(), NEVER.into(), "--".into()]);
        args.push(self.path.clone().unwrap_or_else(|| ".".into()));
        args
    }
}

/// `src/*.{ts,tsx}` as `src/*.ts` and `src/*.tsx`, which ugrep's globs
/// cannot say in one pattern.
pub(super) fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else { return vec![pattern.to_string()] };
    let mut depth = 0;
    let mut parts = Vec::new();
    let mut start = open + 1;
    for (i, c) in pattern[open..].char_indices().map(|(i, c)| (i + open, c)) {
        match c {
            '{' => depth += 1,
            '}' if depth == 1 => {
                parts.push(&pattern[start..i]);
                let (head, tail) = (&pattern[..open], &pattern[i + 1..]);
                if parts.len() == 1 {
                    // `{x}` is no alternative; keep it as written.
                    return expand_braces(tail).into_iter().map(|t| format!("{head}{{{}}}{t}", parts[0])).collect();
                }
                let mut out = Vec::new();
                for part in parts {
                    for rest in expand_braces(&format!("{part}{tail}")) {
                        if out.len() < MAX_EXPANSIONS {
                            out.push(format!("{head}{rest}"));
                        }
                    }
                }
                return out;
            }
            '}' => depth -= 1,
            ',' if depth == 1 => {
                parts.push(&pattern[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    vec![pattern.to_string()]
}

/// `path` with the characters globs give a meaning escaped.
fn escape(path: &str) -> String {
    path.chars().fold(String::new(), |mut out, c| {
        if "*?[]{}\\!".contains(c) {
            out.push('\\');
        }
        out.push(c);
        out
    })
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files by name with a glob pattern, such as `**/*.rs` or `src/**/*.{ts,tsx}`. Searches \
         recursively, honours .gitignore and skips hidden files; returns paths, the most recently \
         modified first (at most `head_limit`, default 100). A pattern without `/` matches file \
         names at any depth (`*.rs`); one with `/` matches paths relative to `path`. Use it to find \
         files by name, and `grep` to find them by content."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob to match, e.g. `**/*.rs`, `src/**/test_*.py` or `*.{json,toml}`."
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search (default: working directory)."
                },
                "head_limit": {
                    "type": "integer",
                    "description": "List at most this many paths (default 100, at most 1000)."
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let stdout = match ugrep(input.ugrep_args()).await {
            Ok(Some(stdout)) => stdout,
            Ok(None) => return ToolOutput::ok("No files found"),
            Err(e) => return e,
        };
        let modified =
            |path: &str| Path::new(path).metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        let mut files: Vec<(SystemTime, &str)> = stdout.lines().map(|p| (modified(p), p)).collect();
        files.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        let total = files.len();
        let limit = input.head_limit.unwrap_or(DEFAULT_HEAD_LIMIT).clamp(1, MAX_HEAD_LIMIT);
        let mut text = files.iter().take(limit).map(|(_, p)| *p).collect::<Vec<_>>().join("\n");
        if total > limit {
            text.push_str(&format!(
                "\n... [showing the {limit} most recently modified of {total} files; narrow the pattern or path, \
                 or raise head_limit]"
            ));
        }
        ToolOutput::ok(truncate(&text, MAX_OUTPUT_BYTES))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests need ugrep on PATH.
    async fn glob(input: Value) -> ToolOutput {
        GlobTool.run(input).await
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for (file, text) in [
            ("src/main.rs", "fn main() {}\n"),
            ("src/tools/empty.rs", ""),
            ("src/tools/bin.rs", "\0\u{1}"),
            ("Cargo.toml", "[package]\n"),
            ("README.md", "# x\n"),
            ("ignored/x.rs", "\n"),
            (".gitignore", "ignored/\n"),
        ] {
            std::fs::create_dir_all(p.join(file).parent().unwrap()).unwrap();
            std::fs::write(p.join(file), text).unwrap();
        }
        dir
    }

    fn lines(out: &ToolOutput, root: &Path) -> Vec<String> {
        assert!(!out.is_error, "{}", out.content);
        let prefix = format!("{}/", root.display());
        out.content.lines().map(|l| l.strip_prefix(&prefix).unwrap_or(l).to_string()).collect()
    }

    #[tokio::test]
    async fn finds_files_by_name() {
        let dir = fixture();
        let root = dir.path();
        let find = async |pattern: &str| {
            let mut found = lines(&glob(json!({"pattern": pattern, "path": root})).await, root);
            found.sort();
            found
        };
        // Empty and binary files too, ignored ones not.
        assert_eq!(find("*.rs").await, ["src/main.rs", "src/tools/bin.rs", "src/tools/empty.rs"]);
        assert_eq!(find("**/*.rs").await, find("*.rs").await);
        assert_eq!(find("src/tools/*.rs").await, ["src/tools/bin.rs", "src/tools/empty.rs"], "starts at `path`");
        assert_eq!(find("/*.{toml,md}").await, ["Cargo.toml", "README.md"]);
        assert_eq!(find("*.{toml,md}").await, ["Cargo.toml", "README.md"]);
        let out = glob(json!({"pattern": "tools/*.rs", "path": root})).await;
        assert_eq!(out, ToolOutput::ok("No files found"));
        let out = glob(json!({"pattern": "src/*.rs", "path": root.join("src")})).await;
        assert_eq!(out, ToolOutput::ok("No files found"), "a pattern with `/` starts at `path`");
        assert!(glob(json!({"pattern": "*.rs", "regex": true})).await.is_error);
    }

    #[tokio::test]
    async fn newest_first() {
        let dir = fixture();
        let root = dir.path();
        let age = |file: &str, secs: u64| {
            let file = std::fs::File::options().write(true).open(root.join(file)).unwrap();
            file.set_modified(SystemTime::now() - std::time::Duration::from_secs(secs)).unwrap();
        };
        age("src/main.rs", 300);
        age("src/tools/empty.rs", 100);
        age("src/tools/bin.rs", 200);
        let found = lines(&glob(json!({"pattern": "*.rs", "path": root})).await, root);
        assert_eq!(found, ["src/tools/empty.rs", "src/tools/bin.rs", "src/main.rs"]);
        let found = lines(&glob(json!({"pattern": "*.rs", "path": root, "head_limit": 2})).await, root);
        assert_eq!(found[..2], ["src/tools/empty.rs", "src/tools/bin.rs"]);
        assert!(found[2].contains("showing the 2 most recently modified of 3 files"), "{found:?}");
    }

    #[test]
    fn expands_braces() {
        assert_eq!(expand_braces("src/*.rs"), ["src/*.rs"]);
        assert_eq!(expand_braces("*.{ts,tsx}"), ["*.ts", "*.tsx"]);
        assert_eq!(expand_braces("{a,b}/*.{c,d}"), ["a/*.c", "a/*.d", "b/*.c", "b/*.d"]);
        assert_eq!(expand_braces("x.{a,{b,c}}"), ["x.a", "x.b", "x.c"]);
        assert_eq!(expand_braces("{x}.{a,b}"), ["{x}.a", "{x}.b"]);
        assert_eq!(expand_braces("open{a,b"), ["open{a,b"]);
        assert_eq!(expand_braces(&"{a,b}".repeat(10)).len(), MAX_EXPANSIONS);
        assert_eq!(escape("dir[1]/a*"), "dir\\[1\\]/a\\*");
    }
}
