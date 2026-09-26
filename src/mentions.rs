//! `@file` mentions in user input.
//!
//! `@path` attaches a file (or directory listing) to the prompt so the model
//! sees it without spending a tool call:
//!
//! - `@src/main.rs`           the whole file, line-numbered (first 2000 lines)
//! - `@src/main.rs#L10-20`    lines 10 to 20 (`#L10` for a single line)
//! - `@src/`                  a listing of the directory
//! - `@shot.png` / `@doc.pdf` sent as an image / PDF document block
//! - `@"path with spaces.txt"`
//!
//! A mention only counts when the path exists and the `@` does not follow
//! an e-mail-address character, so `me@example.com` and `@username` are left
//! alone. Trailing punctuation or Chinese text after the path is ignored
//! (`@main.rs,` / `看看@main.rs的代码`).
//!
//! Images pasted into the REPL are attachments too, shown in the prompt as
//! `[Image #n]`.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};

use crate::llm::Message;
use crate::tools::{FileTracker, number_lines};

const MAX_TEXT_LINES: usize = 2000;
const MAX_TEXT_BYTES: u64 = 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_PDF_BYTES: u64 = 20 * 1024 * 1024;
const MAX_DIR_ENTRIES: usize = 500;
const TRAILING_PUNCTUATION: &[char] = &[',', '.', ';', ':', '!', '?', ')', ']', '}', '"', '\'', '，', '。', '；', '：', '！', '？', '）'];

/// One resolved mention, ready to go into the user message.
#[derive(Debug)]
pub struct Attachment {
    /// Short description for the terminal, e.g. `src/main.rs (120 lines)`.
    pub label: String,
    pub blocks: Vec<Value>,
}

#[derive(Debug, Default)]
pub struct Expanded {
    pub attachments: Vec<Attachment>,
    /// Mentions of existing paths that could not be attached.
    pub warnings: Vec<String>,
}

impl Expanded {
    /// User message content: attachments first, then the prompt, which is
    /// the recommended order for long documents.
    pub fn into_content(self, prompt: &str) -> Vec<Value> {
        let mut content: Vec<Value> = self.attachments.into_iter().flat_map(|a| a.blocks).collect();
        content.push(json!({"type": "text", "text": prompt}));
        content
    }
}

/// Finds and resolves every `@` mention in `input`. Text files attached this
/// way are recorded in `files`, so the model may overwrite them.
pub fn expand(input: &str, files: &FileTracker) -> Expanded {
    let mut expanded = Expanded::default();
    let mut seen = Vec::new();
    for token in tokens(input) {
        let Some(mention) = resolve(&token) else { continue };
        if seen.contains(&mention) {
            continue;
        }
        match attach(&mention, files) {
            Ok(a) => expanded.attachments.push(a),
            Err(e) => expanded.warnings.push(format!("@{}: {e}", mention.path)),
        }
        seen.push(mention);
    }
    expanded
}

#[derive(Debug, Clone, PartialEq)]
struct Mention {
    path: String,
    /// 1-based inclusive line range.
    lines: Option<(usize, usize)>,
}

/// Raw mention texts (without the `@`), in order of appearance.
///
/// An `@` starts a mention unless it follows a character that can appear in
/// an e-mail address, so `看看@a.rs` works but `me@example.com` is skipped.
fn tokens(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(offset) = input[pos..].find('@') {
        let at = pos + offset;
        pos = at + 1;
        let prev = input[..at].chars().next_back();
        if prev.is_some_and(|c| c.is_ascii_alphanumeric() || "._-+%".contains(c)) {
            continue;
        }
        let rest = &input[at + 1..];
        let (token, consumed) = match rest.strip_prefix('"') {
            Some(quoted) => match quoted.find('"') {
                Some(end) => (&quoted[..end], end + 2),
                None => continue,
            },
            None => {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                (&rest[..end], end)
            }
        };
        if !token.is_empty() {
            out.push(token.to_string());
        }
        pos += consumed;
    }
    out
}

/// Turns a token into a mention of an existing path. Besides the whole
/// token, it tries dropping a trailing run of punctuation or non-ASCII text
/// (`@a.rs，谢谢`, `@a.rs的代码`), longest candidate first.
fn resolve(token: &str) -> Option<Mention> {
    let mut cuts = vec![token.len()];
    for (i, c) in token.char_indices().rev() {
        if c.is_ascii() && !TRAILING_PUNCTUATION.contains(&c) {
            break;
        }
        if i > 0 {
            cuts.push(i);
        }
    }
    cuts.into_iter()
        .filter_map(|cut| parse_mention(&token[..cut]))
        .find(|m| Path::new(&m.path).exists())
}

/// Splits an optional `#L<start>[-<end>]` suffix off a path.
fn parse_mention(token: &str) -> Option<Mention> {
    let Some((path, range)) = token.rsplit_once("#L") else {
        return Some(Mention { path: token.to_string(), lines: None });
    };
    let (start, end) = match range.split_once('-') {
        Some((a, b)) => (a.parse().ok()?, b.parse().ok()?),
        None => {
            let n = range.parse().ok()?;
            (n, n)
        }
    };
    if start == 0 || end < start {
        return None;
    }
    Some(Mention { path: path.to_string(), lines: Some((start, end)) })
}

fn attach(mention: &Mention, files: &FileTracker) -> Result<Attachment, String> {
    let path = Path::new(&mention.path);
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        return attach_dir(&mention.path);
    }
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let media_type = match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    };
    if let Some(media_type) = media_type {
        let bytes = read_capped(path, meta.len(), MAX_IMAGE_BYTES)?;
        return Ok(Attachment {
            label: format!("{} (image, {})", mention.path, human_size(meta.len())),
            blocks: vec![json!({"type": "text", "text": format!("Image @{}:", mention.path)}), image_block(media_type, &bytes)],
        });
    }
    if ext == "pdf" {
        let bytes = read_capped(path, meta.len(), MAX_PDF_BYTES)?;
        return Ok(Attachment {
            label: format!("{} (PDF, {})", mention.path, human_size(meta.len())),
            blocks: vec![json!({
                "type": "document",
                "title": mention.path,
                "source": {"type": "base64", "media_type": "application/pdf", "data": BASE64.encode(bytes)},
            })],
        });
    }
    attach_text(mention, meta.len(), files)
}

fn image_block(media_type: &str, bytes: &[u8]) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": BASE64.encode(bytes)}})
}

/// What stands for pasted image `n` in the prompt.
pub fn image_placeholder(n: usize) -> String {
    format!("[Image #{n}]")
}

/// Pasted image `n`, introduced by its placeholder.
pub fn pasted_image(n: usize, media_type: &str, bytes: &[u8]) -> Attachment {
    let name = image_placeholder(n);
    Attachment {
        label: format!("{name} (image, {})", human_size(bytes.len() as u64)),
        blocks: vec![json!({"type": "text", "text": format!("{name}:")}), image_block(media_type, bytes)],
    }
}

/// The highest pasted image number in a conversation, so the next one gets
/// a number of its own; 0 if it has none.
pub fn last_pasted_image(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|m| m.role == "user")
        .flat_map(|m| &m.content)
        .filter_map(|b| b["text"].as_str()?.strip_prefix("[Image #")?.strip_suffix("]:")?.parse().ok())
        .max()
        .unwrap_or(0)
}

fn attach_text(mention: &Mention, size: u64, files: &FileTracker) -> Result<Attachment, String> {
    let path = Path::new(&mention.path);
    let bytes = read_capped(path, size, MAX_TEXT_BYTES)?;
    if bytes.contains(&0) {
        return Err("binary file, not attached".into());
    }
    let text = String::from_utf8_lossy(&bytes);
    let total = text.lines().count();
    let (body, label, attr) = match mention.lines {
        Some((start, end)) => {
            let body = number_lines(&text, start, end - start + 1)?;
            let end = end.min(total);
            (body, format!("{} (lines {start}-{end})", mention.path), format!(" lines=\"{start}-{end}\""))
        }
        None if text.is_empty() => ("(empty file)\n".to_string(), format!("{} (empty)", mention.path), String::new()),
        None => {
            let body = number_lines(&text, 1, MAX_TEXT_LINES)?;
            (body, format!("{} ({total} lines)", mention.path), String::new())
        }
    };
    files.record(path);
    Ok(Attachment {
        label,
        blocks: vec![json!({
            "type": "text",
            "text": format!("<file path=\"{}\"{attr}>\n{body}</file>", mention.path),
        })],
    })
}

fn attach_dir(dir: &str) -> Result<Attachment, String> {
    let mut entries: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.path().is_dir() { format!("{name}/") } else { name }
        })
        .collect();
    entries.sort();
    let total = entries.len();
    let mut body = entries.into_iter().take(MAX_DIR_ENTRIES).collect::<Vec<_>>().join("\n");
    if total > MAX_DIR_ENTRIES {
        body.push_str(&format!("\n... [{} more entries]", total - MAX_DIR_ENTRIES));
    }
    Ok(Attachment {
        label: format!("{dir} (directory, {total} entries)"),
        blocks: vec![json!({"type": "text", "text": format!("<directory path=\"{dir}\">\n{body}\n</directory>")})],
    })
}

fn read_capped(path: &Path, size: u64, max: u64) -> Result<Vec<u8>, String> {
    if size > max {
        return Err(format!("{} is larger than the {} limit", human_size(size), human_size(max)));
    }
    std::fs::read(path).map_err(|e| e.to_string())
}

pub fn human_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1024 * 1024 => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
        b if b >= 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `f` inside a fresh temp dir used as the working directory, so
    /// mentions can use relative paths. Tests using it hold a lock because
    /// the working directory is process-wide.
    fn in_temp_dir(f: impl FnOnce(&Path)) {
        let _guard = crate::util::lock_cwd();
        let dir = tempfile::tempdir().unwrap();
        let old = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(dir.path())));
        std::env::set_current_dir(old).unwrap();
        result.unwrap();
    }

    #[test]
    fn finds_tokens() {
        assert_eq!(tokens("look at @a.rs and @\"b c.txt\" please"), vec!["a.rs", "b c.txt"]);
        assert_eq!(tokens("mail me@example.com"), Vec::<String>::new());
        assert_eq!(tokens("@x@y @"), vec!["x@y"]);
        assert_eq!(tokens("看看@src/main.rs，谢谢"), vec!["src/main.rs，谢谢"]);
        assert_eq!(tokens("@\"a @b\"@c"), vec!["a @b", "c"]);
    }

    #[test]
    fn parses_line_ranges() {
        assert_eq!(parse_mention("a.rs#L3-5"), Some(Mention { path: "a.rs".into(), lines: Some((3, 5)) }));
        assert_eq!(parse_mention("a.rs#L7"), Some(Mention { path: "a.rs".into(), lines: Some((7, 7)) }));
        assert_eq!(parse_mention("a.rs#L5-3"), None);
        assert_eq!(parse_mention("a.rs"), Some(Mention { path: "a.rs".into(), lines: None }));
    }

    #[test]
    fn attaches_files_ranges_and_dirs() {
        in_temp_dir(|_| {
            std::fs::write("a.rs", "one\ntwo\nthree\n").unwrap();
            std::fs::create_dir("sub").unwrap();
            std::fs::write("sub/x.txt", "").unwrap();
            let files = FileTracker::default();

            let e = expand("see @a.rs, @a.rs#L2-3 and @sub then @missing.rs", &files);
            assert!(e.warnings.is_empty(), "{:?}", e.warnings);
            let labels: Vec<&str> = e.attachments.iter().map(|a| a.label.as_str()).collect();
            assert_eq!(labels, vec!["a.rs (3 lines)", "a.rs (lines 2-3)", "sub (directory, 1 entries)"]);
            assert_eq!(
                e.attachments[1].blocks[0]["text"],
                "<file path=\"a.rs\" lines=\"2-3\">\n     2\ttwo\n     3\tthree\n</file>"
            );
            assert_eq!(e.attachments[2].blocks[0]["text"], "<directory path=\"sub\">\nx.txt\n</directory>");
            // Attached files count as read for the write tool.
            assert!(files.check_writable(Path::new("a.rs")).is_ok());
        });
    }

    #[test]
    fn trailing_chinese_text_and_punctuation_are_ignored() {
        in_temp_dir(|_| {
            std::fs::write("a.rs", "x\n").unwrap();
            std::fs::write("a", "x\n").unwrap();
            std::fs::write("中文.md", "x\n").unwrap();
            for input in ["看看 @a.rs。", "看看@a.rs的代码", "(@a.rs)"] {
                let e = expand(input, &FileTracker::default());
                let labels: Vec<&str> = e.attachments.iter().map(|a| a.label.as_str()).collect();
                assert_eq!(labels, vec!["a.rs (1 lines)"], "{input}");
            }
            // A missing file is not shortened into an existing one.
            assert!(expand("@a.rsx", &FileTracker::default()).attachments.is_empty());
            assert_eq!(expand("@中文.md", &FileTracker::default()).attachments.len(), 1);
        });
    }

    #[test]
    fn images_and_binaries() {
        in_temp_dir(|_| {
            std::fs::write("shot.png", b"\x89PNG fake").unwrap();
            std::fs::write("blob.bin", b"\x00\x01").unwrap();
            let e = expand("@shot.png @blob.bin", &FileTracker::default());
            assert_eq!(e.attachments.len(), 1);
            let blocks = &e.attachments[0].blocks;
            assert_eq!(blocks[1]["type"], "image");
            assert_eq!(blocks[1]["source"]["media_type"], "image/png");
            assert_eq!(e.warnings, vec!["@blob.bin: binary file, not attached"]);
        });
    }

    #[test]
    fn pasted_images() {
        let image = pasted_image(3, "image/png", b"hi");
        assert_eq!(image.label, "[Image #3] (image, 2 B)");
        assert_eq!(image.blocks[0], json!({"type": "text", "text": "[Image #3]:"}));
        assert_eq!(image.blocks[1]["source"], json!({"type": "base64", "media_type": "image/png", "data": "aGk="}));

        let messages = vec![
            Message::user(image.blocks.clone()),
            Message::user(vec![json!({"type": "text", "text": "[Image #7]: typed, not pasted"})]),
            Message::user(pasted_image(12, "image/png", b"").blocks),
        ];
        assert_eq!(last_pasted_image(&messages), 12);
        assert_eq!(last_pasted_image(&messages[..2]), 3);
        assert_eq!(last_pasted_image(&[]), 0);
    }

    #[test]
    fn content_puts_attachments_before_prompt() {
        in_temp_dir(|_| {
            std::fs::write("a.txt", "hi\n").unwrap();
            let content = expand("explain @a.txt", &FileTracker::default()).into_content("explain @a.txt");
            assert_eq!(content.len(), 2);
            assert!(content[0]["text"].as_str().unwrap().starts_with("<file path=\"a.txt\">"));
            assert_eq!(content[1], json!({"type": "text", "text": "explain @a.txt"}));
        });
    }
}
