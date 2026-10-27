use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{FileChange, FileTracker, Tool, ToolOutput, parse_input};

/// Creates or overwrites a file with the given contents.
pub struct WriteTool {
    files: FileTracker,
}

impl WriteTool {
    pub fn new(files: FileTracker) -> Self {
        Self { files }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteTool {
    fn access(&self, input: &Value) -> super::scheduling::Access {
        super::scheduling::file_access(input)
    }

    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write `content` to a file, replacing it entirely. Use it to create a file, or to \
         rewrite one completely; to change part of a file use `edit`. Creates any missing \
         parent directories. An existing file must be read with the `read` tool first, and \
         the write is refused if it changed since that read. Relative paths resolve against \
         the working directory."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the file to write."
                },
                "content": {
                    "type": "string",
                    "description": "The complete new contents of the file."
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let mut input: Input = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        input.path = super::workdir::resolve(&input.path);
        let path = Path::new(&input.path);
        if path.is_dir() {
            return ToolOutput::error(format!("{} is a directory", input.path));
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

        // Kept for the diff shown to the user.
        let before = match tokio::fs::read(path).await {
            Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            Err(_) => None,
        };
        crate::session::checkpoint::before_change(path).await;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return ToolOutput::error(format!("Cannot create {}: {e}", parent.display()));
        }
        if let Err(e) = tokio::fs::write(path, &input.content).await {
            return ToolOutput::error(format!("Cannot write {}: {e}", input.path));
        }
        // The model knows the new contents, so further writes are allowed.
        self.files.record(path);

        let verb = if before.is_some() { "Updated" } else { "Created" };
        let summary =
            format!("{verb} {} ({} lines, {} bytes)", input.path, input.content.lines().count(), input.content.len());
        ToolOutput::ok(summary).with_change(FileChange { path: input.path, before, after: input.content })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ReadTool;

    #[tokio::test]
    async fn creates_file_and_parents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/new.txt");
        let tool = WriteTool::new(FileTracker::default());
        let out = tool.run(json!({"path": path, "content": "x\ny\n"})).await;
        assert_eq!(out.content, format!("Created {} (2 lines, 4 bytes)", path.display()));
        assert_eq!(out.change.unwrap().before, None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\ny\n");
    }

    #[tokio::test]
    async fn existing_file_needs_read_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "old").unwrap();
        let files = FileTracker::default();
        let write = WriteTool::new(files.clone());

        let out = write.run(json!({"path": path, "content": "new"})).await;
        assert!(out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");

        ReadTool::new(files).run(json!({"path": path})).await;
        let out = write.run(json!({"path": path, "content": "new"})).await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");

        // A second write without re-reading is fine: the model wrote it.
        let out = write.run(json!({"path": path, "content": "newer"})).await;
        assert!(!out.is_error, "{}", out.content);
    }

    #[tokio::test]
    async fn refuses_if_changed_since_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "old").unwrap();
        let files = FileTracker::default();
        ReadTool::new(files.clone()).run(json!({"path": path})).await;

        // Simulate an external edit with a distinct mtime.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::write(&path, "external").unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();

        let out = WriteTool::new(files).run(json!({"path": path, "content": "new"})).await;
        assert!(out.is_error);
        assert!(out.content.contains("changed since it was last read"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external");
    }

    #[tokio::test]
    async fn rejects_unknown_fields() {
        let tool = WriteTool::new(FileTracker::default());
        let out = tool.run(json!({"path": "x", "content": "", "mode": "append"})).await;
        assert!(out.is_error);
        assert!(out.content.starts_with("Invalid input"));
    }
}
