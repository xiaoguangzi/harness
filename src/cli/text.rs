//! Display-width helpers for text that may contain ANSI style sequences.

use unicode_width::UnicodeWidthChar;

/// Splits `s` into style sequences (`\x1b[...m`) and visible characters.
fn tokens(s: &str) -> impl Iterator<Item = (&str, bool)> {
    let mut rest = s;
    std::iter::from_fn(move || {
        let c = rest.chars().next()?;
        let len = if c == '\x1b' && rest[1..].starts_with('[') {
            rest[2..].find(|c: char| ('@'..='~').contains(&c)).map_or(rest.len(), |i| i + 3)
        } else {
            c.len_utf8()
        };
        let (token, tail) = rest.split_at(len);
        rest = tail;
        Some((token, token.starts_with('\x1b')))
    })
}

pub fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// Columns `s` takes on screen, ignoring style sequences.
pub fn width(s: &str) -> usize {
    tokens(s).filter(|(_, esc)| !esc).flat_map(|(t, _)| t.chars()).map(char_width).sum()
}

/// `s` without style sequences.
pub fn strip_ansi(s: &str) -> String {
    tokens(s).filter(|(_, esc)| !esc).map(|(t, _)| t).collect()
}

/// Cuts plain `s` to at most `max` columns, marking a cut with "…".
pub fn fit(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = char_width(c);
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Keeps the end of plain `s`, at most `max` columns, marking a cut with "…".
pub fn fit_tail(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    let mut kept = Vec::new();
    let mut used = 0;
    for c in s.chars().rev() {
        let w = char_width(c);
        if used + w + 1 > max {
            break;
        }
        kept.push(c);
        used += w;
    }
    std::iter::once('…').chain(kept.into_iter().rev()).collect()
}

/// `s` on one line, safe to print: style sequences removed, tabs expanded,
/// and other control characters replaced.
pub fn printable(s: &str) -> String {
    strip_ansi(s)
        .chars()
        .flat_map(|c| match c {
            '\t' => vec![' '; 4],
            c if c.is_control() => vec!['�'],
            c => vec![c],
        })
        .collect()
}

/// Wraps styled `s` into lines of at most `max` columns, breaking at spaces
/// where it can. Each line ends with a reset and the next one replays the
/// styles opened so far, so lines can be printed side by side.
pub fn wrap(s: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    // Style sequences seen so far, replayed at the start of each new line.
    let mut styles = String::new();
    // Where the last space on this line ends: byte offset in `line`, and
    // how much of `styles` was open by then.
    let mut last_space: Option<(usize, usize)> = None;

    for (token, esc) in tokens(s) {
        if esc {
            line.push_str(token);
            styles.push_str(token);
            continue;
        }
        let c = token.chars().next().unwrap_or(' ');
        let w = char_width(c);
        if used + w > max && used > 0 {
            // Move the word after the last space to the next line.
            let (carry, open) = match last_space {
                Some((at, open)) if c != ' ' => (line.split_off(at), open),
                _ => (String::new(), styles.len()),
            };
            let reset = if styles.is_empty() { "" } else { "\x1b[0m" };
            lines.push(format!("{}{reset}", std::mem::take(&mut line).trim_end()));
            line = format!("{}{carry}", &styles[..open]);
            used = width(&line);
            last_space = None;
            if c == ' ' {
                continue;
            }
        }
        line.push(c);
        used += w;
        if c == ' ' {
            last_space = Some((line.len(), styles.len()));
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// Pads styled `s` with spaces to `cols` columns.
pub fn pad(s: &str, cols: usize) -> String {
    format!("{s}{}", " ".repeat(cols.saturating_sub(width(s))))
}

/// The terminal's width in columns, or 80 when unknown.
pub fn terminal_width() -> usize {
    #[cfg(unix)]
    {
        // SAFETY: TIOCGWINSZ only writes a `winsize` into the given struct.
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_col > 0 {
            return size.ws_col as usize;
        }
    }
    std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()).unwrap_or(80)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_wide_and_styled_text() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("你好"), 4);
        assert_eq!(width("\x1b[1mhi\x1b[22m"), 2);
        assert_eq!(strip_ansi("\x1b[36mx\x1b[39m y"), "x y");
    }

    #[test]
    fn fits_to_width() {
        assert_eq!(fit("hello", 5), "hello");
        assert_eq!(fit("hello world", 6), "hello…");
        assert_eq!(fit("你好世界", 5), "你好…");
        assert_eq!(fit_tail("hello world", 6), "…world");
        assert_eq!(fit_tail("你好世界", 5), "…世界");
    }

    #[test]
    fn makes_text_printable() {
        assert_eq!(printable("\x1b[31mred\x1b[0m\tx\x07"), "red    x�");
    }

    #[test]
    fn wraps_at_spaces_and_replays_styles() {
        assert_eq!(wrap("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap("你好世界", 4), vec!["你好", "世界"]);
        assert_eq!(wrap("", 4), vec![""]);
        assert_eq!(
            wrap("\x1b[1mbold text\x1b[22m", 5),
            vec!["\x1b[1mbold\x1b[0m", "\x1b[1mtext\x1b[22m"]
        );
    }
}
