//! How tool calls and their results show in the conversation: one line
//! per call, the start of its result under it.

use serde_json::Value;

use super::{Output, text};

/// Lines of a tool result shown under its call.
const RESULT_LINES: usize = 3;

/// A `todo_write` call as a checklist.
fn todo_lines(out: &Output, input: &Value) -> Vec<String> {
    let items = crate::tools::todo::items(input);
    let done = items.iter().filter(|(status, _)| *status == "completed").count();
    let mut lines = vec![format!("{} {}", out.paint("32", "●"), out.paint("1", &format!("Todos ({done}/{})", items.len())))];
    let width = text::terminal_width().saturating_sub(8);
    for (i, (status, content)) in items.iter().enumerate() {
        let lead = out.paint("2", if i == 0 { "  ⎿ " } else { "    " });
        let content = text::fit(&text::printable(content), width);
        let item = match *status {
            "completed" => out.paint("2", &format!("✔ {content}")),
            "in_progress" => out.paint("1", &format!("◼ {content}")),
            _ => format!("☐ {content}"),
        };
        lines.push(format!("{lead}{item}"));
    }
    lines
}

/// A one-line view of a tool call: `bash(cargo test)`, `grep(foo · src)`.
pub(super) fn call_label(name: &str, input: &Value, max: usize) -> String {
    // The fields that say the most about a call, most telling first.
    const KEYS: [&str; 12] = [
        "agent_type", "description", "agent_id", "message", "command", "pattern", "path", "file_path", "glob", "url",
        "query", "name",
    ];
    let obj = input.as_object();
    let mut parts: Vec<String> = KEYS
        .iter()
        .filter_map(|k| obj?.get(*k)?.as_str())
        .filter(|v| !v.is_empty())
        .take(2)
        .map(|v| v.lines().next().unwrap_or("").to_string())
        .collect();
    if parts.is_empty() && obj.is_some_and(|o| !o.is_empty()) {
        parts.push(input.to_string());
    }
    let label = if parts.is_empty() { name.to_string() } else { format!("{name}({})", parts.join(" · ")) };
    let fitted = text::fit(&label, max);
    if fitted != label && label.ends_with(')') {
        // Keep the call visibly closed.
        format!("{})", text::fit(&label, max.saturating_sub(1)).trim_end_matches(')'))
    } else {
        fitted
    }
}

/// How a tool call is shown: `● label`, or the todo list itself.
pub(super) fn call_lines(out: &Output, name: &str, input: &Value, width: usize) -> Vec<String> {
    if name == "todo_write" {
        return todo_lines(out, input);
    }
    vec![format!("{} {}", out.paint("32", "●"), call_label(name, input, width.saturating_sub(4)))]
}

/// A tool result under its call, `⎿ ` then indented, red when it failed.
/// A todo list was already shown with its call, so its result is not.
pub(super) fn result_block(out: &Output, name: &str, content: &str, is_error: bool, width: usize) -> Vec<String> {
    if name == "todo_write" && !is_error {
        return Vec::new();
    }
    let color = if is_error { "31" } else { "2" };
    result_lines(name, content, is_error, width.saturating_sub(6))
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let lead = if i == 0 { "  ⎿ " } else { "    " };
            out.paint(color, &text::fit(&format!("{lead}{line}"), width.saturating_sub(2)))
        })
        .collect()
}

/// The first few lines of a tool result, or a summary of it.
fn result_lines(name: &str, content: &str, is_error: bool, width: usize) -> Vec<String> {
    let clean: Vec<String> = content.lines().map(text::printable).collect();
    let lines: Vec<&str> = clean.iter().map(|l| l.trim_end()).filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return vec!["(no output)".to_string()];
    }
    // Numbered lines; an image or PDF read shows its label instead.
    if name == "read" && !is_error && lines[0].split_whitespace().next().is_some_and(|n| n.parse::<u64>().is_ok()) {
        return vec![format!("Read {} lines", lines.len())];
    }
    let mut shown: Vec<String> = lines.iter().take(RESULT_LINES).map(|l| text::fit(l, width)).collect();
    if lines.len() > RESULT_LINES {
        shown.push(format!("… +{} lines", lines.len() - RESULT_LINES));
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn labels_calls_by_their_main_arguments() {
        assert_eq!(call_label("bash", &json!({"command": "cargo test\n--all", "timeout_ms": 5}), 80), "bash(cargo test)");
        assert_eq!(call_label("grep", &json!({"pattern": "fn main", "path": "src"}), 80), "grep(fn main · src)");
        assert_eq!(
            call_label("agent", &json!({"agent_type": "explore", "description": "find traits", "prompt": "..."}), 80),
            "agent(explore · find traits)"
        );
        assert_eq!(call_label("mcp__x__y", &json!({"id": 3}), 80), "mcp__x__y({\"id\":3})");
        assert_eq!(call_label("ping", &json!({}), 80), "ping");
        assert_eq!(call_label("bash", &json!({"command": "echo hello world"}), 12), "bash(echo …)");
    }

    #[test]
    fn summarizes_results() {
        assert_eq!(result_lines("read", "     1\ta\n     2\tb\n", false, 80), vec!["Read 2 lines"]);
        assert_eq!(result_lines("bash", "", false, 80), vec!["(no output)"]);
        assert_eq!(
            result_lines("bash", "1\n2\n\n3\n4\n5\n", false, 80),
            vec!["1", "2", "3", "… +2 lines"]
        );
        assert_eq!(result_lines("read", "No such file", true, 80), vec!["No such file"]);
        assert_eq!(result_lines("read", "shot.png (image, 12 KB)", false, 80), vec!["shot.png (image, 12 KB)"]);
    }
}
