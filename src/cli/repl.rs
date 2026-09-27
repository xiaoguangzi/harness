//! Line editing for the REPL: history, a menu under the prompt (commands
//! while a `/command` is typed, files while an `@path` is), Tab completion,
//! multi-line input (a line ending in `\` continues) and pasting images with
//! Ctrl+V.
//!
//! On a terminal the line is drawn in the input box: the prompt carries the
//! rule above it, and the hint the rule below. The menu is drawn as more of
//! the hint. The box sits on the screen's bottom rows, with blank rows
//! between it and the output until the output fills the screen; as the box
//! grows (the menu opens, say) it takes those rows before the screen
//! scrolls, and as it shrinks it moves back down. Up/Down move the menu's
//! selection and Tab fills it in; Enter runs a command, or fills in a file.
//! A choice is typed through the completer, which puts the cursor after it.

use std::borrow::Cow;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

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
    pasted: Arc<Mutex<Pasted>>,
    /// Command names, for Tab completion when the menu is not shown.
    names: Vec<String>,
    /// Whether the line is drawn in the input box, with styles.
    styled: bool,
    wake: Option<Arc<Wake>>,
    /// The line and hint last drawn.
    shown: Mutex<(String, String)>,
    /// Keeps the box on the bottom rows, while a line is read.
    pin: Mutex<Option<Pin>>,
}

/// Where the input box is, to keep it on the screen's bottom rows.
struct Pin {
    /// The screen row the output goes on from: the rows from there to the
    /// box are blank.
    end: usize,
    /// Rows the line editor last drew, from the prompt's first row down to
    /// the screen's last.
    rows: usize,
    /// Rows to move the box down by once it is drawn.
    lower: usize,
    /// The terminal's size; after a resize nothing is known.
    size: (usize, usize),
}

impl ReplHelper {
    pub fn new(commands: Vec<Command>, styled: bool, wake: Option<Arc<Wake>>) -> Self {
        let names = commands.iter().map(|c| c.name.clone()).collect();
        let menu = Arc::new(Menu::new(commands, "."));
        Self { menu, pasted: Arc::default(), names, styled, wake, shown: Mutex::default(), pin: Mutex::default() }
    }

    /// Puts the input box of the next line read, starting with `initial`,
    /// on the screen's bottom rows, given that the output goes on from the
    /// start of screen row `end`, where the cursor is. The output scrolls up
    /// if there isn't room.
    pub fn pin(&self, end: usize, initial: &str) {
        let (width, Some(height)) = (text::terminal_width(), text::terminal_height()) else { return };
        if !self.styled {
            return;
        }
        let shown = format!("{}{initial}\n{}", self.prompt(), input::rule(width));
        let rows = rows(&shown, width).min(height);
        let top = height - rows;
        let mut end = end.min(height - 1);
        let mut screen = String::new();
        if end > top {
            screen.push_str(&input::goto(height - 1));
            screen.push_str(&"\n".repeat(end - top));
            end = top;
        }
        screen.push_str(&input::goto(top));
        print!("{screen}");
        let _ = std::io::stdout().flush();
        *self.pin.lock().unwrap() = Some(Pin { end, rows, lower: 0, size: (width, height) });
    }

    /// Once the line editor returns: the screen row the output goes on
    /// from, if the box was kept on the bottom rows (and the terminal kept
    /// its size).
    pub fn unpin(&self) -> Option<usize> {
        self.pin.lock().unwrap().take().map(|pin| pin.end)
    }

    /// The prompt. On a terminal, it starts with a row for the input box's
    /// top rule, which the highlighter draws at the terminal's width as it
    /// is when the line is redrawn, so the rule follows a resized window.
    pub fn prompt(&self) -> String {
        if self.styled { format!("\n{}", input::PROMPT) } else { input::PROMPT.to_string() }
    }

    /// Rows from the top of `prompt` to where the cursor is once the line
    /// editor returns, with `line` accepted (hint gone) or cut short
    /// (`None`; hint left on screen). They are laid out the way the editor
    /// does, at the terminal's width.
    pub fn rows_used(&self, prompt: &str, line: Option<&str>) -> usize {
        let shown = self.shown.lock().unwrap();
        let (line, hint) = match line {
            Some(line) => (line, ""),
            None => (shown.0.as_str(), shown.1.as_str()),
        };
        rows(&format!("{prompt}{line}{hint}"), text::terminal_width())
    }

    /// The images pasted into the line being edited are numbered after
    /// this.
    pub fn last_image(&self) -> usize {
        self.pasted.lock().unwrap().last
    }

    /// Binds the keys that drive the menu, and Ctrl+V.
    pub fn bind_keys<H: Helper, I: rustyline::history::History>(&self, editor: &mut Editor<H, I>) {
        let keys = [(KeyCode::Up, MenuKey::Up), (KeyCode::Down, MenuKey::Down), (KeyCode::Tab, MenuKey::Tab), (KeyCode::Enter, MenuKey::Enter)];
        for (code, key) in keys {
            let handler = MenuKeyHandler { menu: self.menu.clone(), key };
            editor.bind_sequence(KeyEvent(code, Modifiers::NONE), EventHandler::Conditional(Box::new(handler)));
        }
        let paste = PasteHandler { pasted: self.pasted.clone() };
        editor.bind_sequence(KeyEvent::ctrl('V'), EventHandler::Conditional(Box::new(paste)));
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
        // Called on every redraw, so it also tells `wake` what is typed.
        if let Some(wake) = &self.wake {
            wake.edited(line);
        }
        let below = self.below(line, pos);
        // The input box's bottom rule comes first, on a row of its own. A
        // line that fills its last row leaves the cursor there, waiting to
        // wrap: the editor counts a newline for that, and the rule's own
        // newline would be one too many.
        let hint = if self.styled {
            let width = text::terminal_width();
            let full = fills_last_row(&format!("{}{line}", input::PROMPT), width);
            Some(format!("{}{}{}", if full { "" } else { "\n" }, input::rule(width), below.unwrap_or_default()))
        } else {
            below
        };
        let shown = hint.clone().unwrap_or_default();
        self.keep_pinned(&format!("{}{line}{shown}", self.prompt()));
        *self.shown.lock().unwrap() = (line.to_string(), shown);
        hint.map(MenuHint)
    }
}

impl ReplHelper {
    /// Before the line editor draws `shown` in place of what it drew last:
    /// a taller box first moves up into the blank rows above it, as far as
    /// they go (the screen scrolls for the rest); a shorter one moves down
    /// once drawn.
    fn keep_pinned(&self, shown: &str) {
        let mut pin = self.pin.lock().unwrap();
        let Some(p) = pin.as_mut() else { return };
        let (width, Some(height)) = (text::terminal_width(), text::terminal_height()) else { return };
        if (width, height) != p.size {
            *pin = None;
            return;
        }
        let rows = rows(shown, width).min(height);
        if rows > p.rows {
            let grow = rows - p.rows;
            let up = grow.min((height - p.rows).saturating_sub(p.end));
            print!("{}", input::close_gap(p.end, up));
            let _ = std::io::stdout().flush();
            p.end -= grow - up;
        } else {
            p.lower = p.rows - rows;
        }
        p.rows = rows;
    }
}

impl Highlighter for ReplHelper {
    /// The editor lays the prompt out as given; the rule drawn into its
    /// first row fits that row whatever its length.
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(&'s self, prompt: &'p str, default: bool) -> Cow<'b, str> {
        if !self.styled || !default || prompt != self.prompt() {
            return Cow::Borrowed(prompt);
        }
        let rule = input::rule(text::terminal_width());
        Cow::Owned(format!("\x1b[2m{rule}\x1b[0m\n\x1b[{}m❯\x1b[0m ", input::PROMPT_SGR))
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        if !self.styled {
            return Cow::Borrowed(hint);
        }
        // The rule, then what `below` gave.
        let body = hint.strip_prefix('\n').unwrap_or(hint);
        let (rule, below) = body.split_at(body.find('\n').unwrap_or(body.len()));
        let below = if below.is_empty() {
            String::new()
        } else if let Some(styled) = self.menu.styled(below) {
            styled
        } else if self.pasted.lock().unwrap().notice.as_ref().is_some_and(|(_, shown)| shown == below) {
            format!("\x1b[33m{below}\x1b[0m")
        } else {
            below.to_string()
        };
        // The line editor moves the cursor from the drawing's end
        // relatively, so it follows the box moving down.
        let lower = match self.pin.lock().unwrap().as_mut() {
            Some(pin) => input::open_gap(pin.end, std::mem::take(&mut pin.lower)),
            None => String::new(),
        };
        Cow::Owned(format!("\n\x1b[2m{rule}\x1b[0m{below}{lower}"))
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

/// Rows `shown` takes when the line editor draws it at `cols` columns, from
/// its first row to the one under its end.
fn rows(shown: &str, cols: usize) -> usize {
    let (row, col) = layout(shown, cols);
    // A full last row puts the end at the start of the next.
    row + usize::from(col == cols) + 1
}

/// Whether `shown` ends exactly at the end of a row.
fn fills_last_row(shown: &str, cols: usize) -> bool {
    layout(shown, cols).1 == cols
}

/// The row and column where `shown` ends at `cols` columns, by the line
/// editor's own layout rules.
pub(super) fn layout(shown: &str, cols: usize) -> (usize, usize) {
    let (mut row, mut col) = (0, 0);
    for c in shown.chars() {
        if c == '\n' {
            row += 1;
            col = 0;
            continue;
        }
        let w = if c == '\t' { 8 - col % 8 } else { text::char_width(c) };
        col += w;
        if col > cols {
            row += 1;
            col = w;
        }
    }
    (row, col)
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
    fn counts_rows_the_way_the_editor_lays_them_out() {
        assert_eq!(rows("───\n❯ hi", 10), 2);
        assert_eq!(rows("───\n❯ hi\n───", 10), 3);
        // 8 columns fill a row exactly; the end moves to the next.
        assert_eq!(rows("❯ 看看看", 8), 2);
        assert_eq!(rows("❯ 看看看看", 8), 2);
        assert_eq!(rows("❯ ab\ncd", 8), 2);
        assert!(fills_last_row("❯ 看看看", 8));
        assert!(!fills_last_row("❯ 看看看\n", 8));
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
