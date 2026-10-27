use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{FileChange, FileTracker, Tool, ToolOutput, parse_input};

/// Replaces an exact piece of a file, so a change costs as much as the
/// change rather than the whole file.
pub struct EditTool {
    files: FileTracker,
}

impl EditTool {
    pub fn new(files: FileTracker) -> Self {
        Self { files }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditTool {
    fn access(&self, input: &Value) -> super::scheduling::Access {
        super::scheduling::file_access(input)
    }

    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Change part of an existing file by replacing `old_string` with `new_string`. \
         `old_string` must match the file exactly, whitespace and indentation included, without \
         the line-number prefix `read` shows, and must occur once unless `replace_all` is set. \
         Keep it to the lines that change plus enough context to be unique; make several edits \
         rather than one huge one. The file must be read with the `read` tool first, and the \
         edit is refused if it changed since that read. Edits to one file in the same response \
         apply in order. Relative paths resolve against the working directory."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the file to change."
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact text to replace."
                },
                "new_string": {
                    "type": "string",
                    "description": "The text to put in its place; empty to delete it."
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence instead of requiring exactly one. Defaults to false."
                }
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let mut input: Input = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        input.path = super::workdir::resolve(&input.path);
        if input.old_string.is_empty() {
            return ToolOutput::error("`old_string` is empty; to create a file, use `write`");
        }
        if input.old_string == input.new_string {
            return ToolOutput::error("`old_string` and `new_string` are the same; nothing to change");
        }
        let path = Path::new(&input.path);
        if path.is_dir() {
            return ToolOutput::error(format!("{} is a directory", input.path));
        }
        if !path.exists() {
            return ToolOutput::error(format!("{} does not exist; to create it, use `write`", input.path));
        }
        if let Some(sandbox) = crate::security::sandbox::active()
            && let Err(e) = sandbox.check_write(path)
        {
            return ToolOutput::error(e);
        }
        let _changing = self.files.changing().await;
        if let Err(e) = self.files.check_writable(path) {
            return ToolOutput::error(e);
        }
        let before = match tokio::fs::read(path).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => return ToolOutput::error(format!("{} is not a UTF-8 text file", input.path)),
            },
            Err(e) => return ToolOutput::error(format!("Cannot read {}: {e}", input.path)),
        };

        // `read` shows lines without their `\r`, so the model writes `\n`.
        let crlf = before.contains("\r\n") && !input.old_string.contains('\r');
        let (old, new) = if crlf {
            (input.old_string.replace('\n', "\r\n"), input.new_string.replace('\n', "\r\n"))
        } else {
            (input.old_string, input.new_string)
        };
        let found = before.matches(&old).count();
        let after = match found {
            0 => {
                return ToolOutput::error(format!(
                    "`old_string` was not found in {}. It must match the file exactly, including \
                     whitespace and indentation, without the line-number prefix `read` adds; read the \
                     lines again if unsure.",
                    input.path
                ));
            }
            1 => before.replacen(&old, &new, 1),
            _ if input.replace_all => before.replace(&old, &new),
            n => {
                return ToolOutput::error(format!(
                    "`old_string` occurs {n} times in {}; include more surrounding lines to pick one, \
                     or set `replace_all` to change every one",
                    input.path
                ));
            }
        };
        crate::session::checkpoint::before_change(path).await;
        if let Err(e) = tokio::fs::write(path, &after).await {
            return ToolOutput::error(format!("Cannot write {}: {e}", input.path));
        }
        self.files.record(path);

        let line = before[..before.find(&old).unwrap_or(0)].matches('\n').count() + 1;
        let summary = match found {
            1 => format!("Edited {} at line {line}", input.path),
            n => format!("Edited {}: replaced {n} occurrences, the first at line {line}", input.path),
        };
        ToolOutput::ok(summary).with_change(FileChange { path: input.path, before: Some(before), after })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ReadTool;

    /// A file with `text`, already read through the returned tool's tracker.
    async fn read_file(dir: &tempfile::TempDir, text: &str) -> (std::path::PathBuf, EditTool) {
        let path = dir.path().join("f.txt");
        std::fs::write(&path, text).unwrap();
        let files = FileTracker::default();
        ReadTool::new(files.clone()).run(json!({"path": path})).await;
        (path, EditTool::new(files))
    }

    #[tokio::test]
    async fn replaces_a_unique_string() {
        let dir = tempfile::tempdir().unwrap();
        let (path, edit) = read_file(&dir, "fn a() {}\nfn b() {}\n").await;
        let out = edit.run(json!({"path": path, "old_string": "fn b() {}", "new_string": "fn c() {}"})).await;
        assert_eq!(out.content, format!("Edited {} at line 2", path.display()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a() {}\nfn c() {}\n");
        let change = out.change.unwrap();
        assert_eq!(change.before.as_deref(), Some("fn a() {}\nfn b() {}\n"));
        assert_eq!(change.after, "fn a() {}\nfn c() {}\n");

        // Its own change doesn't need a new read.
        let out = edit.run(json!({"path": path, "old_string": "fn a() {}\n", "new_string": ""})).await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn c() {}\n");
    }

    #[tokio::test]
    async fn needs_a_read_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "old").unwrap();
        let out = EditTool::new(FileTracker::default())
            .run(json!({"path": path, "old_string": "old", "new_string": "new"}))
            .await;
        assert!(out.is_error && out.content.contains("read it with the `read` tool"), "{}", out.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
    }

    #[tokio::test]
    async fn refuses_missing_and_ambiguous_strings() {
        let dir = tempfile::tempdir().unwrap();
        let (path, edit) = read_file(&dir, "x = 1\nx = 1\n").await;
        let out = edit.run(json!({"path": path, "old_string": "y = 1", "new_string": "y = 2"})).await;
        assert!(out.is_error && out.content.contains("was not found"), "{}", out.content);
        let out = edit.run(json!({"path": path, "old_string": "x = 1", "new_string": "x = 2"})).await;
        assert!(out.is_error && out.content.contains("occurs 2 times"), "{}", out.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x = 1\nx = 1\n");

        let out = edit.run(json!({"path": path, "old_string": "x = 1", "new_string": "x = 2", "replace_all": true})).await;
        assert_eq!(out.content, format!("Edited {}: replaced 2 occurrences, the first at line 1", path.display()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x = 2\nx = 2\n");
    }

    #[tokio::test]
    async fn refuses_what_write_is_for() {
        let dir = tempfile::tempdir().unwrap();
        let (path, edit) = read_file(&dir, "a").await;
        let missing = dir.path().join("new.txt");
        let out = edit.run(json!({"path": missing, "old_string": "a", "new_string": "b"})).await;
        assert!(out.is_error && out.content.contains("use `write`"), "{}", out.content);
        let out = edit.run(json!({"path": path, "old_string": "", "new_string": "b"})).await;
        assert!(out.is_error && out.content.contains("use `write`"), "{}", out.content);
        let out = edit.run(json!({"path": path, "old_string": "a", "new_string": "a"})).await;
        assert!(out.is_error, "{}", out.content);
        let out = edit.run(json!({"path": path, "old_string": "a", "new_string": "b", "line": 1})).await;
        assert!(out.is_error && out.content.starts_with("Invalid input"), "{}", out.content);
    }

    #[tokio::test]
    async fn keeps_crlf_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let (path, edit) = read_file(&dir, "one\r\ntwo\r\nthree\r\n").await;
        let out = edit.run(json!({"path": path, "old_string": "one\ntwo", "new_string": "one\n2"})).await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\r\n2\r\nthree\r\n");
    }

    #[tokio::test]
    async fn edits_from_one_response_all_apply() {
        let dir = tempfile::tempdir().unwrap();
        let (path, edit) = read_file(&dir, "a\nb\nc\n").await;
        let outs = futures::future::join_all(["a", "b", "c"].map(|s| {
            edit.run(json!({"path": path, "old_string": format!("{s}\n"), "new_string": format!("{s}{s}\n")}))
        }))
        .await;
        assert!(outs.iter().all(|o| !o.is_error), "{outs:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "aa\nbb\ncc\n");
    }

    #[tokio::test]
    async fn agents_editing_one_file_at_once_never_lose_a_change() {
        // Each agent (a subagent, say) has its own tracker; both read the file.
        let dir = tempfile::tempdir().unwrap();
        let (path, first) = read_file(&dir, "a\nb\n").await;
        let files = FileTracker::default();
        ReadTool::new(files.clone()).run(json!({"path": path})).await;
        let second = EditTool::new(files);
        for _ in 0..20 {
            let (one, two) = tokio::join!(
                first.run(json!({"path": path, "old_string": "a", "new_string": "a+"})),
                second.run(json!({"path": path, "old_string": "b", "new_string": "b+"})),
            );
            // One goes first; the other sees the file changed under it.
            assert!(one.is_error != two.is_error, "{one:?} {two:?}");
            let refused = if one.is_error { &one } else { &two };
            assert!(refused.content.contains("changed since it was last read"), "{}", refused.content);
            let text = std::fs::read_to_string(&path).unwrap();
            assert_eq!(text.matches('+').count(), 1, "{text}");
            std::fs::write(&path, "a\nb\n").unwrap();
            for tool in [&first, &second] {
                tool.files.record(&path);
            }
        }
    }
}
