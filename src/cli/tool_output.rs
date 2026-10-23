//! Tool output retained separately from the compact terminal transcript.

use serde_json::Value;

use super::dialog::Item;
use super::output::call_label;
use crate::llm::Message;
use crate::tools::ToolOutput;

pub(super) struct Record {
    pub id: String,
    pub name: String,
    pub input: Value,
    content: Option<String>,
    is_error: bool,
    attachments: usize,
    interrupted: bool,
}

impl Record {
    /// Human-readable result first; protocol details are opt-in.
    pub fn expanded(&self, inputs: bool) -> Vec<(String, &'static str)> {
        let color = if self.is_error { "31" } else if self.interrupted { "33" } else { "32" };
        let mut lines = vec![(format!("● {}", call_label(&self.name, &self.input, usize::MAX)), color)];
        // The compact label keeps only the first command line.
        if let Some(command) = self.input["command"].as_str() {
            for line in command.lines().skip(1) { lines.push((format!("  {line}"), "0")); }
        }
        if inputs {
            lines.push((format!("  Call ID: {}", self.id), "2"));
            lines.push(("  Input:".into(), "2"));
            for line in serde_json::to_string_pretty(&self.input).unwrap_or_default().lines() {
                lines.push((format!("    {line}"), "2"));
            }
        }
        match &self.content {
            Some(content) if !content.is_empty() => {
                for (i, line) in content.split_terminator('\n').enumerate() {
                    lines.push((format!("{}{}", if i == 0 { "  ⎿ " } else { "    " }, line),
                        if self.is_error { "31" } else { "0" }));
                }
            }
            Some(_) => lines.push(("  ⎿ (no output)".into(), "2")),
            None => lines.push((if self.interrupted { "  ⎿ Interrupted · no result received" }
                else { "  ⎿ Running · waiting for result" }.into(), "2")),
        }
        if self.attachments > 0 {
            lines.push((format!("    {} image/document attachments", self.attachments), "2"));
        }
        lines
    }

    pub fn pending(id: &str, name: &str, input: &Value) -> Self {
        Self { id: id.into(), name: name.into(), input: input.clone(), content: None,
            is_error: false, attachments: 0, interrupted: false }
    }

    pub fn complete(&mut self, output: &ToolOutput) {
        self.content = Some(output.content.clone());
        self.is_error = output.is_error;
        self.attachments = output.images.len();
    }

    pub fn interrupt(&mut self) { self.interrupted = true; }

    pub fn item(&self, number: usize) -> Item {
        let status = if self.interrupted { "Interrupted · no result received" }
            else if self.content.is_none() { "Running · waiting for result" }
            else if self.is_error { "Error" } else { "Completed" };
        let mut details = vec![format!("Call ID: {}", self.id), "Input:".into(),
            serde_json::to_string_pretty(&self.input).unwrap_or_default(), String::new(), "Output:".into()];
        details.push(self.content.clone().unwrap_or_else(|| status.into()));
        if self.attachments > 0 {
            details.push(format!("{} image/document attachments (text shown above)", self.attachments));
        }
        Item::new(format!("#{number} {}", call_label(&self.name, &self.input, usize::MAX)), status).details(details)
    }
}

pub(super) fn items(records: &[Record]) -> Vec<Item> {
    records.iter().enumerate().map(|(i, record)| record.item(i + 1)).collect()
}

/// Rebuilds saved output using call IDs; tool names need not be unique.
pub(super) fn restore(messages: &[Message]) -> Vec<Record> {
    let mut records: Vec<Record> = Vec::new();
    for block in messages.iter().flat_map(|m| &m.content) {
        match block["type"].as_str() {
            Some("tool_use") => records.push(Record::pending(block["id"].as_str().unwrap_or(""),
                block["name"].as_str().unwrap_or(""), &block["input"])),
            Some("tool_result") => {
                let id = block["tool_use_id"].as_str().unwrap_or("");
                if let Some(record) = records.iter_mut().rev().find(|r| r.id == id) {
                    let mut output = ToolOutput::ok(crate::tools::result_text(&block["content"]));
                    output.is_error = block["is_error"].as_bool().unwrap_or(false);
                    if let Value::Array(blocks) = &block["content"] {
                        output.images = blocks.iter().filter(|b| matches!(b["type"].as_str(), Some("image" | "document"))).cloned().collect();
                    }
                    record.complete(&output);
                }
            }
            _ => {}
        }
    }
    for record in &mut records {
        if record.content.is_none() { record.interrupt(); }
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn restored_same_name_calls_match_ids_and_keep_all_lines_and_blanks() {
        let content = "first\n\n".to_string() + &"long output 中文\n".repeat(30);
        let messages = vec![Message { role: "assistant", source: None, content: vec![
            json!({"type":"tool_use","id":"a","name":"bash","input":{"command":"df -h"}}),
            json!({"type":"tool_use","id":"b","name":"bash","input":{"command":"du -sh"}}),
        ]}, Message::user(vec![
            json!({"type":"tool_result","tool_use_id":"b","content":"second","is_error":true}),
            json!({"type":"tool_result","tool_use_id":"a","content":content}),
        ])];
        let records = restore(&messages);
        assert_eq!(records[0].content.as_deref(), Some(content.as_str()));
        assert_eq!(records[1].content.as_deref(), Some("second"));
        assert_eq!(records[1].item(2).description, "Error");
        assert!(records[0].item(1).details.contains(&content));
        assert!(records[0].item(1).label.contains("df -h"));
    }

    #[test]
    fn interrupted_and_multimodal_results_do_not_dump_encoded_attachments() {
        let messages = vec![Message::user(vec![
            json!({"type":"tool_use","id":"a","name":"read","input":{}}),
            json!({"type":"tool_use","id":"b","name":"bash","input":{}}),
            json!({"type":"tool_result","tool_use_id":"a","content":[
                {"type":"text","text":"image label"}, {"type":"image","source":{"data":"encoded"}}
            ]}),
        ])];
        let records = restore(&messages);
        let details = records[0].item(1).details.join("\n");
        assert!(details.contains("image label"));
        assert!(details.contains("1 image/document"));
        assert!(!details.contains("encoded"));
        assert!(records[1].item(2).description.starts_with("Interrupted"));
    }
}
