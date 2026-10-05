//! Shows a file change as a compact, numbered diff under its tool call.

use std::time::Duration;

use similar::{ChangeTag, TextDiff};

use super::text;
use crate::tools::FileChange;

/// Unchanged lines kept around each change.
const CONTEXT: usize = 3;
/// Body lines shown at most, not counting the summary.
const MAX_LINES: usize = 40;
/// Lines of a new file shown.
const NEW_FILE_LINES: usize = 10;

/// One line of the rendered body.
#[derive(Debug, PartialEq)]
enum Row {
    Context(usize, String),
    Removed(usize, String),
    Added(usize, String),
    /// Unchanged lines skipped between hunks.
    Gap,
}

/// The summary line and the diff below it, styled with `paint(sgr, s)` and
/// fit to `width` columns.
pub fn render(change: &FileChange, width: usize, paint: impl Fn(&str, &str) -> String) -> Vec<String> {
    let (summary, rows, hidden) = match &change.before {
        None => created(change),
        Some(before) if *before == change.after => return vec![format!("No changes to {}", change.path)],
        Some(before) => updated(&change.path, before, &change.after),
    };

    let number_width = rows
        .iter()
        .filter_map(|r| match r {
            Row::Context(n, _) | Row::Removed(n, _) | Row::Added(n, _) => Some(n.to_string().len()),
            Row::Gap => None,
        })
        .max()
        .unwrap_or(1);
    let room = width.saturating_sub(number_width + 3);
    let mut lines = vec![summary];
    for row in &rows {
        let (n, mark, line, sgr) = match row {
            Row::Context(n, l) => (n, ' ', l, "2"),
            Row::Removed(n, l) => (n, '-', l, "31"),
            Row::Added(n, l) => (n, '+', l, "32"),
            Row::Gap => {
                lines.push(paint("2", &format!("{:>number_width$}", "⋮")));
                continue;
            }
        };
        let line = format!("{n:>number_width$} {mark} {}", text::fit(line, room));
        lines.push(paint(sgr, &line));
    }
    if hidden > 0 {
        lines.push(paint("2", &format!("… +{hidden} more lines")));
    }
    lines
}

fn created(change: &FileChange) -> (String, Vec<Row>, usize) {
    let total = change.after.lines().count();
    let summary = format!("Created {} ({} {})", change.path, total, text::plural(total, "line"));
    let rows = change.after.lines().take(NEW_FILE_LINES).enumerate().map(|(i, l)| Row::Added(i + 1, clean(l))).collect();
    (summary, rows, total.saturating_sub(NEW_FILE_LINES))
}

fn updated(path: &str, before: &str, after: &str) -> (String, Vec<Row>, usize) {
    // Huge rewrites fall back to a coarser diff rather than stall the UI.
    let diff = TextDiff::configure().timeout(Duration::from_millis(200)).diff_lines(before, after);
    let (mut added, mut removed) = (0, 0);
    let mut rows = Vec::new();
    // Only the first `MAX_LINES` rows are shown, so later ones are just
    // counted, without cleaning their text.
    let mut hidden = 0;
    let mut push = |row: &dyn Fn() -> Row| {
        if rows.len() < MAX_LINES {
            rows.push(row());
        } else {
            hidden += 1;
        }
    };
    for (i, group) in diff.grouped_ops(CONTEXT).iter().enumerate() {
        if i > 0 {
            push(&|| Row::Gap);
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let line = || clean(change.value());
                match change.tag() {
                    ChangeTag::Equal => push(&|| Row::Context(change.new_index().unwrap_or(0) + 1, line())),
                    ChangeTag::Delete => {
                        removed += 1;
                        push(&|| Row::Removed(change.old_index().unwrap_or(0) + 1, line()));
                    }
                    ChangeTag::Insert => {
                        added += 1;
                        push(&|| Row::Added(change.new_index().unwrap_or(0) + 1, line()));
                    }
                }
            }
        }
    }
    let summary = format!(
        "Updated {path} with {added} {} and {removed} {}",
        text::plural(added, "addition"),
        text::plural(removed, "removal")
    );
    (summary, rows, hidden)
}

/// A file line made safe for the terminal.
fn clean(line: &str) -> String {
    text::printable(line.trim_end_matches(['\n', '\r']))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(before: Option<&str>, after: &str) -> FileChange {
        FileChange { path: "f.rs".into(), before: before.map(Into::into), after: after.into() }
    }

    fn plain(change: &FileChange) -> Vec<String> {
        render(change, 80, |_, s| s.to_string())
    }

    #[test]
    fn shows_changed_lines_with_context_and_numbers() {
        let before = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n";
        let after = "a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\n";
        assert_eq!(
            plain(&change(Some(before), after)),
            vec![
                "Updated f.rs with 2 additions and 1 removal",
                " 1   a",
                " 2 - b",
                " 2 + B",
                " 3   c",
                " 4   d",
                " 5   e",
                " ⋮",
                "10   j",
                "11   k",
                "12   l",
                "13 + m",
            ]
        );
    }

    #[test]
    fn previews_new_files() {
        let after: String = (1..=12).map(|i| format!("line {i}\n")).collect();
        let lines = plain(&change(None, &after));
        assert_eq!(lines[0], "Created f.rs (12 lines)");
        assert_eq!(lines[1], " 1 + line 1");
        assert_eq!(lines.len(), 1 + NEW_FILE_LINES + 1);
        assert_eq!(lines.last().unwrap(), "… +2 more lines");
    }

    #[test]
    fn notes_unchanged_files() {
        assert_eq!(plain(&change(Some("x\n"), "x\n")), vec!["No changes to f.rs"]);
    }
}
