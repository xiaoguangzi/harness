//! Line editing for the REPL: history, Tab completion of `@paths` and
//! `/commands`, a grey hint completing the command being typed, and
//! multi-line input (a line ending in `\` continues).

use std::borrow::Cow;
use std::path::Path;

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Context, Helper};

const MAX_CANDIDATES: usize = 200;

#[derive(Helper)]
pub struct ReplHelper {
    /// `/help`, `/exit`, ... plus one entry per skill.
    commands: Vec<String>,
    styled: bool,
}

impl ReplHelper {
    pub fn new(commands: Vec<String>, styled: bool) -> Self {
        Self { commands, styled }
    }
}

impl Hinter for ReplHelper {
    type Hint = String;

    /// The rest of the only command that starts with what was typed.
    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        if pos < line.len() || !line.starts_with('/') || line.contains(char::is_whitespace) {
            return None;
        }
        let mut matches = self.commands.iter().filter(|c| c.starts_with(line) && !c.contains(' '));
        match (matches.next(), matches.next()) {
            (Some(only), None) if only.len() > line.len() => Some(only[line.len()..].to_string()),
            _ => None,
        }
    }
}

impl Highlighter for ReplHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(&'s self, prompt: &'p str, default: bool) -> Cow<'b, str> {
        if self.styled && default { Cow::Owned(format!("\x1b[1;35m{prompt}\x1b[0m")) } else { Cow::Borrowed(prompt) }
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        if self.styled { Cow::Owned(format!("\x1b[2m{hint}\x1b[0m")) } else { Cow::Borrowed(hint) }
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        false
    }
}

impl Validator for ReplHelper {
    /// A line ending in `\` continues on the next one.
    fn validate(&self, ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        Ok(if ctx.input().ends_with('\\') { ValidationResult::Incomplete } else { ValidationResult::Valid(None) })
    }
}

/// Joins continued lines: `a\` + newline + `b` -> `a` + newline + `b`.
pub fn join_continuations(input: &str) -> String {
    input.replace("\\\n", "\n")
}

impl Completer for ReplHelper {
    type Candidate = Pair;

    fn complete(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> rustyline::Result<(usize, Vec<Pair>)> {
        Ok(complete(line, pos, &self.commands, Path::new(".")))
    }
}

/// Relative paths are completed against `base`.
fn complete(line: &str, pos: usize, commands: &[String], base: &Path) -> (usize, Vec<Pair>) {
    let before = &line[..pos];
    // The word under the cursor starts after the last whitespace, or at the
    // last `@` that begins a mention (so `看看@src` completes too).
    let word_start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let word = &before[word_start..];

    if let Some(at) = word.rfind('@') {
        let prev = word[..at].chars().next_back();
        if !prev.is_some_and(|c| c.is_ascii_alphanumeric() || "._-+%".contains(c)) {
            let partial = &word[at + 1..];
            return (word_start + at + 1, complete_path(partial, base));
        }
    }
    if word_start == 0 && word.starts_with('/') {
        let pairs = commands
            .iter()
            .filter(|c| c.starts_with(word))
            .map(|c| Pair { display: c.clone(), replacement: format!("{c} ") })
            .collect();
        return (0, pairs);
    }
    (pos, Vec::new())
}

/// Entries of the directory part of `partial` whose names start with the
/// rest. Directories get a trailing `/`; hidden entries are only offered
/// when the typed name starts with `.`.
fn complete_path(partial: &str, base: &Path) -> Vec<Pair> {
    let (dir, prefix) = match partial.rfind('/') {
        Some(i) => (&partial[..=i], &partial[i + 1..]),
        None => ("", partial),
    };
    let Ok(entries) = std::fs::read_dir(base.join(dir)) else {
        return Vec::new();
    };
    let mut pairs: Vec<Pair> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with(prefix) || (name.starts_with('.') && !prefix.starts_with('.')) {
                return None;
            }
            let slash = if e.path().is_dir() { "/" } else { "" };
            Some(Pair { display: format!("{name}{slash}"), replacement: format!("{dir}{name}{slash}") })
        })
        .collect();
    pairs.sort_by(|a, b| a.display.cmp(&b.display));
    pairs.truncate(MAX_CANDIDATES);
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete(line: &str, pos: usize, commands: &[String]) -> (usize, Vec<Pair>) {
        // Complete against this crate's own tree, which has src/ and Cargo.toml.
        super::complete(line, pos, commands, Path::new(env!("CARGO_MANIFEST_DIR")))
    }

    fn replacements(pairs: &[Pair]) -> Vec<&str> {
        pairs.iter().map(|p| p.replacement.as_str()).collect()
    }

    #[test]
    fn completes_commands_at_line_start() {
        let commands = vec!["/help".to_string(), "/exit".to_string(), "/greet".to_string()];
        let (start, pairs) = complete("/g", 2, &commands);
        assert_eq!((start, replacements(&pairs)), (0, vec!["/greet "]));
        let (_, pairs) = complete("say /g", 6, &commands);
        assert!(pairs.is_empty());
    }

    #[test]
    fn completes_paths_after_at() {
        let line = "look at @sr";
        let (start, pairs) = complete(line, line.len(), &[]);
        assert_eq!(start, line.len() - 2);
        assert_eq!(replacements(&pairs), vec!["src/"]);

        let line = "看看@src/ma";
        let (start, pairs) = complete(line, line.len(), &[]);
        assert_eq!(&line[start..], "src/ma");
        assert_eq!(replacements(&pairs), vec!["src/main.rs"]);

        let (_, pairs) = complete("mail me@sr", 10, &[]);
        assert!(pairs.is_empty());
    }

    #[test]
    fn joins_continued_lines() {
        assert_eq!(join_continuations("a\\\nb\\\nc"), "a\nb\nc");
        assert_eq!(join_continuations("C:\\dir"), "C:\\dir");
    }

    #[test]
    fn hides_dotfiles_unless_asked() {
        let (_, pairs) = complete("@", 1, &[]);
        assert!(!replacements(&pairs).iter().any(|r| r.starts_with('.')));
        let (_, pairs) = complete("@.git", 5, &[]);
        assert!(replacements(&pairs).contains(&".gitignore"));
    }
}
