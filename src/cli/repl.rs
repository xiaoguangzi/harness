//! Line editing for the REPL: history, a menu under the prompt (commands
//! while a `/command` is typed, files while an `@path` is), Tab completion,
//! and multi-line input (a line ending in `\` continues).
//!
//! The menu is drawn as a multi-line hint. Up/Down move its selection and
//! Tab fills it in; Enter runs a command, or fills in a file. A choice is
//! typed through the completer, which puts the cursor after it.

use std::borrow::Cow;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::{Hint, Hinter};
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{
    Cmd, ConditionalEventHandler, Context, Editor, Event, EventContext, EventHandler, Helper, KeyCode, KeyEvent,
    Modifiers, RepeatCount,
};

use super::menu::{Command, Kind, Menu, list_dir, mention_at_end};
use super::text;
use crate::agent::Inbox;

/// Lets a background subagent's report start a turn while the prompt waits
/// for input. The line editor can't be told to stop reading, so this sends
/// its thread a SIGINT: the editor's handler cuts the read short and it
/// returns as for Ctrl-C. Only while it shows an empty line, so nothing
/// typed is lost.
pub struct Wake {
    inbox: Weak<Inbox>,
    /// Set from the editor's first redraw on, when its SIGINT handler is in
    /// place.
    reading: AtomicBool,
    /// The thread reading, as a `pthread_t`.
    reader: AtomicUsize,
    empty: AtomicBool,
    raised: AtomicBool,
}

impl Wake {
    pub fn new(inbox: &Arc<Inbox>) -> Self {
        Self {
            inbox: Arc::downgrade(inbox),
            reading: AtomicBool::new(false),
            reader: AtomicUsize::new(0),
            empty: AtomicBool::new(false),
            raised: AtomicBool::new(false),
        }
    }

    /// Interrupts the line editor if it shows an empty line and something
    /// in the inbox wants a turn.
    pub fn poke(&self) {
        let wanted = self.inbox.upgrade().is_some_and(|inbox| inbox.wants_turn());
        if wanted
            && self.reading.load(Ordering::SeqCst)
            && self.empty.load(Ordering::SeqCst)
            && !self.raised.swap(true, Ordering::SeqCst)
        {
            // SAFETY: only sends a signal to a thread of this process, one
            // that is still running (the REPL's).
            unsafe { libc::pthread_kill(self.reader.load(Ordering::SeqCst) as libc::pthread_t, libc::SIGINT) };
        }
    }

    /// Called on each redraw of the line being edited, on its thread.
    fn edited(&self, line: &str) {
        self.empty.store(line.is_empty(), Ordering::SeqCst);
        self.reader.store(unsafe { libc::pthread_self() } as usize, Ordering::SeqCst);
        self.reading.store(true, Ordering::SeqCst);
        if line.is_empty() {
            self.poke();
        }
    }

    /// Called when the line editor returns: whether this interrupted it.
    pub fn finish_reading(&self) -> bool {
        self.reading.store(false, Ordering::SeqCst);
        self.raised.swap(false, Ordering::SeqCst)
    }
}

/// The menu, shown below the line being edited.
pub struct MenuHint(String);

impl Hint for MenuHint {
    fn display(&self) -> &str {
        &self.0
    }

    /// Nothing for the Right arrow to insert: Tab fills in the selection.
    fn completion(&self) -> Option<&str> {
        None
    }
}

#[derive(Helper)]
pub struct ReplHelper {
    menu: Arc<Menu>,
    /// Command names, for Tab completion when the menu is not shown.
    names: Vec<String>,
    styled: bool,
    wake: Option<Arc<Wake>>,
}

impl ReplHelper {
    pub fn new(commands: Vec<Command>, styled: bool, wake: Option<Arc<Wake>>) -> Self {
        let names = commands.iter().map(|c| c.name.clone()).collect();
        Self { menu: Arc::new(Menu::new(commands, ".")), names, styled, wake }
    }

    /// Binds the keys that drive the menu.
    pub fn bind_keys<H: Helper, I: rustyline::history::History>(&self, editor: &mut Editor<H, I>) {
        let keys = [(KeyCode::Up, MenuKey::Up), (KeyCode::Down, MenuKey::Down), (KeyCode::Tab, MenuKey::Tab), (KeyCode::Enter, MenuKey::Enter)];
        for (code, key) in keys {
            let handler = MenuKeyHandler { menu: self.menu.clone(), key };
            editor.bind_sequence(KeyEvent(code, Modifiers::NONE), EventHandler::Conditional(Box::new(handler)));
        }
    }

    /// Call before reading a line: files are searched afresh.
    pub fn new_line(&self) {
        self.menu.forget_files();
    }

    /// The command Enter picked from the menu for the last line read.
    pub fn take_accepted(&self) -> Option<String> {
        self.menu.take_accepted()
    }
}

#[derive(Clone, Copy)]
enum MenuKey {
    Up,
    Down,
    Tab,
    Enter,
}

struct MenuKeyHandler {
    menu: Arc<Menu>,
    key: MenuKey,
}

impl ConditionalEventHandler for MenuKeyHandler {
    /// Acts on the menu while it is shown; otherwise the key does its usual
    /// thing (history, completion, accepting the line).
    fn handle(&self, _evt: &Event, _n: RepeatCount, _positive: bool, ctx: &EventContext) -> Option<Cmd> {
        let line = ctx.line();
        if !ctx.has_hint() || ctx.pos() < line.len() {
            return None;
        }
        let (kind, items, selected) = self.menu.selection(line)?;
        let choice = items[selected].text.clone();
        match (self.key, kind) {
            // With nothing to choose between, the arrows keep walking
            // history (which may have recalled a `/command`).
            (MenuKey::Up | MenuKey::Down, _) if items.len() < 2 => return None,
            (MenuKey::Up, _) => self.menu.step(line, -1),
            (MenuKey::Down, _) => self.menu.step(line, 1),
            (MenuKey::Tab, Kind::Command) => {
                self.menu.set_pick(0, format!("{choice} "));
                return Some(Cmd::Complete);
            }
            (MenuKey::Enter, Kind::Command) => {
                if choice != line.trim_end() {
                    self.menu.set_accepted(choice);
                }
                return Some(Cmd::AcceptLine);
            }
            // A path typed out in full is sent as is.
            (MenuKey::Enter, Kind::File { start }) if line[start..] == choice => return None,
            (MenuKey::Tab | MenuKey::Enter, Kind::File { start }) => {
                self.menu.set_pick(start, file_text(&choice));
                return Some(Cmd::Complete);
            }
        }
        Some(Cmd::Repaint)
    }
}

/// What choosing `path` types: a directory stays open for its entries, a
/// file is finished with a space, and a path with spaces is quoted.
fn file_text(path: &str) -> String {
    if path.contains(char::is_whitespace) {
        format!("\"{path}\" ")
    } else if path.ends_with('/') {
        path.to_string()
    } else {
        format!("{path} ")
    }
}

impl Hinter for ReplHelper {
    type Hint = MenuHint;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<MenuHint> {
        // Called on every redraw, so it also tells `wake` what is typed.
        if let Some(wake) = &self.wake {
            wake.edited(line);
        }
        if pos < line.len() {
            return None;
        }
        let width = text::terminal_width().saturating_sub(1);
        self.menu.render(line, width).map(MenuHint)
    }
}

impl Highlighter for ReplHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(&'s self, prompt: &'p str, default: bool) -> Cow<'b, str> {
        if self.styled && default { Cow::Owned(format!("\x1b[1;35m{prompt}\x1b[0m")) } else { Cow::Borrowed(prompt) }
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        match self.menu.styled(hint) {
            Some(styled) if self.styled => Cow::Owned(styled),
            _ => Cow::Borrowed(hint),
        }
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
        // A choice made in the menu is the only candidate, so it is typed.
        if let Some((start, text)) = self.menu.take_pick() {
            return Ok((start, vec![Pair { display: text.clone(), replacement: text }]));
        }
        Ok(complete(line, pos, &self.names, Path::new(".")))
    }
}

/// Relative paths are completed against `base`.
fn complete(line: &str, pos: usize, commands: &[String], base: &Path) -> (usize, Vec<Pair>) {
    let before = &line[..pos];
    if let Some((start, partial)) = mention_at_end(before) {
        let pairs = list_dir(partial, base)
            .into_iter()
            .map(|path| Pair { display: path.rsplit_once('/').map_or(path.clone(), |(_, n)| n.to_string()), replacement: path })
            .collect();
        return (start, pairs);
    }
    if before.starts_with('/') && !before.contains(char::is_whitespace) {
        let pairs = commands
            .iter()
            .filter(|c| c.starts_with(before))
            .map(|c| Pair { display: c.clone(), replacement: format!("{c} ") })
            .collect();
        return (0, pairs);
    }
    (pos, Vec::new())
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
