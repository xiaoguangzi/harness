//! The menu shown under the prompt: commands while a `/command` is typed,
//! files while an `@path` mention is.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::text;

/// Menu rows shown at once; the list scrolls with the selection.
const MENU_ROWS: usize = 8;
/// Files offered for one `@` query.
const MAX_FILE_ITEMS: usize = 50;
/// Paths indexed for `@` search.
const MAX_INDEX: usize = 50_000;
/// Directories not indexed when walking a tree that is not a git checkout.
const SKIP_DIRS: [&str; 2] = ["target", "node_modules"];

/// A command offered in the menu: `/model`, `/model gw/gpt-5`, `/<skill>`.
pub struct Command {
    pub name: String,
    pub description: String,
}

impl Command {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self { name: name.into(), description: description.into() }
    }
}

/// A menu row: the text choosing it types, and an optional description.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub text: String,
    pub description: String,
}

/// What the menu is completing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    /// The whole line is a `/command`.
    Command,
    /// An `@path` mention whose path starts at byte `start`.
    File { start: usize },
}

pub struct Menu {
    commands: Vec<Command>,
    /// Relative paths are listed and searched against this directory.
    base: PathBuf,
    /// Project files and directories, built on the first `@` search.
    index: Mutex<Option<Arc<Index>>>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The line the selection and cached items belong to.
    line: String,
    items: Option<(Kind, Vec<Item>)>,
    selected: usize,
    /// The last menu drawn, as (plain, styled) text.
    drawn: (String, String),
    /// A choice to type: (start byte, text), for the completer.
    pick: Option<(usize, String)>,
    /// The command Enter ran, when it differs from what was typed.
    accepted: Option<String>,
}

impl Menu {
    pub fn new(commands: Vec<Command>, base: impl Into<PathBuf>) -> Self {
        Self { commands, base: base.into(), index: Mutex::default(), state: Mutex::default() }
    }

    /// Drops the file index, so the next `@` search sees new files.
    pub fn forget_files(&self) {
        *self.index.lock().unwrap() = None;
    }

    /// What the menu offers for `line` (the text up to the cursor), with
    /// the selected row. Typing a different line resets the selection.
    pub fn selection(&self, line: &str) -> Option<(Kind, Vec<Item>, usize)> {
        let mut state = self.state.lock().unwrap();
        if state.line != line || state.items.is_none() {
            state.items = Some(self.items(line));
            state.line = line.to_string();
            state.selected = 0;
        }
        let (kind, items) = state.items.clone()?;
        if items.is_empty() {
            return None;
        }
        let selected = state.selected.min(items.len() - 1);
        Some((kind, items, selected))
    }

    /// Moves the selection by `step`, wrapping around.
    pub fn step(&self, line: &str, step: isize) {
        let Some((_, items, selected)) = self.selection(line) else { return };
        let count = items.len() as isize;
        self.state.lock().unwrap().selected = (selected as isize + step).rem_euclid(count) as usize;
    }

    pub fn set_pick(&self, start: usize, text: String) {
        self.state.lock().unwrap().pick = Some((start, text));
    }

    pub fn take_pick(&self) -> Option<(usize, String)> {
        self.state.lock().unwrap().pick.take()
    }

    pub fn set_accepted(&self, command: String) {
        self.state.lock().unwrap().accepted = Some(command);
    }

    pub fn take_accepted(&self) -> Option<String> {
        self.state.lock().unwrap().accepted.take()
    }

    /// The styled form of the menu last rendered as `plain`.
    pub fn styled(&self, plain: &str) -> Option<String> {
        let state = self.state.lock().unwrap();
        (state.drawn.0 == plain).then(|| state.drawn.1.clone())
    }

    /// The menu for `line` as plain rows, each after a newline; the styled
    /// form is kept for [`Menu::styled`].
    pub fn render(&self, line: &str, width: usize) -> Option<String> {
        let (_, items, selected) = self.selection(line)?;
        let start = (selected + 1).saturating_sub(MENU_ROWS);
        let has_descriptions = items.iter().any(|i| !i.description.is_empty());
        let name_width = if has_descriptions {
            items.iter().map(|i| text::width(&i.text)).max().unwrap_or(0).min((width / 2).max(16))
        } else {
            width.saturating_sub(2)
        };
        let (mut plain, mut styled) = (String::new(), String::new());
        for (i, item) in items.iter().enumerate().skip(start).take(MENU_ROWS) {
            let name = text::fit(&item.text, name_width);
            let room = width.saturating_sub(name_width + 6);
            let description = text::fit(item.description.lines().next().unwrap_or(""), room);
            let name = if has_descriptions { text::pad(&name, name_width) } else { name };
            let row = format!("  {name}  {description}");
            plain.push('\n');
            plain.push_str(row.trim_end());
            styled.push('\n');
            styled.push_str(&if i == selected {
                format!("  \x1b[1;35m{name}\x1b[0m  {description}")
            } else {
                format!("  {name}  \x1b[2m{description}\x1b[0m")
            });
        }
        let more = items.len().saturating_sub(start + MENU_ROWS);
        if more > 0 {
            let row = format!("  ↓ {more} more");
            plain.push_str(&format!("\n{row}"));
            styled.push_str(&format!("\n\x1b[2m{row}\x1b[0m"));
        }
        self.state.lock().unwrap().drawn = (plain.clone(), styled.trim_end().to_string());
        Some(plain)
    }

    fn items(&self, line: &str) -> (Kind, Vec<Item>) {
        if line.starts_with('/') && !line.contains('\n') {
            return (Kind::Command, self.command_items(line));
        }
        match mention_at_end(line) {
            Some((start, partial)) => (Kind::File { start }, self.file_items(partial)),
            None => (Kind::Command, Vec::new()),
        }
    }

    /// Commands starting with `line`. Arguments are offered once a space
    /// was typed (`/model ` lists models).
    fn command_items(&self, line: &str) -> Vec<Item> {
        let words = line.split(' ').count();
        self.commands
            .iter()
            .filter(|c| c.name.starts_with(line) && c.name.split(' ').count() == words)
            .map(|c| Item { text: c.name.clone(), description: c.description.clone() })
            .collect()
    }

    /// Paths for an `@` mention: the entries of the directory typed so far
    /// that start with the rest, then project files matching it anywhere.
    /// Entries git ignores are left out, unless that directory is typed.
    fn file_items(&self, partial: &str) -> Vec<Item> {
        let index = self.index();
        let dir = partial.rfind('/').map_or("", |i| &partial[..=i]);
        let mut paths = list_dir(partial, &self.base);
        if index.from_git && (dir.is_empty() || index.set.contains(dir)) {
            paths.retain(|p| index.set.contains(p));
        }
        if !partial.is_empty() {
            for path in rank_paths(&index.paths, partial) {
                if paths.len() >= MAX_FILE_ITEMS {
                    break;
                }
                if !paths.contains(path) {
                    paths.push(path.clone());
                }
            }
        }
        paths.truncate(MAX_FILE_ITEMS);
        paths.into_iter().map(|text| Item { text, description: String::new() }).collect()
    }

    fn index(&self) -> Arc<Index> {
        let mut index = self.index.lock().unwrap();
        index.get_or_insert_with(|| Arc::new(Index::build(&self.base))).clone()
    }
}

/// The `@` mention being typed at the end of `line`: where its path starts,
/// and the path so far. An `@` after an e-mail-style character is not one.
pub fn mention_at_end(line: &str) -> Option<(usize, &str)> {
    let word_start = line.char_indices().rev().find(|(_, c)| c.is_whitespace()).map_or(0, |(i, c)| i + c.len_utf8());
    let word = &line[word_start..];
    let at = word.rfind('@')?;
    let prev = word[..at].chars().next_back();
    if prev.is_some_and(|c| c.is_ascii_alphanumeric() || "._-+%".contains(c)) {
        return None;
    }
    let partial = &word[at + 1..];
    (!partial.starts_with('"')).then_some((word_start + at + 1, partial))
}

/// Entries of the directory part of `partial` whose names start with the
/// rest, as paths to type. Directories get a trailing `/`; hidden entries
/// are only offered when the typed name starts with `.`.
pub fn list_dir(partial: &str, base: &Path) -> Vec<String> {
    let (dir, prefix) = match partial.rfind('/') {
        Some(i) => (&partial[..=i], &partial[i + 1..]),
        None => ("", partial),
    };
    let Ok(entries) = std::fs::read_dir(base.join(dir)) else {
        return Vec::new();
    };
    let mut paths: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with(prefix) || (name.starts_with('.') && !prefix.starts_with('.')) {
                return None;
            }
            let slash = if e.path().is_dir() { "/" } else { "" };
            Some(format!("{dir}{name}{slash}"))
        })
        .collect();
    paths.sort();
    paths.truncate(MAX_FILE_ITEMS);
    paths
}

/// Paths matching `query`, best first: those starting with it, then those
/// whose file name starts with or contains it, then those containing it,
/// then those containing its characters in order. Shorter paths win ties.
fn rank_paths<'a>(paths: &'a [String], query: &str) -> Vec<&'a String> {
    let query = query.to_lowercase();
    let mut scored: Vec<(u8, usize, &String)> = paths
        .iter()
        .filter_map(|path| {
            let lower = path.to_lowercase();
            let trimmed = lower.trim_end_matches('/');
            let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
            let tier = if lower.starts_with(&query) {
                0
            } else if name.starts_with(&query) {
                1
            } else if name.contains(&query) {
                2
            } else if lower.contains(&query) {
                3
            } else if is_subsequence(&query, &lower) {
                4
            } else {
                return None;
            };
            Some((tier, path.len(), path))
        })
        .collect();
    scored.sort();
    scored.into_iter().take(MAX_FILE_ITEMS).map(|(_, _, p)| p).collect()
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut rest = haystack.chars();
    needle.chars().all(|c| rest.any(|h| h == c))
}

/// The project's files and their directories (with a trailing `/`),
/// relative to the base directory.
struct Index {
    paths: Vec<String>,
    set: HashSet<String>,
    /// Whether git listed the files, so what is missing is ignored.
    from_git: bool,
}

impl Index {
    /// What git tracks or would track, or else a walk that skips hidden
    /// and build directories.
    fn build(base: &Path) -> Self {
        let git = std::process::Command::new("git")
            .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
            .current_dir(base)
            .stderr(std::process::Stdio::null())
            .output();
        let (mut paths, from_git): (Vec<String>, bool) = match git {
            Ok(out) if out.status.success() => (
                String::from_utf8_lossy(&out.stdout)
                    .split('\0')
                    .filter(|f| !f.is_empty())
                    .take(MAX_INDEX)
                    .map(String::from)
                    .collect(),
                true,
            ),
            _ => (walk(base), false),
        };
        let dirs: BTreeSet<String> = paths
            .iter()
            .flat_map(|f| f.match_indices('/').map(|(i, _)| f[..=i].to_string()).collect::<Vec<_>>())
            .collect();
        paths.extend(dirs);
        let set = paths.iter().cloned().collect();
        Self { paths, set, from_git }
    }
}

fn walk(base: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(base.join(&dir)) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = dir.join(&name);
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(path);
                }
            } else {
                files.push(path.to_string_lossy().into_owned());
                if files.len() >= MAX_INDEX {
                    return files;
                }
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu() -> Menu {
        let commands = ["/help", "/model", "/mcp", "/clear", "/model gw/a", "/model gw/b"]
            .iter()
            .map(|n| Command::new(*n, format!("about {n}")))
            .collect();
        Menu::new(commands, env!("CARGO_MANIFEST_DIR"))
    }

    fn texts(line: &str) -> Vec<String> {
        menu().selection(line).map(|(_, items, _)| items.into_iter().map(|i| i.text).collect()).unwrap_or_default()
    }

    #[test]
    fn commands_match_by_prefix() {
        assert_eq!(texts("/"), vec!["/help", "/model", "/mcp", "/clear"]);
        assert_eq!(texts("/m"), vec!["/model", "/mcp"]);
        assert_eq!(texts("/model "), vec!["/model gw/a", "/model gw/b"]);
        assert!(texts("hello").is_empty());
        assert!(texts("/nope").is_empty());
    }

    #[test]
    fn selection_wraps_and_resets_on_typing() {
        let m = menu();
        let selected = |line: &str| m.selection(line).map(|(_, items, i)| items[i].text.clone());
        assert_eq!(selected("/m").as_deref(), Some("/model"));
        m.step("/m", 1);
        assert_eq!(selected("/m").as_deref(), Some("/mcp"));
        m.step("/m", 1);
        assert_eq!(selected("/m").as_deref(), Some("/model"));
        m.step("/m", -1);
        assert_eq!(selected("/m").as_deref(), Some("/mcp"));
        assert_eq!(selected("/mc").as_deref(), Some("/mcp"));
        assert_eq!(selected("/").as_deref(), Some("/help"));
    }

    #[test]
    fn renders_rows_and_overflow() {
        let m = menu();
        let plain = m.render("/", 40).unwrap();
        assert_eq!(plain, "\n  /help   about /help\n  /model  about /model\n  /mcp    about /mcp\n  /clear  about /clear");
        assert!(m.styled(&plain).unwrap().contains("\x1b[1;35m/help"));
        assert!(m.render("/zzz", 40).is_none());
    }

    #[test]
    fn finds_mentions_being_typed() {
        assert_eq!(mention_at_end("look at @src/ma"), Some((9, "src/ma")));
        assert_eq!(mention_at_end("看看@src"), Some(("看看@".len(), "src")));
        assert_eq!(mention_at_end("@"), Some((1, "")));
        assert_eq!(mention_at_end("mail me@ex"), None);
        assert_eq!(mention_at_end("@src/main.rs "), None);
        assert_eq!(mention_at_end("@\"a b"), None);
    }

    #[test]
    fn files_list_the_directory_then_search_the_project() {
        let (kind, items, _) = menu().selection("see @").unwrap();
        assert_eq!(kind, Kind::File { start: 5 });
        let texts: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();
        assert!(texts.contains(&"src/") && texts.contains(&"Cargo.toml"), "{texts:?}");
        assert!(!texts.iter().any(|t| t.starts_with('.')));
        // Ignored by git, unless browsed into.
        assert!(!texts.contains(&"target/"), "{texts:?}");
        assert!(texts_of(&menu(), "@target/").iter().any(|t| t.starts_with("target/")));

        let found = texts_of(&menu(), "@markdown");
        assert_eq!(found.first().map(String::as_str), Some("src/cli/markdown.rs"));
        let found = texts_of(&menu(), "@src/ma");
        assert_eq!(found.first().map(String::as_str), Some("src/main.rs"));
    }

    fn texts_of(m: &Menu, line: &str) -> Vec<String> {
        m.selection(line).map(|(_, items, _)| items.into_iter().map(|i| i.text).collect()).unwrap_or_default()
    }

    #[test]
    fn ranks_paths() {
        let paths: Vec<String> =
            ["src/main.rs", "src/cli/", "src/cli/mod.rs", "docs/domain.md", "Cargo.toml", "src/cli/markdown.rs"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let rank = |q: &str| rank_paths(&paths, q).into_iter().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(rank("src/c"), vec!["src/cli/", "src/cli/mod.rs", "src/cli/markdown.rs"]);
        assert_eq!(rank("ma"), vec!["src/main.rs", "src/cli/markdown.rs", "docs/domain.md"]);
        assert_eq!(rank("cli"), vec!["src/cli/", "src/cli/mod.rs", "src/cli/markdown.rs"]);
        assert_eq!(rank("smr"), vec!["src/main.rs", "src/cli/mod.rs", "src/cli/markdown.rs"]);
        assert!(rank("zzz").is_empty());
    }
}
