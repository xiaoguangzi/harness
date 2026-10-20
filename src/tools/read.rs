use async_trait::async_trait;
use tokio::io::AsyncBufReadExt;
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

    /// The notes of the subdirectories `path` is in, below the working
    /// directory (whose own are in the system prompt), that the model has
    /// not seen yet.
    fn directory_notes(&self, path: &std::path::Path) -> String {
        let root = super::workdir::get();
        let mut text = String::new();
        for note in self.files.new_notes(&root, path) {
            let label = note.strip_prefix(&root).unwrap_or(&note).display().to_string();
            if let Some(notes) = crate::prompt::directory_notes(&note, &label) {
                text.push_str("\n\n");
                text.push_str(&notes);
            }
        }
        text
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
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a text file and return its contents with 1-based line numbers. \
         Reads up to 2000 lines by default; use `offset` and `limit` to page through \
         larger files. Lines longer than 2000 characters are truncated. \
         Images (png, jpg, gif, webp, up to 3.75 MB) and PDFs (up to 20 MB) are returned \
         for you to view; `offset` and `limit` do not apply to them. \
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

    fn read_only(&self) -> bool {
        true
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let mut input: Input = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        input.path = super::workdir::resolve(&input.path);

        if let Some(sandbox) = crate::sandbox::active()
            && let Err(e) = sandbox.check_read(input.path.as_ref())
        {
            return ToolOutput::error(e);
        }
        let path: &std::path::Path = input.path.as_ref();
        if let Ok(meta) = std::fs::metadata(path)
            && let Some(media) = crate::mentions::media_block(path, &input.path, meta.len())
        {
            return match media {
                Ok((label, block)) => ToolOutput { images: vec![block], ..ToolOutput::ok(label) },
                Err(e) => ToolOutput::error(format!("{}: {e}", input.path)),
            };
        }
        let offset = input.offset.unwrap_or(1).max(1);
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT).max(1);
        let window = match read_window(&input.path, offset, limit).await {
            Ok(w) => w,
            Err(e) => return ToolOutput::error(e),
        };
        self.files.record(input.path.as_ref());
        if window.total == 0 {
            return ToolOutput::ok(format!("({} is empty)", input.path));
        }
        let lines = window.lines.iter().map(String::as_str);
        match render(lines, offset, limit, window.total) {
            Ok(out) => ToolOutput::ok(out + &self.directory_notes(path)),
            Err(e) => ToolOutput::error(format!("{}: {e}", input.path)),
        }
    }
}

/// Lines `offset..offset + limit` of a file, and its line count.
struct Window {
    lines: Vec<String>,
    total: usize,
}

/// Reads a file in chunks, keeping only the requested lines, and only the
/// start of a line too long to show, so no file is held in memory whole.
async fn read_window(path: &str, offset: usize, limit: usize) -> Result<Window, String> {
    // More than enough bytes for `MAX_LINE_CHARS` characters, so `render`
    // still marks a cut line as truncated.
    const MAX_LINE_BYTES: usize = (MAX_LINE_CHARS + 1) * 4;
    let cannot = |e: std::io::Error| format!("Cannot read {path}: {e}");
    let file = tokio::fs::File::open(path).await.map_err(cannot)?;
    let mut reader = tokio::io::BufReader::with_capacity(64 * 1024, file);
    let wanted = offset - 1..(offset - 1).saturating_add(limit);
    let mut lines = Vec::new();
    // The line being read, if it is wanted; its index; the last byte seen.
    let (mut line, mut index, mut last) = (Vec::new(), 0, None);
    let take_line = |line: &mut Vec<u8>| {
        let text = String::from_utf8_lossy(line).into_owned();
        line.clear();
        text
    };
    loop {
        let chunk = reader.fill_buf().await.map_err(cannot)?;
        if chunk.is_empty() {
            break;
        }
        if chunk.contains(&0) {
            return Err(format!("{path} looks like a binary file"));
        }
        let mut pieces = chunk.split(|&b| b == b'\n').peekable();
        while let Some(piece) = pieces.next() {
            if wanted.contains(&index) {
                let room = MAX_LINE_BYTES.saturating_sub(line.len());
                line.extend_from_slice(&piece[..piece.len().min(room)]);
            }
            // The last piece runs on into the next chunk.
            if pieces.peek().is_none() {
                break;
            }
            if wanted.contains(&index) {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                lines.push(take_line(&mut line));
            }
            index += 1;
        }
        let n = chunk.len();
        last = chunk.last().copied();
        reader.consume(n);
    }
    // A last line without a newline counts too.
    if last.is_some_and(|b| b != b'\n') {
        if wanted.contains(&index) {
            lines.push(take_line(&mut line));
        }
        index += 1;
    }
    Ok(Window { lines, total: index })
}

/// Renders up to `limit` lines starting at 1-based `offset` in `cat -n`
/// style, noting when more lines remain. Errors if `offset` is past the end.
pub(crate) fn number_lines(text: &str, offset: usize, limit: usize) -> Result<String, String> {
    let offset = offset.max(1);
    let limit = limit.max(1);
    render(text.lines().skip(offset - 1).take(limit), offset, limit, text.lines().count())
}

/// Renders `lines`, which start at 1-based `offset` (at least 1) and are at
/// most `limit` (at least 1) of the `total`.
fn render<'a>(
    lines: impl Iterator<Item = &'a str>,
    offset: usize,
    limit: usize,
    total: usize,
) -> Result<String, String> {
    if offset > total {
        return Err(format!("offset {offset} is past the end ({total} lines)"));
    }

    let mut out = String::new();
    for (i, line) in (offset - 1..).zip(lines) {
        let line = match line.char_indices().nth(MAX_LINE_CHARS) {
            Some((cut, _)) => format!("{}... [line truncated]", &line[..cut]),
            None => line.to_string(),
        };
        out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
    }
    let last = (offset - 1).saturating_add(limit).min(total);
    if last < total {
        out.push_str(&format!(
            "... [showing lines {offset}-{last} of {total}; use offset={} to continue]\n",
            last + 1
        ));
    }
    Ok(out)
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

    #[test]
    fn shows_each_subdirectory_s_notes_once() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        std::fs::create_dir_all(root.join("crates/core/src")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "top").unwrap();
        std::fs::write(root.join("crates/AGENTS.md"), "crates").unwrap();
        std::fs::write(root.join("crates/core/AGENTS.md"), "core").unwrap();
        let files = FileTracker::default();
        let file = root.join("crates/core/src/lib.rs");
        let names = |notes: Vec<std::path::PathBuf>| -> Vec<String> {
            notes.iter().map(|n| n.strip_prefix(root).unwrap().display().to_string()).collect()
        };
        assert_eq!(names(files.new_notes(root, &file)), ["crates/AGENTS.md", "crates/core/AGENTS.md"], "not the root's");
        assert!(files.new_notes(root, &file).is_empty(), "shown once");
        assert!(files.new_notes(root, &root.join("crates/x.rs")).is_empty());
        assert!(files.new_notes(root, std::path::Path::new("/elsewhere/AGENTS.md")).is_empty());
        files.retain(&[]);
        assert_eq!(files.new_notes(root, &file).len(), 2, "again after a compaction");

        let notes = crate::prompt::directory_notes(&root.join("crates/core/AGENTS.md"), "crates/core/AGENTS.md").unwrap();
        assert!(notes.starts_with("`crates/core/AGENTS.md` holds notes for the files in `crates/core/`"), "{notes}");
        assert!(notes.ends_with("<agents-md>\ncore\n</agents-md>"), "{notes}");
    }

    #[tokio::test]
    async fn returns_images_and_pdfs_as_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nfake").unwrap();
        let out = read_tool().run(json!({"path": png})).await;
        assert!(!out.is_error && out.content.ends_with("shot.png (image, 12 B)"), "{}", out.content);
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0]["type"], "image");
        assert_eq!(out.images[0]["source"]["media_type"], "image/png");

        let pdf = dir.path().join("spec.PDF");
        std::fs::write(&pdf, b"%PDF-1.4").unwrap();
        let out = read_tool().run(json!({"path": pdf})).await;
        assert_eq!(out.images[0]["type"], "document");
        assert_eq!(out.images[0]["source"]["media_type"], "application/pdf");

        let big = dir.path().join("big.jpg");
        std::fs::write(&big, vec![0u8; 4_000_000]).unwrap();
        let out = read_tool().run(json!({"path": big})).await;
        assert!(out.is_error && out.content.contains("limit"), "{}", out.content);
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
    async fn reads_across_chunks() {
        // A line longer than a read chunk, CRLF endings, no final newline.
        let long = "x".repeat(200_000);
        let mut content = format!("{long}\r\nshort\r\n");
        for i in 3..=100_000 {
            content.push_str(&format!("line {i}\n"));
        }
        content.push_str("tail");
        let f = file_with(content.as_bytes());

        let out = read_tool().run(json!({"path": path(&f), "limit": 2})).await;
        let first = format!("     1\t{}... [line truncated]\n     2\tshort\n", "x".repeat(MAX_LINE_CHARS));
        assert_eq!(out.content, format!("{first}... [showing lines 1-2 of 100001; use offset=3 to continue]\n"));

        let out = read_tool().run(json!({"path": path(&f), "offset": 99_999})).await;
        assert_eq!(out.content, " 99999\tline 99999\n100000\tline 100000\n100001\ttail\n");
    }

    #[tokio::test]
    async fn empty_file() {
        let f = file_with(b"");
        let out = read_tool().run(json!({"path": path(&f)})).await;
        assert_eq!(out.content, format!("({} is empty)", path(&f)));
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
