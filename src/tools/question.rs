//! Model-initiated choices, answered by the conversation's frontend.

use std::sync::Arc;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use crate::security::permissions::Asker;
use super::{Tool, ToolOutput, parse_input};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OptionItem {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub question: String,
    #[serde(default)]
    pub options: Vec<OptionItem>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, tag = "status", rename_all = "snake_case")]
pub enum Answer {
    Selected { index: usize },
    Custom { text: String },
    Cancelled {},
}

impl Request {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.question.trim().is_empty() || self.question.len() > 4000 {
            return Err("question must contain 1 to 4000 bytes of nonblank text");
        }
        if self.options.len() == 1 || self.options.len() > 6 {
            return Err("options must be empty (free text), or contain 2 to 6 choices");
        }
        let mut labels = std::collections::HashSet::new();
        for option in &self.options {
            if option.label.trim().is_empty() || option.label.len() > 200 || option.description.len() > 2000 {
                return Err("option labels must be nonblank and at most 200 bytes; descriptions at most 2000 bytes");
            }
            if !labels.insert(option.label.trim().to_lowercase()) { return Err("option labels must be unique"); }
        }
        Ok(())
    }

    pub fn validate_answer(&self, answer: &Answer) -> Result<(), &'static str> {
        match answer {
            Answer::Selected { index } if *index >= self.options.len() => Err("choice index is out of range"),
            Answer::Custom { text } if text.trim().is_empty() || text.len() > 16000 => Err("answer must contain 1 to 16000 bytes of nonblank text"),
            _ => Ok(()),
        }
    }
}

tokio::task_local! {
    static FRONTEND: (Option<Arc<dyn Asker>>, Option<String>);
}

pub async fn scope<F: std::future::Future>(asker: Option<Arc<dyn Asker>>, agent: Option<String>, future: F) -> F::Output {
    FRONTEND.scope((asker, agent), future).await
}

pub struct AskUserQuestionTool;

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str { "ask_user_question" }
    fn description(&self) -> &str {
        "Ask the user an important choice or missing requirement and wait for their answer in an interactive panel. \
         Give 2 to 6 distinct options with tradeoffs, recommended option first, or omit options for free text. \
         The UI always offers a custom answer and cancellation; do not add those as options. \
         Use for consequential ambiguity, not routine implementation choices or tool permission approval. \
         Do not perform work dependent on the answer in the same tool batch. \
         Cancellation or unavailable input is not consent: do not guess or immediately ask again."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {
            "question": {"type": "string", "minLength": 1, "maxLength": 4000},
            "options": {"type": "array", "maxItems": 6, "items": {
                "type": "object", "properties": {
                    "label": {"type": "string", "minLength": 1, "maxLength": 200},
                    "description": {"type": "string", "maxLength": 2000}
                }, "required": ["label", "description"], "additionalProperties": false
            }}
        }, "required": ["question"], "additionalProperties": false})
    }
    fn read_only(&self) -> bool { true }

    async fn run(&self, input: Value) -> ToolOutput {
        let request: Request = match parse_input(input) { Ok(r) => r, Err(e) => return e };
        if let Err(e) = request.validate() { return ToolOutput::error(e); }
        let (asker, agent) = FRONTEND.try_with(Clone::clone).unwrap_or_default();
        let answer = match asker {
            Some(asker) => asker.question(&request, agent.as_deref()).await,
            None => None,
        };
        let Some(answer) = answer else {
            return ToolOutput::ok(json!({"status": "unavailable", "message":
                "Interactive input is unavailable here. Present the question and options in your reply and wait; do not choose for the user."}).to_string());
        };
        if let Err(e) = request.validate_answer(&answer) { return ToolOutput::error(e); }
        let result = match answer {
            Answer::Selected { index } => json!({"status": "answered", "index": index, "answer": request.options[index].label}),
            Answer::Custom { text } => json!({"status": "answered", "answer": text}),
            Answer::Cancelled {} => json!({"status": "cancelled", "message": "The user dismissed the question without choosing. Do not assume an answer or immediately ask again."}),
        };
        ToolOutput::ok(result.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_invalid_input_before_contacting_the_frontend() {
        for input in [
            json!({"question": " "}), json!({"question": "why?", "extra": true}),
            json!({"question": "why?", "options": [{"label": "one", "description": ""}]}),
            json!({"question": "why?", "options": [{"label": "one", "description": ""}, {"label": " One ", "description": ""}]}),
            json!({"question": "why?", "options": [{"label": "one", "description": "", "unknown": true}]}),
        ] {
            assert!(AskUserQuestionTool.run(input.clone()).await.is_error, "{input}");
        }
        let out = AskUserQuestionTool.run(json!({"question": "why?"})).await;
        assert!(!out.is_error);
        assert_eq!(serde_json::from_str::<Value>(&out.content).unwrap()["status"], "unavailable");
    }

    #[test]
    fn validates_answers_without_turning_cancellation_into_a_choice() {
        let request: Request = serde_json::from_value(json!({"question": "why?", "options": [
            {"label": "a", "description": ""}, {"label": "b", "description": ""}
        ]})).unwrap();
        assert!(request.validate_answer(&Answer::Selected { index: 2 }).is_err());
        assert!(request.validate_answer(&Answer::Custom { text: "  ".into() }).is_err());
        assert!(request.validate_answer(&Answer::Custom { text: "自己的方案".into() }).is_ok());
        assert!(request.validate_answer(&Answer::Cancelled {}).is_ok());
        assert!(serde_json::from_value::<Answer>(json!({"status": "selected", "index": 0, "text": "no"})).is_err());
        assert!(serde_json::from_value::<Answer>(json!({"status": "cancelled", "index": 0})).is_err());
    }
}
