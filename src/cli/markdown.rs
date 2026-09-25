//! Renders streamed Markdown for the terminal, a line at a time.
//!
//! Text is buffered until a line is complete, then styled: headings, bold,
//! italics, inline code, code blocks, lists, quotes, rules and links.
//! Table rows are held until the table ends, so columns can be aligned.

use super::text;

const BOLD: &str = "\x1b[1m";
const NO_BOLD: &str = "\x1b[22m";
const ITALIC: &str = "\x1b[3m";
const NO_ITALIC: &str = "\x1b[23m";
const UNDERLINE: &str = "\x1b[4m";
const NO_UNDERLINE: &str = "\x1b[24m";
const DIM: &str = "\x1b[2m";
const CODE: &str = "\x1b[36m";
const CODE_BLOCK: &str = "\x1b[33m";
const DEFAULT_COLOR: &str = "\x1b[39m";

pub struct Markdown {
    width: usize,
    /// Text after the last newline.
    partial: String,
    /// The opening fence while inside a code block.
    fence: Option<String>,
    table: Vec<String>,
    /// Blank lines held back, so leading and trailing ones are dropped and
    /// runs of them collapse.
    blank: bool,
    started: bool,
}

impl Markdown {
    pub fn new(width: usize) -> Self {
        Self { width, partial: String::new(), fence: None, table: Vec::new(), blank: false, started: false }
    }

    /// Feeds streamed text; returns the lines it completed, rendered.
    pub fn push(&mut self, text: &str) -> Vec<String> {
        self.partial.push_str(text);
        let mut out = Vec::new();
        while let Some(i) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=i).collect();
            self.line(line.trim_end_matches(['\n', '\r']), &mut out);
        }
        out
    }

    /// Ends the text block: renders what is left and resets.
    pub fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        let rest = std::mem::take(&mut self.partial);
        if !rest.is_empty() {
            self.line(&rest, &mut out);
        }
        self.flush_table(&mut out);
        *self = Self::new(self.width);
        out
    }

    fn line(&mut self, line: &str, out: &mut Vec<String>) {
        let trimmed = line.trim_start();
        if let Some(fence) = &self.fence {
            let closes = trimmed.starts_with(fence.as_str())
                && trimmed.trim_start_matches(['`', '~']).trim().is_empty();
            if closes {
                self.fence = None;
            } else if line.is_empty() {
                self.emit(String::new(), out);
            } else {
                self.emit(format!("  {CODE_BLOCK}{line}{DEFAULT_COLOR}"), out);
            }
            return;
        }
        if trimmed.starts_with('|') {
            self.table.push(trimmed.to_string());
            return;
        }
        self.flush_table(out);

        if trimmed.is_empty() {
            self.blank = self.started;
            return;
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let marker: String = trimmed.chars().take_while(|&c| c == '`' || c == '~').collect();
            let lang = trimmed[marker.len()..].trim();
            self.fence = Some(marker);
            if !lang.is_empty() {
                self.emit(format!("  {DIM}{lang}{NO_BOLD}"), out);
            }
            return;
        }
        let indent = &line[..line.len() - trimmed.len()];
        // Quotes and list items wrap with a hanging indent: (marker, body).
        let (marker, body) = if let Some((level, title)) = heading(trimmed) {
            let style = if level == 1 { format!("{BOLD}{UNDERLINE}") } else { BOLD.to_string() };
            self.emit(format!("{style}{}{NO_UNDERLINE}{NO_BOLD}", inline(title)), out);
            return;
        } else if is_rule(trimmed) {
            self.emit(format!("{DIM}{}{NO_BOLD}", "─".repeat(self.width.min(60))), out);
            return;
        } else if let Some(quote) = trimmed.strip_prefix('>') {
            (format!("{DIM}│{NO_BOLD} "), format!("{ITALIC}{}{NO_ITALIC}", inline(quote.trim_start())))
        } else if let Some(item) = ["- ", "* ", "+ "].iter().find_map(|m| trimmed.strip_prefix(m)) {
            let item = match item.strip_prefix("[ ] ").or_else(|| item.strip_prefix("[x] ")) {
                Some(rest) if item.starts_with("[x]") => format!("☑ {}", inline(rest)),
                Some(rest) => format!("☐ {}", inline(rest)),
                None => inline(item),
            };
            (format!("{indent}• "), item)
        } else if let Some((number, item)) = numbered(trimmed) {
            (format!("{indent}{number} "), inline(item))
        } else {
            self.emit(inline(line), out);
            return;
        };
        let hang = " ".repeat(text::width(&marker));
        let wrapped = text::wrap(&body, self.width.saturating_sub(hang.len() + 1).max(20));
        for (i, part) in wrapped.into_iter().enumerate() {
            self.emit(format!("{}{part}", if i == 0 { &marker } else { &hang }), out);
        }
    }

    fn emit(&mut self, line: String, out: &mut Vec<String>) {
        if std::mem::take(&mut self.blank) {
            out.push(String::new());
        }
        self.started = true;
        out.push(line);
    }

    fn flush_table(&mut self, out: &mut Vec<String>) {
        if self.table.is_empty() {
            return;
        }
        let rows = std::mem::take(&mut self.table);
        for line in render_table(&rows, self.width) {
            self.emit(line, out);
        }
    }
}

/// `## Title` -> (2, "Title").
fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|&c| c == '#').count();
    let rest = &line[level..];
    ((1..=6).contains(&level) && rest.starts_with(' ')).then(|| (level, rest.trim()))
}

/// `12. item` -> ("12.", "item").
fn numbered(line: &str) -> Option<(&str, &str)> {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    let rest = &line[digits..];
    (digits > 0 && (rest.starts_with(". ") || rest.starts_with(") "))).then(|| (&line[..=digits], rest[2..].trim_start()))
}

fn is_rule(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() >= 3 && ['-', '*', '_'].iter().any(|&m| compact.chars().all(|c| c == m))
}

/// Styles inline Markdown: `code`, **bold**, *italic*, [links](url).
pub fn inline(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    // Finds `pat` at or after `from`, returning its index.
    let find = |pat: &str, from: usize| -> Option<usize> {
        let pat: Vec<char> = pat.chars().collect();
        (from..chars.len().saturating_sub(pat.len() - 1)).find(|&j| chars[j..j + pat.len()] == pat[..])
    };
    let text = |a: usize, b: usize| chars[a..b].iter().collect::<String>();

    while i < chars.len() {
        let c = chars[i];
        let prev_word = i > 0 && chars[i - 1].is_alphanumeric();
        match c {
            '`' => {
                let ticks = chars[i..].iter().take_while(|&&c| c == '`').count();
                let marker = "`".repeat(ticks);
                if let Some(end) = find(&marker, i + ticks) {
                    out.push_str(&format!("{CODE}{}{DEFAULT_COLOR}", text(i + ticks, end).trim()));
                    i = end + ticks;
                    continue;
                }
            }
            '*' | '_' if chars.get(i + 1) == Some(&c) && !prev_word => {
                let marker: String = [c, c].iter().collect();
                if let Some(end) = find(&marker, i + 2).filter(|&e| e > i + 2) {
                    out.push_str(&format!("{BOLD}{}{NO_BOLD}", inline(&text(i + 2, end))));
                    i = end + 2;
                    continue;
                }
            }
            '*' | '_' if !prev_word && chars.get(i + 1).is_some_and(|n| !n.is_whitespace() && *n != c) => {
                let end = (i + 2..chars.len()).find(|&j| {
                    chars[j] == c
                        && !chars[j - 1].is_whitespace()
                        && !chars.get(j + 1).is_some_and(|n| n.is_alphanumeric())
                });
                if let Some(end) = end {
                    out.push_str(&format!("{ITALIC}{}{NO_ITALIC}", inline(&text(i + 1, end))));
                    i = end + 1;
                    continue;
                }
            }
            '[' => {
                if let Some(close) = find("](", i + 1)
                    && let Some(end) = find(")", close + 2)
                {
                    let label = text(i + 1, close);
                    let url = text(close + 2, end);
                    out.push_str(&format!("{UNDERLINE}{}{NO_UNDERLINE}", inline(&label)));
                    if url != label {
                        out.push_str(&format!(" {DIM}({url}){NO_BOLD}"));
                    }
                    i = end + 1;
                    continue;
                }
            }
            _ => {}
        }
        out.push(c);
        i += 1;
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Center,
    Right,
}

/// Lays out table rows in aligned columns, wrapping cells to fit `width`.
fn render_table(rows: &[String], width: usize) -> Vec<String> {
    let parse = |row: &str| -> Vec<String> {
        let row = row.trim().trim_start_matches('|');
        let row = row.strip_suffix('|').unwrap_or(row);
        row.split('|').map(|c| c.trim().to_string()).collect()
    };
    let mut cells: Vec<Vec<String>> = rows.iter().map(|r| parse(r)).collect();
    let is_separator = |row: &Vec<String>| {
        row.iter().all(|c| !c.is_empty() && c.trim_matches(':').chars().all(|ch| ch == '-'))
    };
    let separator = cells.iter().position(is_separator);
    let aligns: Vec<Align> = separator
        .map(|i| {
            cells[i]
                .iter()
                .map(|c| match (c.starts_with(':'), c.ends_with(':')) {
                    (true, true) => Align::Center,
                    (false, true) => Align::Right,
                    _ => Align::Left,
                })
                .collect()
        })
        .unwrap_or_default();
    if let Some(i) = separator {
        cells.remove(i);
    }
    let header = separator.is_some_and(|i| i == 1);

    let columns = cells.iter().map(Vec::len).max().unwrap_or(0);
    for row in &mut cells {
        row.resize(columns, String::new());
    }
    let styled: Vec<Vec<String>> = cells.iter().map(|row| row.iter().map(|c| inline(c)).collect()).collect();
    let mut widths: Vec<usize> =
        (0..columns).map(|c| styled.iter().map(|row| text::width(&row[c])).max().unwrap_or(0).max(1)).collect();

    // Shrink the widest columns until the table fits: " a │ b │ c".
    let chrome = 1 + 3 * columns.saturating_sub(1);
    let available = width.saturating_sub(chrome + 1);
    while widths.iter().sum::<usize>() > available {
        let (widest, &w) = widths.iter().enumerate().max_by_key(|&(_, w)| *w).unwrap();
        if w <= 6 {
            break;
        }
        widths[widest] = w - 1;
    }

    let mut out = Vec::new();
    for (r, row) in styled.iter().enumerate() {
        let wrapped: Vec<Vec<String>> = row.iter().zip(&widths).map(|(cell, &w)| text::wrap(cell, w)).collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for l in 0..height {
            let parts: Vec<String> = wrapped
                .iter()
                .enumerate()
                .map(|(c, lines)| {
                    let cell = lines.get(l).map_or("", String::as_str);
                    let cell = if header && r == 0 { format!("{BOLD}{cell}{NO_BOLD}") } else { cell.to_string() };
                    align(&cell, widths[c], aligns.get(c).copied().unwrap_or(Align::Left))
                })
                .collect();
            out.push(format!(" {}", parts.join(&format!(" {DIM}│{NO_BOLD} "))).trim_end().to_string());
        }
        if header && r == 0 {
            let rules: Vec<String> = widths.iter().map(|&w| "─".repeat(w + 2)).collect();
            out.push(format!("{DIM}{}{NO_BOLD}", rules.join("┼")));
        }
    }
    out
}

fn align(cell: &str, width: usize, align: Align) -> String {
    let gap = width.saturating_sub(text::width(cell));
    match align {
        Align::Left => text::pad(cell, width),
        Align::Right => format!("{}{cell}", " ".repeat(gap)),
        Align::Center => format!("{}{cell}{}", " ".repeat(gap / 2), " ".repeat(gap - gap / 2)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(md: &str, width: usize) -> Vec<String> {
        let mut m = Markdown::new(width);
        // Stream it in small pieces, as a model would.
        let mut out = Vec::new();
        let chars: Vec<char> = md.chars().collect();
        for chunk in chars.chunks(3) {
            out.extend(m.push(&chunk.iter().collect::<String>()));
        }
        out.extend(m.finish());
        out.iter().map(|l| text::strip_ansi(l)).collect()
    }

    #[test]
    fn styles_inline_markup() {
        assert_eq!(inline("a `b` c"), format!("a {CODE}b{DEFAULT_COLOR} c"));
        assert_eq!(inline("**x**"), format!("{BOLD}x{NO_BOLD}"));
        assert_eq!(inline("*x* y"), format!("{ITALIC}x{NO_ITALIC} y"));
        assert_eq!(inline("[doc](http://a)"), format!("{UNDERLINE}doc{NO_UNDERLINE} {DIM}(http://a){NO_BOLD}"));
        // Not emphasis: snake_case, a lone star, arithmetic.
        assert_eq!(inline("snake_case_name"), "snake_case_name");
        assert_eq!(inline("a * b * c"), "a * b * c");
        assert_eq!(inline("2*3*4"), "2*3*4");
        assert_eq!(inline("unclosed `code"), "unclosed `code");
    }

    #[test]
    fn renders_blocks() {
        let md = "\n\n## Title\n\nSome **bold** text.\n\n\n- one\n  - two\n1. first\n> quote\n---\n```rust\nfn main() {}\n\n```\ndone\n\n";
        assert_eq!(
            render(md, 80),
            vec!["Title", "", "Some bold text.", "", "• one", "  • two", "1. first", "│ quote", &"─".repeat(60),
                 "  rust", "  fn main() {}", "", "done"]
        );
    }

    #[test]
    fn list_items_wrap_with_a_hanging_indent() {
        let md = "- one two three four five six seven\n  12. alpha beta gamma delta epsilon\n";
        assert_eq!(
            render(md, 22),
            vec!["• one two three four", "  five six seven", "  12. alpha beta gamma", "      delta epsilon"]
        );
    }

    #[test]
    fn aligns_tables() {
        let md = "| Name | Kind |\n|---|--:|\n| `Tool` | trait |\n| 观察者 | x |\nafter";
        assert_eq!(
            render(md, 80),
            vec![" Name   │  Kind", "────────┼───────", " Tool   │ trait", " 观察者 │     x", "after"]
        );
    }

    #[test]
    fn wraps_wide_tables() {
        let md = "| a | b |\n|---|---|\n| short | a long cell that must wrap |\n";
        let lines = render(md, 24);
        assert!(lines.iter().all(|l| text::width(l) <= 24), "{lines:?}");
        assert_eq!(lines[2], " short │ a long cell");
        assert_eq!(lines[3], "       │ that must wrap");
    }

    #[test]
    fn a_partial_line_waits_for_its_newline() {
        let mut m = Markdown::new(80);
        assert!(m.push("hel").is_empty());
        assert_eq!(m.push("lo\nwor"), vec!["hello"]);
        assert_eq!(m.finish(), vec!["wor"]);
    }
}
