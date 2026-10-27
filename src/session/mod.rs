//! Saved conversations, so a later run can pick one up again
//! (`--continue`, `--resume`, `/resume`).
//!
//! Each conversation is a JSON Lines file in `~/.harness/sessions/`: a
//! header line (id, working directory, title, model, times), then one
//! message per line in the history's Anthropic form. The file is rewritten
//! after every turn, through a temporary file, so it is never left half
//! written; listing reads only the header lines. Conversations can hold
//! file contents and command output, so the files are readable by their
//! owner only.

pub mod checkpoint;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::llm::Message;

/// Bumped when the file layout changes incompatibly.
const VERSION: u64 = 1;

/// What a session file's header says about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Meta {
    pub id: String,
    /// The working directory it ran in; sessions are listed per directory.
    pub cwd: String,
    /// The first prompt, cut to one short line.
    pub title: String,
    /// The `provider/model` in use when it was last saved.
    pub model: String,
    /// Unix seconds.
    pub created: u64,
    pub updated: u64,
    pub messages: usize,
}

/// Where sessions are saved.
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `~/.harness/sessions`, or nothing without a home directory.
    pub fn default_location() -> Option<Self> {
        crate::util::harness_home().map(|dir| Self::new(dir.join("sessions")))
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.jsonl"))
    }

    /// Writes the conversation, replacing any earlier save of it.
    pub fn save(&self, meta: &Meta, messages: &[Message]) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let path = self.path(&meta.id);
        // Per process, so two runs saving the same session never share one.
        let tmp = self.dir.join(format!("{}.jsonl.{}.tmp", meta.id, std::process::id()));
        let header = json!({
            "version": VERSION,
            "id": meta.id,
            "cwd": meta.cwd,
            "title": meta.title,
            "model": meta.model,
            "created": meta.created,
            "updated": meta.updated,
            "messages": messages.len(),
        });
        let mut out = String::new();
        for line in std::iter::once(header).chain(messages.iter().map(message_json)) {
            out.push_str(&line.to_string());
            out.push('\n');
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(out.as_bytes()).and_then(|()| file.sync_all())?;
        std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Sessions saved from `cwd`, most recently updated first. Unreadable
    /// files are skipped.
    pub fn list(&self, cwd: &str) -> Vec<Meta> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut found: Vec<Meta> = entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .filter_map(|e| read_header(&e.path()).ok())
            .filter(|m| m.cwd == cwd)
            .collect();
        found.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| b.id.cmp(&a.id)));
        found
    }

    /// Finds a session by its id or a unique prefix of it.
    pub fn find(&self, id: &str) -> Result<PathBuf> {
        let exact = self.path(id);
        if exact.is_file() {
            return Ok(exact);
        }
        let entries = std::fs::read_dir(&self.dir).map(|e| e.flatten().collect::<Vec<_>>()).unwrap_or_default();
        let mut matches: Vec<PathBuf> = entries
            .into_iter()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .filter(|p| p.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.starts_with(id)))
            .collect();
        match matches.len() {
            _ if id.is_empty() => bail!("no session id given"),
            0 => bail!("no saved session `{id}`"),
            1 => Ok(matches.remove(0)),
            n => bail!("`{id}` matches {n} sessions; give more of the id"),
        }
    }

    /// Reads a session back: its header and its history.
    pub fn load(&self, id: &str) -> Result<(Meta, Vec<Message>)> {
        let path = self.find(id)?;
        let file = std::fs::File::open(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut lines = BufReader::new(file).lines();
        let header = lines.next().context("empty session file")??;
        let meta = parse_header(&header)?;
        let mut messages = Vec::new();
        for (i, line) in lines.enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&line).with_context(|| format!("{}: message {}", path.display(), i + 1))?;
            messages.push(parse_message(value).with_context(|| format!("{}: message {}", path.display(), i + 1))?);
        }
        Ok((meta, messages))
    }
}

/// Saves the conversation in progress after each turn: a new one, or one
/// that was resumed and continues under its id.
pub struct Recorder {
    store: Store,
    meta: Meta,
}

impl Recorder {
    pub fn new(store: Store, cwd: String) -> Self {
        let meta = Meta { id: String::new(), cwd, title: String::new(), model: String::new(), created: 0, updated: 0, messages: 0 };
        let mut recorder = Self { store, meta };
        recorder.restart();
        recorder
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    /// The file the conversation is saved to (once it has a message).
    pub fn file(&self) -> PathBuf {
        self.store.path(&self.meta.id)
    }

    /// Records a new conversation from here on, e.g. after `/clear`; the
    /// previous one stays saved.
    pub fn restart(&mut self) {
        self.meta = Meta { id: new_id(), created: now(), title: String::new(), ..self.meta.clone() };
    }

    /// Continues recording a resumed conversation, now in this directory.
    pub fn resume(&mut self, meta: Meta) {
        self.meta = Meta { cwd: std::mem::take(&mut self.meta.cwd), ..meta };
    }

    /// Titles the conversation after its first prompt.
    pub fn set_title(&mut self, line: &str) {
        let first = line.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
        self.meta.title = crate::util::ellipsize(first, 80);
    }

    /// Saves the conversation as it is now. Nothing is written until it
    /// has a message; one rewound to its start is no longer saved.
    pub fn save(&mut self, messages: &[Message], model: &str) -> Result<()> {
        if messages.is_empty() {
            return match std::fs::remove_file(self.file()) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).context("removing the saved conversation"),
                _ => Ok(()),
            };
        }
        self.meta.model = model.to_string();
        self.meta.updated = now();
        self.meta.messages = messages.len();
        self.store.save(&self.meta, messages)
    }
}

fn read_header(path: &Path) -> Result<Meta> {
    let mut line = String::new();
    BufReader::new(std::fs::File::open(path)?).read_line(&mut line)?;
    parse_header(&line)
}

fn parse_header(line: &str) -> Result<Meta> {
    let v: Value = serde_json::from_str(line).context("bad session header")?;
    if v["version"].as_u64() != Some(VERSION) {
        bail!("unsupported session version {}", v["version"]);
    }
    let text = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let id = text("id");
    if id.is_empty() {
        bail!("session header has no id");
    }
    Ok(Meta {
        id,
        cwd: text("cwd"),
        title: text("title"),
        model: text("model"),
        created: v["created"].as_u64().unwrap_or(0),
        updated: v["updated"].as_u64().unwrap_or(0),
        messages: v["messages"].as_u64().unwrap_or(0) as usize,
    })
}

fn message_json(m: &Message) -> Value {
    let mut v = json!({"role": m.role, "content": m.content});
    if let Some(source) = &m.source {
        v["source"] = json!(source);
    }
    v
}

fn parse_message(v: Value) -> Result<Message> {
    let role = match v["role"].as_str() {
        Some("user") => "user",
        Some("assistant") => "assistant",
        other => bail!("unknown role {other:?}"),
    };
    let Some(content) = v["content"].as_array() else { bail!("message has no content") };
    Ok(Message { role, content: content.clone(), source: v["source"].as_str().map(String::from) })
}

/// Unix seconds now.
pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// A new session id: the UTC start time, sortable, plus a random suffix
/// (`20260925-143012-3fa9`).
pub fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (y, m, d) = civil_from_days((secs / 86400) as i64);
    let t = secs % 86400;
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(since.as_nanos());
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}-{:04x}", t / 3600, t / 60 % 60, t % 60, h.finish() & 0xffff)
}

/// Days since 1970-01-01 to a (year, month, day) date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `90` seconds ago -> `1m ago`.
pub fn ago(then: u64, now: u64) -> String {
    let secs = now.saturating_sub(then);
    match secs {
        0..60 => "just now".to_string(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

/// How a run should pick up an earlier conversation.
#[derive(Debug, Clone, PartialEq)]
pub enum Resume {
    /// `--continue`: the latest one in this directory.
    Latest,
    /// `--resume` alone: choose from a list.
    Pick,
    /// `--resume <id>`.
    Id(String),
}

/// Takes `-c`/`--continue` and `-r`/`--resume [id]` out of the command
/// line; the rest is the one-shot prompt. `--resume` takes the next
/// argument as the id, so a one-shot prompt after it needs the id too.
pub fn parse_args(args: Vec<String>) -> Result<(Option<Resume>, Vec<String>)> {
    let mut resume = None;
    let mut rest = Vec::new();
    let mut args = args.into_iter().peekable();
    while let Some(arg) = args.next() {
        let this = match arg.as_str() {
            "-c" | "--continue" => Resume::Latest,
            "-r" | "--resume" => match args.next_if(|a| !a.starts_with('-')) {
                Some(id) => Resume::Id(id),
                None => Resume::Pick,
            },
            _ => match arg.strip_prefix("--resume=") {
                Some(id) => Resume::Id(id.to_string()),
                None => {
                    rest.push(arg);
                    continue;
                }
            },
        };
        if resume.replace(this).is_some() {
            bail!("give only one of --continue and --resume");
        }
    }
    Ok((resume, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str, cwd: &str, updated: u64) -> Meta {
        Meta {
            id: id.into(),
            cwd: cwd.into(),
            title: "fix it".into(),
            model: "gw/m".into(),
            created: 1,
            updated,
            messages: 0,
        }
    }

    #[test]
    fn saves_and_loads_a_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("sessions"));
        let messages = vec![
            Message::user(vec![json!({"type": "text", "text": "hi"})]),
            Message {
                role: "assistant",
                content: vec![json!({"type": "thinking", "thinking": "", "signature": "s"}), json!({"type": "text", "text": "yo"})],
                source: Some("gw/m".into()),
            },
        ];
        store.save(&meta("20260925-100000-aaaa", "/p", 5), &messages).unwrap();
        let (m, loaded) = store.load("20260925-100000").unwrap();
        assert_eq!(m, Meta { messages: 2, ..meta("20260925-100000-aaaa", "/p", 5) });
        assert_eq!(loaded.len(), 2);
        assert_eq!((loaded[1].role, &loaded[1].content, &loaded[1].source), ("assistant", &messages[1].content, &messages[1].source));
        assert_eq!(loaded[0].source, None);

        use std::os::unix::fs::PermissionsExt;
        let file = store.find("20260925-100000-aaaa").unwrap();
        assert_eq!(std::fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn lists_this_directory_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let one = [Message::user(vec![json!({"type": "text", "text": "hi"})])];
        store.save(&meta("a", "/p", 5), &one).unwrap();
        store.save(&meta("b", "/p", 9), &one).unwrap();
        store.save(&meta("c", "/other", 7), &one).unwrap();
        std::fs::write(dir.path().join("junk.jsonl"), "not json\n").unwrap();
        let ids: Vec<String> = store.list("/p").into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["b", "a"]);
    }

    #[test]
    fn finds_sessions_by_unique_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save(&meta("20260925-100000-aaaa", "/p", 1), &[]).unwrap();
        store.save(&meta("20260925-110000-bbbb", "/p", 1), &[]).unwrap();
        assert!(store.find("20260925-11").is_ok());
        assert!(store.find("20260925").unwrap_err().to_string().contains("matches 2"));
        assert!(store.find("nope").is_err());
        assert!(store.find("").is_err());
    }

    #[test]
    fn makes_sortable_ids() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_721), (2026, 9, 25));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        let id = new_id();
        assert_eq!(id.len(), 20, "{id}");
    }

    #[test]
    fn parses_resume_flags() {
        let args = |a: &[&str]| parse_args(a.iter().map(|s| s.to_string()).collect()).unwrap();
        assert_eq!(args(&["fix", "it"]), (None, vec!["fix".into(), "it".into()]));
        assert_eq!(args(&["-c", "go", "on"]), (Some(Resume::Latest), vec!["go".into(), "on".into()]));
        assert_eq!(args(&["--resume"]), (Some(Resume::Pick), vec![]));
        assert_eq!(args(&["--resume", "abc", "go"]), (Some(Resume::Id("abc".into())), vec!["go".into()]));
        assert_eq!(args(&["--resume=abc"]), (Some(Resume::Id("abc".into())), vec![]));
        assert!(parse_args(vec!["-r".into(), "-c".into()]).is_err());
    }
}
