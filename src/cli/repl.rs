//! Command/file completion and input history. Interactive input is drawn
//! by Output for both idle prompts and running turns; rustyline remains
//! the fallback for redirected input and conventional command prompts.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::{Hint, Hinter};
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{
    Cmd, ConditionalEventHandler, Context, Editor, Event, EventContext, EventHandler, Helper, KeyCode, KeyEvent,
    Modifiers, RepeatCount,
};

use super::clipboard::{self, Clip, Image};
use super::input;
use super::menu::{Command, Kind, Menu, list_dir, mention_at_end};
use super::text;
use crate::agent::Inbox;
use crate::mentions;

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
    pasted: Arc<Mutex<Pasted>>,
    /// Command names, for Tab completion when the menu is not shown.
    names: Mutex<Vec<String>>,
}

impl ReplHelper {
    pub fn new(commands: Vec<Command>) -> Self {
        let names = Mutex::new(commands.iter().map(|c| c.name.clone()).collect());
        let menu = Arc::new(Menu::new(commands, "."));
        Self { menu, pasted: Arc::default(), names }
    }

    /// Reads the interactive composer through the same renderer used by a
    /// running turn. No cursor reports or line-editor layout guesses.
    pub async fn read_box(
        &self,
        out: &super::output::Output,
        typing: &mut super::input::Typing,
        inbox: Arc<Inbox>,
        history: Vec<String>,
    ) -> Result<Option<(String, Vec<(usize, Image)>)>, rustyline::error::ReadlineError> {
        use super::keys::{Key, KeyWatch};
        use rustyline::error::ReadlineError;
        self.menu.forget_files();
        out.open_input(std::mem::take(typing), inbox.clone());
        out.set_menu(Some(self.menu.clone()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let watch = KeyWatch::for_prompt(move |key| { let _ = tx.send(key); });
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        let mut history_at = history.len();
        let mut saved = String::new();
        let mut last_esc: Option<Instant> = None;
        let result = loop {
            let key = tokio::select! {
                _ = tokio::signal::ctrl_c() => break Err(ReadlineError::Interrupted),
                key = rx.recv() => match key { Some(key) => key, None => break Err(ReadlineError::Eof) },
                _ = tick.tick() => {
                    if inbox.wants_turn() && out.input_is_empty() {
                        break Ok(None);
                    }
                    continue;
                }
            };
            if out.tool_output_key(&key) { continue; }
            if key == Key::PasteImage {
                super::paste_clipboard(out);
                continue;
            }
            let action = out.edit_input(|t| {
                let draft = &mut t.draft;
                if key == Key::Eof && draft.is_empty() {
                    return Some(Err(ReadlineError::Eof));
                }
                if key == Key::Esc {
                    if draft.is_empty() {
                        if last_esc.take().is_some_and(|at| at.elapsed() < DOUBLE_ESC) {
                            return Some(Ok(Some(("/rewind".into(), Vec::new()))));
                        }
                        last_esc = Some(Instant::now());
                    }
                    return None;
                }
                last_esc = None;
                if let Some(line) = draft.at_end()
                    && let Some((kind, items, selected)) = self.menu.selection(line)
                {
                    let choice = items[selected].text.clone();
                    match (&key, kind) {
                        (Key::Up | Key::Down, _) if items.len() > 1 => {
                            self.menu.step(line, if key == Key::Up { -1 } else { 1 });
                            return None;
                        }
                        (Key::Tab, Kind::Command) => {
                            draft.complete(0, &format!("{choice} "));
                            return None;
                        }
                        (Key::Enter, Kind::Command) => {
                            draft.set_text(&choice);
                        }
                        (Key::Tab | Key::Enter, Kind::File { start }) if key == Key::Tab || line[start..] != choice => {
                            draft.complete(start, &file_text(&choice));
                            return None;
                        }
                        _ => {}
                    }
                }
                match key {
                    Key::Enter => draft.submit().map(|message| Ok(Some(message))),
                    Key::Tab => {
                        let (start, choices) = complete(draft.text(), draft.position(), &self.names.lock().unwrap(), Path::new("."));
                        if let [choice] = choices.as_slice() { draft.complete(start, &choice.replacement); }
                        None
                    }
                    Key::Up if draft.on_first_line() && history_at > 0 => {
                        if history_at == history.len() { saved = draft.text().to_string(); }
                        history_at -= 1;
                        draft.set_text(&history[history_at]);
                        None
                    }
                    Key::Down if draft.on_last_line() && history_at < history.len() => {
                        history_at += 1;
                        draft.set_text(if history_at == history.len() { &saved } else { &history[history_at] });
                        None
                    }
                    key => { draft.edit(key); None }
                }
            }).flatten();
            if let Some(action) = action { break action; }
        };
        drop(watch);
        out.set_menu(None);
        *typing = out.close_input();
        if result.is_err() {
            typing.draft = super::input::Draft::default();
        }
        result
    }

    /// Offers `commands` from now on, e.g. after the plugins were reloaded.
    pub fn set_commands(&self, commands: Vec<Command>) {
        *self.names.lock().unwrap() = commands.iter().map(|c| c.name.clone()).collect();
        self.menu.set_commands(commands);
    }

    pub fn prompt(&self) -> String { input::PROMPT.to_string() }

    /// The images pasted into the line being edited are numbered after
    /// this.
    pub fn last_image(&self) -> usize {
        self.pasted.lock().unwrap().last
    }

    /// Binds the keys that drive the menu, Ctrl+V, and Esc.
    pub fn bind_keys<H: Helper, I: rustyline::history::History>(&self, editor: &mut Editor<H, I>) {
        let keys = [(KeyCode::Up, MenuKey::Up), (KeyCode::Down, MenuKey::Down), (KeyCode::Tab, MenuKey::Tab), (KeyCode::Enter, MenuKey::Enter)];
        for (code, key) in keys {
            let handler = MenuKeyHandler { menu: self.menu.clone(), key };
            editor.bind_sequence(KeyEvent(code, Modifiers::NONE), EventHandler::Conditional(Box::new(handler)));
        }
        let paste = PasteHandler { pasted: self.pasted.clone() };
        editor.bind_sequence(KeyEvent::ctrl('V'), EventHandler::Conditional(Box::new(paste)));
        let esc = EscHandler { menu: self.menu.clone(), last: Mutex::new(None) };
        editor.bind_sequence(KeyEvent(KeyCode::Esc, Modifiers::NONE), EventHandler::Conditional(Box::new(esc)));
    }

    /// Call before reading a line: files are searched afresh, and images
    /// pasted into it are numbered after `last_image`, the conversation's
    /// last. `images` were pasted into the text it starts with.
    pub fn new_line(&self, last_image: usize, images: Vec<(usize, Image)>) {
        self.menu.forget_files();
        let last = images.iter().map(|(n, _)| *n).fold(last_image, usize::max);
        *self.pasted.lock().unwrap() = Pasted { images, last, notice: None };
    }

    /// The images pasted into the last line read, by number.
    pub fn take_images(&self) -> Vec<(usize, Image)> {
        std::mem::take(&mut self.pasted.lock().unwrap().images)
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

/// How soon a second Esc must follow the first to count as Esc Esc.
const DOUBLE_ESC: Duration = Duration::from_millis(800);

/// Esc twice at an empty prompt runs `/rewind`.
struct EscHandler {
    menu: Arc<Menu>,
    /// When Esc was last pressed at an empty prompt.
    last: Mutex<Option<Instant>>,
}

impl ConditionalEventHandler for EscHandler {
    fn handle(&self, _evt: &Event, _n: RepeatCount, _positive: bool, ctx: &EventContext) -> Option<Cmd> {
        let mut last = self.last.lock().unwrap();
        if !ctx.line().is_empty() {
            *last = None;
            return None;
        }
        match last.take() {
            Some(at) if at.elapsed() < DOUBLE_ESC => {
                self.menu.set_accepted("/rewind".to_string());
                Some(Cmd::AcceptLine)
            }
            _ => {
                *last = Some(Instant::now());
                Some(Cmd::Noop)
            }
        }
    }
}

/// Images pasted into the line being edited. Each shows in it as
/// `[Image #n]`, numbered on from the conversation's, so the model can tell
/// them apart from earlier ones.
#[derive(Default)]
struct Pasted {
    images: Vec<(usize, Image)>,
    last: usize,
    /// Why Ctrl+V pasted nothing, shown under the line until it changes:
    /// (the line, the hint).
    notice: Option<(String, String)>,
}

struct PasteHandler {
    pasted: Arc<Mutex<Pasted>>,
}

impl ConditionalEventHandler for PasteHandler {
    /// Types a placeholder for the clipboard's image, or `@` mentions of the
    /// files copied; otherwise says why nothing was pasted.
    fn handle(&self, _evt: &Event, _n: RepeatCount, _positive: bool, ctx: &EventContext) -> Option<Cmd> {
        let clip = clipboard::read();
        let mut pasted = self.pasted.lock().unwrap();
        let text = match clip {
            Ok(Some(Clip::Image(image))) => {
                pasted.last += 1;
                let n = pasted.last;
                pasted.images.push((n, image));
                mentions::image_placeholder(n)
            }
            Ok(Some(Clip::Files(paths))) => file_mentions(&paths, &std::env::current_dir().unwrap_or_default()),
            Ok(None) => {
                let key = if cfg!(target_os = "macos") { "Cmd+V" } else { "Ctrl+Shift+V" };
                pasted.notice = Some((ctx.line().to_string(), format!("\n  no image in the clipboard (text pastes with {key})")));
                return Some(Cmd::Repaint);
            }
            Err(e) => {
                pasted.notice = Some((ctx.line().to_string(), format!("\n  ! {e}")));
                return Some(Cmd::Repaint);
            }
        };
        Some(Cmd::Insert(1, spaced(ctx.line(), ctx.pos(), &text)))
    }
}

/// `@` mentions of `paths`, relative to `cwd` when inside it.
pub(super) fn file_mentions(paths: &[PathBuf], cwd: &Path) -> String {
    let mentions: Vec<String> = paths
        .iter()
        .map(|path| {
            let path = match path.strip_prefix(cwd) {
                Ok(rel) if rel.as_os_str().is_empty() => ".".into(),
                Ok(rel) => rel.to_string_lossy(),
                Err(_) => path.to_string_lossy(),
            };
            if path.contains(char::is_whitespace) { format!("@\"{path}\"") } else { format!("@{path}") }
        })
        .collect();
    mentions.join(" ")
}

/// `text` to insert at byte `pos` of `line`, set off from the words around
/// it by spaces.
pub(super) fn spaced(line: &str, pos: usize, text: &str) -> String {
    let before = line[..pos].chars().next_back().is_some_and(|c| !c.is_whitespace());
    let after = !line[pos..].starts_with(char::is_whitespace);
    format!("{}{text}{}", if before { " " } else { "" }, if after { " " } else { "" })
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

impl ReplHelper {
    /// What shows under the line: why Ctrl+V pasted nothing, or the menu.
    fn below(&self, line: &str, pos: usize) -> Option<String> {
        {
            let mut pasted = self.pasted.lock().unwrap();
            match &pasted.notice {
                Some((shown_for, hint)) if shown_for == line => return Some(hint.clone()),
                Some(_) => pasted.notice = None,
                None => {}
            }
        }
        if pos < line.len() {
            return None;
        }
        let width = text::terminal_width().saturating_sub(1);
        self.menu.render(line, width)
    }
}

impl Hinter for ReplHelper {
    type Hint = MenuHint;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<MenuHint> {
        self.below(line, pos).map(MenuHint)
    }
}

impl Highlighter for ReplHelper {
    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool { false }
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
        Ok(complete(line, pos, &self.names.lock().unwrap(), Path::new(".")))
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
    fn pasted_text_is_set_off_by_spaces() {
        assert_eq!(spaced("", 0, "[Image #1]"), "[Image #1] ");
        assert_eq!(spaced("look at", 7, "[Image #1]"), " [Image #1] ");
        assert_eq!(spaced("look at ", 8, "[Image #1]"), "[Image #1] ");
        assert_eq!(spaced("a b", 1, "x"), " x");
        assert_eq!(spaced("看看", "看".len(), "x"), " x ");
    }

    #[test]
    fn copied_files_become_mentions() {
        let cwd = Path::new("/work/repo");
        let paths = [PathBuf::from("/work/repo/src/a.rs"), PathBuf::from("/tmp/My Shot.png"), PathBuf::from("/work/repo")];
        assert_eq!(file_mentions(&paths, cwd), "@src/a.rs @\"/tmp/My Shot.png\" @.");
    }

    #[test]
    fn hides_dotfiles_unless_asked() {
        let (_, pairs) = complete("@", 1, &[]);
        assert!(!replacements(&pairs).iter().any(|r| r.starts_with('.')));
        let (_, pairs) = complete("@.git", 5, &[]);
        assert!(replacements(&pairs).contains(&".gitignore"));
    }
}
