//! `todo_write`: the model's checklist for a task of several steps. Each call
//! sends the whole list; the frontends show it, and a compaction carries the
//! latest one over, so nothing else keeps it.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolOutput, parse_input};

pub struct TodoTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    todos: Vec<Todo>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Todo {
    content: String,
    status: Status,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Pending,
    InProgress,
    Completed,
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo_write"
    }

    fn description(&self) -> &str {
        "Write your todo list for the current task: the whole list each time, replacing the last one. \
         The user sees it. Use it for work with three or more steps; skip it for small tasks."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "Every item, in order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "What to do, in a short imperative sentence."},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                        },
                        "required": ["content", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["todos"],
            "additionalProperties": false
        })
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        if input.todos.iter().any(|t| t.content.trim().is_empty()) {
            return ToolOutput::error("Every todo needs content");
        }
        let done = input.todos.iter().filter(|t| t.status == Status::Completed).count();
        if input.todos.is_empty() || done == input.todos.len() {
            return ToolOutput::ok("Todo list updated; everything on it is done.");
        }
        ToolOutput::ok(format!(
            "Todo list updated ({done} of {} done). Keep it current: mark an item in_progress when you \
             start it and completed as soon as it is done.",
            input.todos.len()
        ))
    }
}

/// `(status, content)` of each item in a `todo_write` input, for display;
/// empty if the input is not a todo list.
pub fn items(input: &Value) -> Vec<(&str, &str)> {
    let todos = input["todos"].as_array().map(Vec::as_slice).unwrap_or_default();
    todos.iter().filter_map(|t| Some((t["status"].as_str()?, t["content"].as_str()?))).collect()
}

/// A `todo_write` input as a Markdown checklist.
pub fn checklist(input: &Value) -> String {
    let lines: Vec<String> = items(input)
        .into_iter()
        .map(|(status, content)| match status {
            "completed" => format!("- [x] {content}"),
            "in_progress" => format!("- [ ] {content} (in progress)"),
            _ => format!("- [ ] {content}"),
        })
        .collect();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn takes_the_whole_list() {
        let input = json!({"todos": [
            {"content": "Read the code", "status": "completed"},
            {"content": "Fix the bug", "status": "in_progress"},
            {"content": "Run the tests", "status": "pending"},
        ]});
        let out = TodoTool.run(input.clone()).await;
        assert!(!out.is_error && out.content.contains("1 of 3 done"), "{}", out.content);
        assert_eq!(checklist(&input), "- [x] Read the code\n- [ ] Fix the bug (in progress)\n- [ ] Run the tests");

        let done = TodoTool.run(json!({"todos": [{"content": "x", "status": "completed"}]})).await;
        assert!(done.content.contains("everything on it is done"));
        assert!(TodoTool.run(json!({"todos": [{"content": "x", "status": "doing"}]})).await.is_error);
        assert!(TodoTool.run(json!({"todos": [{"content": " ", "status": "pending"}]})).await.is_error);
    }
}
