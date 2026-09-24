use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{FileTracker, Tool, ToolOutput, parse_input};

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;

/// Reads a text file and returns it with line numbers (`cat -n` style).
pub struct ReadTool {
    files: FileTracker,
}

impl ReadTool {
    pub fn new(files: FileTracker) -> Self {
        Self { files }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read a text file and return its contents with 1-based line numbers. \
         Reads up to 2000 lines by default; use `offset` and `limit` to page through \
         larger files. Lines longer than 2000 characters are truncated. \
         Relative paths resolve against the working directory."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the file to read."
                },
                "offset": {
                    "type": "integer",
                    "description": "1-based line number to start from (default 1)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to return (default 2000)."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };

        let bytes = match tokio::fs::read(&input.path).await {
            Ok(b) => b,
            Err(e) => return ToolOutput::error(format!("Cannot read {}: {e}", input.path)),
        };
        if bytes.contains(&0) {
            return ToolOutput::error(format!("{} looks like a binary file", input.path));
        }
        self.files.record(input.path.as_ref());
        let text = String::from_utf8_lossy(&bytes);
        if text.is_empty() {
            return ToolOutput::ok(format!("({} is empty)", input.path));
        }

        let offset = input.offset.unwrap_or(1).max(1);
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT).max(1);
        let total = text.lines().count();
        if offset > total {
            return ToolOutput::error(format!(
                "offset {offset} is past the end of {} ({total} lines)",
                input.path
            ));
        }

        let mut out = String::new();
        for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
            let line = match line.char_indices().nth(MAX_LINE_CHARS) {
                Some((cut, _)) => format!("{}... [line truncated]", &line[..cut]),
                None => line.to_string(),
            };
            out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
        }
        let last = (offset - 1 + limit).min(total);
        if last < total {
            out.push_str(&format!(
                "... [showing lines {offset}-{last} of {total}; use offset={} to continue]\n",
                last + 1
            ));
        }
        ToolOutput::ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn file_with(content: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f
    }

    fn read_tool() -> ReadTool {
        ReadTool::new(FileTracker::default())
    }

    fn path(f: &tempfile::NamedTempFile) -> String {
        f.path().to_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn numbers_lines() {
        let f = file_with(b"a\nb\nc\n");
        let out = read_tool().run(json!({"path": path(&f)})).await;
        assert_eq!(out, ToolOutput::ok("     1\ta\n     2\tb\n     3\tc\n"));
    }

    #[tokio::test]
    async fn pages_with_offset_and_limit() {
        let f = file_with(b"a\nb\nc\nd\n");
        let out = read_tool().run(json!({"path": path(&f), "offset": 2, "limit": 2})).await;
        assert!(!out.is_error);
        assert!(out.content.starts_with("     2\tb\n     3\tc\n"));
        assert!(out.content.contains("use offset=4 to continue"));
    }

    #[tokio::test]
    async fn offset_past_end_is_error() {
        let f = file_with(b"a\n");
        let out = read_tool().run(json!({"path": path(&f), "offset": 5})).await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn rejects_binary() {
        let f = file_with(b"\x00\x01\x02");
        let out = read_tool().run(json!({"path": path(&f)})).await;
        assert!(out.is_error);
        assert!(out.content.contains("binary"));
    }

    #[tokio::test]
    async fn missing_file_is_error() {
        let out = read_tool().run(json!({"path": "/definitely/not/here"})).await;
        assert!(out.is_error);
    }
}
