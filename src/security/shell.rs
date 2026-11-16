//! Reading a `bash` command for the permission rules: splitting a compound
//! command into its commands, stripping the wrappers rules look past, and
//! telling the commands that only read.

/// The commands of a compound `bash` command, split where the shell runs one
/// after another or pipes one into the next (`&&`, `||`, `;`, `|`, `|&`, `&`,
/// newlines), outside quotes; `None` if it cannot be split with certainty (a
/// comment, a heredoc, `$'…'`), or runs a command substitution (`$(…)`,
/// backticks, `<(…)`) that no rule can be said to allow. Redirections (`2>&1`,
/// `&>`) do not split, nor newlines in quotes or after a backslash.
pub(crate) fn commands(command: &str) -> Option<Vec<&str>> {
    let bytes = command.as_bytes();
    let mut parts = Vec::new();
    let (mut start, mut i) = (0, 0);
    // Whether a command must follow the last operator (`&&`, `||`, `|`).
    let mut wants_command = false;
    // An empty part is fine after `;` or `&`, not where a command must follow.
    fn push<'a>(parts: &mut Vec<&'a str>, part: &'a str, wants: bool) -> Option<()> {
        let part = part.trim();
        if !part.is_empty() {
            parts.push(part);
        }
        (!part.is_empty() || !wants).then_some(())
    }
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'\'' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'\'' {
                    i += 1;
                }
                if i == bytes.len() {
                    return None;
                }
            }
            b'"' => {
                i += 1;
                loop {
                    match bytes.get(i)? {
                        b'"' => break,
                        b'\\' => i += 1,
                        b'`' => return None,
                        b'$' if bytes.get(i + 1) == Some(&b'(') => return None,
                        _ => {}
                    }
                    i += 1;
                }
            }
            b'`' => return None,
            // ANSI-C quoting and heredocs: not followed here.
            b'$' if matches!(bytes.get(i + 1), Some(b'(' | b'\'')) => return None,
            c @ (b'<' | b'>') => match bytes.get(i + 1) {
                Some(b'(') => return None,
                Some(b'<') if c == b'<' => return None,
                // `>&`, `<&`: the `&` is the redirection's.
                Some(b'&') => i += 1,
                _ => {}
            },
            // `&>`: a redirection.
            b'&' if bytes.get(i + 1) == Some(&b'>') => {}
            // A comment, which may hide a quote or a line's end.
            b'#' if i == 0 || bytes[i - 1].is_ascii_whitespace() || b";&|()<>".contains(&bytes[i - 1]) => return None,
            c @ (b'&' | b'|' | b';' | b'\n') => {
                push(&mut parts, &command[start..i], wants_command)?;
                let doubled = matches!(c, b'&' | b'|') && bytes.get(i + 1) == Some(&c);
                let piped_stderr = c == b'|' && bytes.get(i + 1) == Some(&b'&');
                if doubled || piped_stderr {
                    i += 1;
                }
                wants_command = doubled || c == b'|';
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    push(&mut parts, &command[start..], wants_command)?;
    Some(parts)
}

/// Variables a leading assignment of which an allow rule looks past
/// (`NODE_ENV=test npm test`): they change what a program prints or how it
/// logs, not what it runs. `PATH`, `NODE_OPTIONS`, `GIT_*`, `LD_*` and the
/// like are not here: they can make a command run something else.
const SAFE_VARS: &[&str] = &[
    "CARGO_TERM_COLOR", "CI", "CLICOLOR", "CLICOLOR_FORCE", "COLUMNS", "DEBUG", "FORCE_COLOR", "LANG", "LANGUAGE",
    "LC_ALL", "LC_CTYPE", "NO_COLOR", "NODE_ENV", "PYTHONDONTWRITEBYTECODE", "PYTHONUNBUFFERED", "RUST_BACKTRACE",
    "RUST_LOG", "RUST_TEST_THREADS", "TERM", "TZ",
];

/// One command (a part of a compound one) without the wrappers that run
/// their arguments as the command, as Claude Code strips them before
/// matching rules: `timeout`, `time`, `nice`, `nohup`, `stdbuf`, `command`
/// (not `command -v`), `builtin`, zsh's `noglob`, `xargs` without options,
/// and leading assignments: of [`SAFE_VARS`] only for an allow rule, of any
/// variable (`any_assignment`) for a deny or ask rule. A wrapper whose
/// options are not plain is left on, so the command matches only rules for
/// it as written.
pub(crate) fn strip_wrappers(command: &str, any_assignment: bool) -> &str {
    strip(command, any_assignment, true)
}

fn strip(command: &str, any_assignment: bool, xargs: bool) -> &str {
    let mut rest = command.trim();
    while let Some(inner) = strip_one(rest, any_assignment, xargs).map(str::trim_start) {
        if inner.is_empty() {
            break;
        }
        rest = inner;
    }
    rest
}

/// What follows the first word of `command`, if it is a wrapper (with its
/// options) or an assignment that [`strip_wrappers`] strips.
fn strip_one(command: &str, any_assignment: bool, xargs: bool) -> Option<&str> {
    let (word, mut rest) = split_word(command);
    if let Some((name, value)) = word.split_once('=') {
        let name_ok = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        let safe = any_assignment || (SAFE_VARS.contains(&name) && !value.contains(['`', '$']));
        return (name_ok && safe).then_some(rest);
    }
    // Takes an option's value, attached (`-n5`, `--signal=KILL`) or as the next word.
    let value = |attached: &str, rest: &mut &str, ok: fn(&str) -> bool| -> bool {
        let value = if attached.is_empty() {
            let (next, after) = split_word(rest);
            *rest = after;
            next
        } else {
            attached
        };
        ok(value)
    };
    let number = |v: &str| v.strip_prefix(['-', '+']).unwrap_or(v).parse::<u32>().is_ok();
    let duration = |v: &str| {
        let digits = v.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(v);
        !digits.is_empty() && digits.parse::<f64>().is_ok_and(|d| d >= 0.0) && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
    };
    let plain = |v: &str| !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric());
    let next_is_option = |rest: &str| split_word(rest).0.starts_with('-');
    match word {
        "nohup" | "builtin" | "noglob" => Some(rest),
        "xargs" if !xargs => None,
        "command" | "xargs" => (!next_is_option(rest)).then_some(rest),
        "time" => match split_word(rest) {
            ("-p", after) => Some(after),
            _ => Some(rest),
        },
        "nice" => {
            let (option, after) = split_word(rest);
            if let Some(attached) = option.strip_prefix("--adjustment=") {
                return number(attached).then_some(after);
            }
            match option.strip_prefix("-n") {
                Some(attached) => {
                    rest = after;
                    value(attached, &mut rest, number).then_some(rest)
                }
                None if option.starts_with('-') => number(option).then_some(after),
                None => Some(rest),
            }
        }
        "timeout" => {
            loop {
                let (option, after) = split_word(rest);
                if !option.starts_with('-') {
                    break;
                }
                rest = after;
                let ok = match option {
                    "--preserve-status" | "--foreground" | "-v" | "--verbose" => true,
                    _ if option.starts_with("--signal=") => plain(&option[9..]),
                    _ if option.starts_with("--kill-after=") => duration(&option[13..]),
                    _ if option.starts_with("-s") => value(&option[2..], &mut rest, plain),
                    _ if option.starts_with("-k") => value(&option[2..], &mut rest, duration),
                    _ => false,
                };
                if !ok {
                    return None;
                }
            }
            let (limit, after) = split_word(rest);
            duration(limit).then_some(after)
        }
        "stdbuf" => {
            let mode = |v: &str| v == "L" || v.parse::<u64>().is_ok() || v.get(..v.len().saturating_sub(1)).is_some_and(|n| n.parse::<u64>().is_ok());
            loop {
                let (option, after) = split_word(rest);
                if !option.starts_with('-') {
                    return Some(rest);
                }
                rest = after;
                let ok = match option.split_once('=') {
                    Some(("--input" | "--output" | "--error", v)) => mode(v),
                    _ if matches!(option.get(..2), Some("-i" | "-o" | "-e")) => value(&option[2..], &mut rest, mode),
                    _ => false,
                };
                if !ok {
                    return None;
                }
            }
        }
        _ => None,
    }
}

/// The first word of `command` and what follows it; a word runs to
/// whitespace outside quotes.
fn split_word(command: &str) -> (&str, &str) {
    let command = command.trim_start();
    let bytes = command.as_bytes();
    let mut i = 0;
    let mut quote = None;
    while i < bytes.len() {
        match (quote, bytes[i]) {
            (None, b'\\') => i += 1,
            (None, b'\'' | b'"') => quote = Some(bytes[i]),
            (Some(b'"'), b'\\') => i += 1,
            (Some(q), c) if c == q => quote = None,
            (None, c) if c.is_ascii_whitespace() => break,
            _ => {}
        }
        i += 1;
    }
    let i = i.min(bytes.len());
    (&command[..i], &command[i..])
}

/// A word of a command as the shell passes it on, quotes removed.
#[derive(Debug, PartialEq)]
pub(crate) struct Word {
    pub text: String,
    /// Whether it has unquoted glob or brace characters, which the shell may
    /// expand to other words (a file named `-delete`, say).
    pub glob: bool,
}

/// The words of one command (a part of a compound one), unquoted, leaving
/// out redirections to `/dev/null` and between file descriptors (`2>&1`).
/// `None` when what the command does cannot be read from its text: it
/// redirects to or from a file, expands a variable or a command (`$`,
/// backticks), starts with `~`, or has a subshell or group.
pub(crate) fn plain_words(command: &str) -> Option<Vec<Word>> {
    let mut words = Vec::new();
    let mut word = Word { text: String::new(), glob: false };
    // Whether `word` has been started (`''` is an empty word).
    let mut started = false;
    let mut chars = command.chars().peekable();
    fn finish(words: &mut Vec<Word>, word: &mut Word, started: &mut bool) {
        if std::mem::replace(started, false) {
            words.push(std::mem::replace(word, Word { text: String::new(), glob: false }));
        }
    }
    while let Some(c) = chars.next() {
        match c {
            // Only what the shell splits at: not a no-break space, say.
            c if c.is_ascii_whitespace() => finish(&mut words, &mut word, &mut started),
            '\\' => match chars.next()? {
                // A line continued.
                '\n' => {}
                c => {
                    word.text.push(c);
                    started = true;
                }
            },
            '\'' => {
                started = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => word.text.push(c),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '$' | '`' => return None,
                        '\\' => match chars.next()? {
                            c @ ('"' | '\\' | '$' | '`') => word.text.push(c),
                            '\n' => {}
                            c => word.text.extend(['\\', c]),
                        },
                        c => word.text.push(c),
                    }
                }
            }
            '$' | '`' | '(' | ')' | ';' | '|' => return None,
            '~' if !started => return None,
            '>' | '<' | '&' => {
                // `2>`: the digits name the descriptor; `&>`: both outputs.
                if started && word.text.chars().all(|c| c.is_ascii_digit()) && !word.glob {
                    word.text.clear();
                    started = false;
                }
                finish(&mut words, &mut word, &mut started);
                let output = match c {
                    '&' if chars.next_if_eq(&'>').is_some() => true,
                    '&' => return None,
                    c => c == '>',
                };
                if output {
                    // `>>`, `>|`.
                    chars.next_if(|&c| c == '>' || c == '|');
                }
                if chars.next_if_eq(&'&').is_some() {
                    // `>&2`, `<&3`, `>&-`: to another descriptor, not a file.
                    if chars.next_if_eq(&'-').is_none() && chars.next_if(char::is_ascii_digit).is_none() {
                        return None;
                    }
                    while chars.next_if(char::is_ascii_digit).is_some() {}
                    continue;
                }
                while chars.next_if(|c| c.is_ascii_whitespace() && *c != '\n').is_some() {}
                let mut target = String::new();
                while let Some(c) = chars.next_if(|c| !c.is_ascii_whitespace()) {
                    target.push(c);
                }
                if target != "/dev/null" {
                    return None;
                }
            }
            c => {
                word.glob |= matches!(c, '*' | '?' | '[' | '{');
                word.text.push(c);
                started = true;
            }
        }
    }
    finish(&mut words, &mut word, &mut started);
    Some(words)
}

/// Commands that read and print, never change anything, whatever their
/// options: they run without asking, as Claude Code's built-in set does.
const READ_ONLY: &[&str] = &[
    "basename", "cat", "cut", "diff", "dirname", "du", "echo", "grep", "head", "ls", "pwd", "realpath", "stat",
    "tail", "tr", "wc",
];

/// `git` subcommands that only read, with any options but those in
/// [`GIT_WRITES`].
const GIT_READS: &[&str] = &[
    "blame", "cat-file", "describe", "diff", "grep", "log", "ls-files", "ls-tree", "merge-base", "rev-list",
    "rev-parse", "show", "status",
];

/// `git` options that write a file or run a program.
const GIT_WRITES: &[&str] = &["--output", "--ext-diff", "-O", "--open-files-in-pager"];

/// `find` actions that run a command, delete or write a file, or read the
/// paths to search from one.
const FIND_ACTS: &[&str] =
    &["-delete", "-exec", "-execdir", "-ok", "-okdir", "-fls", "-fprint", "-fprint0", "-fprintf", "-files0-from"];

/// Whether `command`, one command of a compound one, only reads: one of
/// [`READ_ONLY`], a `find` without actions, a reading form of `git`, `which`,
/// or a `cd` that stays in the working directory (`moves`: whether another
/// command of the compound one changes directory, after which `git` could
/// run another repository's hooks). `denied(Some(path))` says whether a
/// `Read` deny rule covers a path, which the command then may not name;
/// `denied(None)` whether there is any, a glob then being too wide to check.
pub(crate) fn reads_only(command: &str, moves: bool, denied: &dyn Fn(Option<&str>) -> bool) -> bool {
    let Some(words) = plain_words(strip(command, false, false)) else { return false };
    let Some((first, args)) = words.split_first() else { return false };
    if first.glob || first.text != command_name(command) {
        return false;
    }
    let names_denied =
        args.iter().any(|a| if a.glob { denied(None) } else { !a.text.starts_with('-') && denied(Some(&a.text)) });
    if names_denied {
        return false;
    }
    let glob = args.iter().any(|a| a.glob);
    let is = |list: &[&str], arg: &Word| list.iter().any(|w| arg.text == *w || arg.text.starts_with(&format!("{w}=")));
    match first.text.as_str() {
        name if READ_ONLY.contains(&name) => true,
        "which" => args.iter().all(|a| !a.text.starts_with('-') || matches!(a.text.as_str(), "-a" | "-s")),
        "find" => !glob && !args.iter().any(|a| FIND_ACTS.contains(&a.text.as_str())),
        "cd" => match args {
            [dir] => !glob && !dir.text.starts_with(['/', '-']) && !dir.text.split('/').any(|c| c == ".."),
            _ => false,
        },
        "git" => {
            let args = args.strip_prefix(&[Word { text: "--no-pager".into(), glob: false }][..]).unwrap_or(args);
            let Some((sub, rest)) = args.split_first() else { return false };
            let listing = |options: &[&str]| rest.iter().all(|a| options.contains(&a.text.as_str()));
            !moves
                && !glob
                && !rest.iter().any(|a| GIT_WRITES.iter().any(|w| a.text.starts_with(w)))
                && match sub.text.as_str() {
                    s if GIT_READS.contains(&s) => true,
                    "branch" => listing(&["-a", "--all", "-r", "--remotes", "-v", "-vv", "--verbose", "--show-current"]),
                    "remote" => listing(&["-v", "--verbose"]),
                    "tag" => rest.is_empty() || rest.iter().any(|a| is(&["-l", "--list"], a)),
                    "stash" => matches!(rest.first().map(|a| a.text.as_str()), Some("list" | "show")),
                    _ => false,
                }
        }
        _ => false,
    }
}

/// The first word of `command` (without wrappers), as written: a command
/// named with quotes or escapes is not taken for the plain one.
fn command_name(command: &str) -> &str {
    split_word(strip(command, false, false)).0
}

/// Whether `command` changes to another directory (`cd` to anything but `.`).
pub(crate) fn changes_dir(command: &str) -> bool {
    let words = plain_words(strip(command, false, false));
    match words.as_deref() {
        Some([first, args @ ..]) if first.text == "cd" => !matches!(args, [dir] if dir.text == "." || dir.text == "./"),
        Some(_) => false,
        // Cannot tell: assume it might.
        None => command_name(command) == "cd",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_words_as_the_shell_passes_them() {
        let words = |command: &str| plain_words(command).map(|w| w.into_iter().map(|w| w.text).collect::<Vec<_>>());
        assert_eq!(words("grep -n 'a b' \"c\\\"d\" e\\ f 2>/dev/null").unwrap(), ["grep", "-n", "a b", "c\"d", "e f"]);
        assert_eq!(words("ls 2>&1 >&- &>/dev/null >>/dev/null x").unwrap(), ["ls", "x"]);
        assert_eq!(words("echo '' x").unwrap(), ["echo", "", "x"]);
        assert_eq!(words("cat .en\\\nv a\u{a0}b").unwrap(), ["cat", ".env", "a\u{a0}b"]);
        for unread in ["ls >out", "ls 2> err", "ls &>x", "ls >| x", "cat <x", "ls >&x", "echo $HOME", "echo \"$(x)\"", "cat ~/x", "(ls)"] {
            assert_eq!(words(unread), None, "{unread}");
        }
        assert!(plain_words("ls *.rs a").unwrap()[1].glob);
        assert!(!plain_words("ls '*.rs'").unwrap()[1].glob);
    }
}
