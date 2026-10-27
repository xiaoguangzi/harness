//! Checkpoints, for `/rewind`: each prompt the user sends starts one, and
//! before `edit` or `write` first changes a file after it, the file is kept
//! as it was, so the code can be put back the way it was before any prompt.
//! Changes made any other way (`bash`, an MCP tool, the user's editor) are
//! not tracked.
//!
//! A conversation's checkpoints refer to its user messages by index, and go
//! when those messages do (a rewind, a compaction). The REPL keeps them with
//! the saved session, in `~/.harness/file-history/<session id>/`: a file per
//! kept version, named after its contents, and `checkpoints.json`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::llm::Message;

/// Bumped when `checkpoints.json` changes incompatibly.
const VERSION: u64 = 1;
const LIST_FILE: &str = "checkpoints.json";

tokio::task_local! {
    static CURRENT: Arc<FileHistory>;
}

/// Runs `future`, a turn of the conversation that keeps `history`, so the
/// files its tools change are kept there.
pub async fn scope<F: Future>(history: Arc<FileHistory>, future: F) -> F::Output {
    CURRENT.scope(history, future).await
}

/// Keeps `path` as it is now in the running turn's checkpoint, if this is
/// its first change since; call it before changing the file.
pub async fn before_change(path: &Path) {
    if let Ok(history) = CURRENT.try_with(Clone::clone) {
        history.keep(path).await;
    }
}

/// `~/.harness/file-history/<id>`, where a saved session's checkpoints go.
pub fn session_dir(id: &str) -> Option<PathBuf> {
    crate::util::harness_home().map(|dir| dir.join("file-history").join(id))
}

/// A prompt the conversation can be rewound to.
#[derive(Debug, Clone, PartialEq)]
pub struct Checkpoint {
    /// Index in the history of the user message the prompt starts.
    pub message: usize,
    /// The prompt as the user typed it.
    pub prompt: String,
    /// Unix seconds.
    pub time: u64,
    /// Each file changed from this prompt until the next one, as it was
    /// before: the name of its kept version, or `None` if it did not exist.
    pub files: BTreeMap<PathBuf, Option<String>>,
}

/// A file restoring the code would change.
#[derive(Debug, PartialEq)]
pub struct Change {
    pub path: PathBuf,
    /// What it is put back to; `None` removes it.
    pub before: Option<Arc<Vec<u8>>>,
    /// What it holds now; `None` if it does not exist.
    pub now: Option<Vec<u8>>,
}

/// One conversation's checkpoints.
#[derive(Default)]
pub struct FileHistory {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    checkpoints: Vec<Checkpoint>,
    /// Where kept versions and the checkpoint list are saved, if anywhere.
    dir: Option<PathBuf>,
    /// Kept versions not saved in `dir`, by name.
    unsaved: HashMap<String, Arc<Vec<u8>>>,
}

impl FileHistory {
    /// Starts a checkpoint for the prompt at index `message` of the history;
    /// any at or after it are gone with the messages they referred to.
    pub fn begin(&self, message: usize, prompt: String) {
        let mut state = self.state.lock().unwrap();
        state.checkpoints.retain(|c| c.message < message);
        let time = crate::session::now();
        state.checkpoints.push(Checkpoint { message, prompt, time, files: BTreeMap::new() });
    }

    /// The checkpoints, oldest first.
    pub fn checkpoints(&self) -> Vec<Checkpoint> {
        self.state.lock().unwrap().checkpoints.clone()
    }

    /// Forgets the checkpoints of messages from index `len` on, after the
    /// history was cut there.
    pub fn truncate(&self, len: usize) {
        self.state.lock().unwrap().checkpoints.retain(|c| c.message < len);
    }

    /// After a compaction replaced the history: only the checkpoint of the
    /// prompt at `from`, which is now at `to`, is still there, if any.
    pub fn rebase(&self, from: Option<usize>, to: usize) {
        let mut state = self.state.lock().unwrap();
        state.checkpoints.retain(|c| Some(c.message) == from);
        for c in &mut state.checkpoints {
            c.message = to;
        }
    }

    /// Forgets everything, for a new conversation; it is saved nowhere
    /// until [`FileHistory::set_dir`].
    pub fn clear(&self) {
        *self.state.lock().unwrap() = State::default();
    }

    async fn keep(&self, path: &Path) {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let kept = |state: &State| state.checkpoints.last().is_none_or(|c| c.files.contains_key(&path));
        if kept(&self.state.lock().unwrap()) {
            return;
        }
        let version = match tokio::fs::read(&path).await {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            // The change will fail too.
            Err(_) => return,
        };
        let name = version.as_ref().map(|bytes| version_name(bytes));
        let dir = self.state.lock().unwrap().dir.clone();
        if let (Some(bytes), Some(name)) = (version, &name) {
            let saved = dir.is_some_and(|dir| write_version(&dir, name, &bytes).is_ok());
            if !saved {
                self.state.lock().unwrap().unsaved.insert(name.clone(), Arc::new(bytes));
            }
        }
        let mut state = self.state.lock().unwrap();
        if !kept(&state)
            && let Some(last) = state.checkpoints.last_mut()
        {
            last.files.insert(path, name);
        }
    }

    /// What putting the code back the way it was before the prompt at
    /// `message` would change: every file changed since, as it was before
    /// its first change, unless it is that way now.
    pub fn changes(&self, message: usize) -> Result<Vec<Change>> {
        let mut first: BTreeMap<PathBuf, Option<String>> = BTreeMap::new();
        let (unsaved, dir) = {
            let state = self.state.lock().unwrap();
            for c in state.checkpoints.iter().filter(|c| c.message >= message) {
                for (path, version) in &c.files {
                    first.entry(path.clone()).or_insert_with(|| version.clone());
                }
            }
            (state.unsaved.clone(), state.dir.clone())
        };
        let mut changes = Vec::new();
        for (path, version) in first {
            let before = match version {
                None => None,
                Some(name) => Some(match unsaved.get(&name) {
                    Some(bytes) => bytes.clone(),
                    None => {
                        let file = dir.as_ref().map(|d| d.join(&name));
                        let read = file.and_then(|f| std::fs::read(f).ok());
                        Arc::new(read.with_context(|| format!("the kept version of {} is missing", path.display()))?)
                    }
                }),
            };
            let now = std::fs::read(&path).ok();
            if now.as_deref() != before.as_deref().map(Vec::as_slice) {
                changes.push(Change { path, before, now });
            }
        }
        Ok(changes)
    }

    /// Saves kept versions and the checkpoint list to `dir` from here on,
    /// with those kept so far.
    pub fn set_dir(&self, dir: Option<PathBuf>) {
        let mut state = self.state.lock().unwrap();
        state.dir = dir;
        if let Some(dir) = state.dir.clone() {
            state.unsaved.retain(|name, bytes| write_version(&dir, name, bytes).is_err());
        }
    }

    /// Writes the checkpoint list, and the versions not saved yet, to the
    /// directory set; nothing without one, or before the first checkpoint.
    pub fn save(&self) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut state = self.state.lock().unwrap();
        let Some(dir) = state.dir.clone() else { return Ok(()) };
        let list = dir.join(LIST_FILE);
        if state.checkpoints.is_empty() && !list.exists() {
            return Ok(());
        }
        state.unsaved.retain(|name, bytes| write_version(&dir, name, bytes).is_err());
        let checkpoints: Vec<Value> = state
            .checkpoints
            .iter()
            .map(|c| json!({"message": c.message, "prompt": c.prompt, "time": c.time, "files": c.files}))
            .collect();
        let out = json!({"version": VERSION, "checkpoints": checkpoints}).to_string();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = dir.join(format!("{LIST_FILE}.{}.tmp", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(out.as_bytes()).and_then(|()| file.sync_all())?;
        std::fs::rename(&tmp, &list).with_context(|| format!("writing {}", list.display()))?;
        Ok(())
    }

    /// Reads back the checkpoints saved in `dir` for the conversation
    /// `messages`, and saves to it from here on. Checkpoints that no longer
    /// match a prompt of it are left out.
    pub fn load(&self, dir: PathBuf, messages: &[Message]) -> Result<()> {
        let list = dir.join(LIST_FILE);
        let checkpoints = match std::fs::read_to_string(&list) {
            Ok(text) => parse_list(&text, messages).with_context(|| format!("reading {}", list.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", list.display())),
        };
        *self.state.lock().unwrap() = State { checkpoints, dir: Some(dir), unsaved: HashMap::new() };
        Ok(())
    }
}

fn parse_list(text: &str, messages: &[Message]) -> Result<Vec<Checkpoint>> {
    let v: Value = serde_json::from_str(text)?;
    if v["version"].as_u64() != Some(VERSION) {
        bail!("unsupported version {}", v["version"]);
    }
    let Some(list) = v["checkpoints"].as_array() else { bail!("no checkpoint list") };
    let mut checkpoints = Vec::new();
    for c in list {
        let Some(message) = c["message"].as_u64().map(|m| m as usize) else { continue };
        if messages.get(message).is_none_or(|m| m.role != "user") {
            continue;
        }
        let files = c["files"].as_object().into_iter().flatten();
        let files = files.map(|(path, name)| (PathBuf::from(path), name.as_str().map(String::from))).collect();
        let prompt = c["prompt"].as_str().unwrap_or_default().to_string();
        checkpoints.push(Checkpoint { message, prompt, time: c["time"].as_u64().unwrap_or(0), files });
    }
    Ok(checkpoints)
}

/// Puts back the files in `changes`, going on past failures; says which
/// failed and why.
pub fn restore(changes: &[Change]) -> Result<()> {
    let mut failed = Vec::new();
    for change in changes {
        let path = &change.path;
        let result = match &change.before {
            Some(bytes) => path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(path, bytes.as_slice())),
            None => match std::fs::remove_file(path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                result => result,
            },
        };
        if let Err(e) = result {
            failed.push(format!("{}: {e}", path.display()));
        }
    }
    if !failed.is_empty() {
        bail!("could not restore {}", failed.join("; "));
    }
    Ok(())
}

/// A name for a file's contents: their FNV-1a hash and length, so the same
/// contents are kept once.
fn version_name(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3));
    format!("{hash:016x}-{}", bytes.len())
}

/// Saves a kept version in `dir`, unless it is there already.
fn write_version(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = dir.join(name);
    if path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    file.write_all(bytes)?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Message {
        Message::user(vec![json!({"type": "text", "text": text})])
    }

    fn assistant() -> Message {
        Message { role: "assistant", content: vec![json!({"type": "text", "text": "ok"})], source: None }
    }

    async fn change(history: &Arc<FileHistory>, path: &Path, contents: Option<&str>) {
        scope(history.clone(), before_change(path)).await;
        match contents {
            Some(text) => std::fs::write(path, text).unwrap(),
            None => std::fs::remove_file(path).unwrap(),
        }
    }

    #[tokio::test]
    async fn restores_files_as_they_were_before_a_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.txt"), dir.path().join("b.txt"));
        std::fs::write(&a, "a0").unwrap();
        let history = Arc::new(FileHistory::default());

        // Before the first prompt, nothing is kept.
        change(&history, &a, Some("a1")).await;
        history.begin(0, "one".into());
        change(&history, &a, Some("a2")).await;
        change(&history, &a, Some("a3")).await;
        history.begin(2, "two".into());
        change(&history, &b, Some("b1")).await;
        change(&history, &a, Some("a4")).await;
        history.begin(4, "three".into());

        let files: Vec<usize> = history.checkpoints().iter().map(|c| c.files.len()).collect();
        assert_eq!(files, [1, 2, 0]);
        let to_two = history.changes(2).unwrap();
        assert_eq!(to_two.len(), 2);
        assert_eq!(to_two[0].before.as_deref().map(Vec::as_slice), Some(&b"a3"[..]));
        assert_eq!((to_two[1].before.as_ref(), to_two[1].now.as_deref()), (None, Some(&b"b1"[..])));
        restore(&history.changes(0).unwrap()).unwrap();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "a1");
        assert!(!b.exists());
        assert!(history.changes(0).unwrap().is_empty(), "nothing left to restore");
        assert!(history.changes(4).unwrap().is_empty());
    }

    #[tokio::test]
    async fn checkpoints_go_with_their_messages() {
        let history = FileHistory::default();
        history.begin(0, "one".into());
        history.begin(2, "two".into());
        history.begin(4, "three".into());
        history.truncate(4);
        assert_eq!(history.checkpoints().len(), 2);
        // A turn removed by an error, then started again.
        history.begin(2, "two again".into());
        let prompts: Vec<String> = history.checkpoints().into_iter().map(|c| c.prompt).collect();
        assert_eq!(prompts, ["one", "two again"]);
        history.rebase(Some(2), 1);
        let kept = history.checkpoints();
        assert_eq!((kept.len(), kept[0].message, kept[0].prompt.as_str()), (1, 1, "two again"));
        history.rebase(None, 0);
        assert!(history.checkpoints().is_empty());
    }

    #[tokio::test]
    async fn saves_and_loads_with_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let (file, saved) = (dir.path().join("f.txt"), dir.path().join("history"));
        std::fs::write(&file, "v0").unwrap();
        let history = Arc::new(FileHistory::default());
        history.begin(0, "one".into());
        change(&history, &file, Some("v1")).await;
        history.save().unwrap();
        assert!(!saved.exists(), "nowhere to save yet");
        history.set_dir(Some(saved.clone()));
        history.begin(2, "two".into());
        change(&history, &file, Some("v2")).await;
        history.save().unwrap();

        use std::os::unix::fs::PermissionsExt;
        let list = saved.join(LIST_FILE);
        assert_eq!(std::fs::metadata(&list).unwrap().permissions().mode() & 0o777, 0o600);

        let loaded = FileHistory::default();
        let messages = [user("one"), assistant(), user("two"), assistant()];
        loaded.load(saved.clone(), &messages).unwrap();
        assert_eq!(loaded.checkpoints(), history.checkpoints());
        restore(&loaded.changes(0).unwrap()).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v0");

        // Checkpoints of messages the session no longer has are left out.
        let shorter = FileHistory::default();
        shorter.load(saved, &messages[..2]).unwrap();
        assert_eq!(shorter.checkpoints().len(), 1);
    }
}
