//! Asking the user about a tool call in the terminal: the question takes
//! the input box's place until they answer, at the prompt or while a turn
//! runs, one call at a time.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use tokio::sync::oneshot;

use super::keys::Key;
use super::Output;
use super::calls::call_label;
use super::text;
use super::style::{BOLD, CYAN, DIM, GREEN, QUESTION, RED};
use crate::security::permissions::{Answer, Asker, Request};

/// Lines of an edit shown with the question, for each side.
const EDIT_LINES: usize = 4;

/// A question waiting for the user's answer.
pub struct Question {
    title: String,
    detail: Vec<String>,
    options: Vec<String>,
    selected: usize,
    answer: Option<oneshot::Sender<usize>>,
}

impl Question {
    fn new(request: &Request, waiting: usize, answer: oneshot::Sender<usize>) -> Self {
        let width = text::terminal_width().saturating_sub(16).max(20);
        let mut title = format!("Allow {}?", call_label(&request.tool, &request.input, width));
        if let Some(agent) = &request.agent {
            title.push_str(&format!("  (from the {agent} subagent)"));
        }
        let mut detail = vec![format!("It needs asking: {}.", request.reason)];
        if request.tool == "edit" {
            let lines = |key: &str, sign: char| -> Vec<String> {
                let text = request.input[key].as_str().unwrap_or_default();
                let mut lines: Vec<String> = text.lines().take(EDIT_LINES).map(|l| format!("{sign} {l}")).collect();
                if text.lines().count() > EDIT_LINES {
                    lines.push(format!("{sign} …"));
                }
                lines
            };
            detail.extend(lines("old_string", '-'));
            detail.extend(lines("new_string", '+'));
        } else if request.tool == "write" {
            let lines = request.input["content"].as_str().map_or(0, |c| c.lines().count());
            detail.push(format!("It writes {lines} {}.", text::plural(lines, "line")));
        }
        if waiting > 0 {
            let calls = if waiting == 1 { "call waits" } else { "calls wait" };
            detail.push(format!("{waiting} more {calls} after this one."));
        }
        let options = vec![
            "Yes".to_string(),
            format!("Yes, and don't ask again this session for {}", request.suggestion),
            "No, and tell the model to do something else (Esc)".to_string(),
        ];
        Self { title, detail, options, selected: 0, answer: Some(answer) }
    }

    /// Takes `key`; true once it is answered.
    pub fn key(&mut self, key: &Key) -> bool {
        let chosen = match key {
            Key::Up => {
                self.selected = self.selected.checked_sub(1).unwrap_or(self.options.len() - 1);
                None
            }
            Key::Down | Key::Tab => {
                self.selected = (self.selected + 1) % self.options.len();
                None
            }
            Key::Enter => Some(self.selected),
            Key::Esc => Some(self.options.len() - 1),
            Key::Text(t) => t.trim().parse::<usize>().ok().filter(|n| (1..=self.options.len()).contains(n)).map(|n| n - 1),
            _ => None,
        };
        let Some(chosen) = chosen else { return false };
        if let Some(answer) = self.answer.take() {
            let _ = answer.send(chosen);
        }
        true
    }

    pub fn render(&self, width: usize, paint: &dyn Fn(&str, &str) -> String) -> Vec<String> {
        let mut rows = vec![paint(DIM, &super::input::rule(width))];
        rows.push(format!("{} {}", paint(QUESTION, "?"), paint(BOLD, &text::fit(&text::printable(&self.title), width.saturating_sub(3)))));
        for line in &self.detail {
            let sgr = if line.starts_with("- ") { RED } else if line.starts_with("+ ") { GREEN } else { DIM };
            rows.push(format!("  {}", paint(sgr, &text::fit(&text::printable(line), width.saturating_sub(3)))));
        }
        for (i, option) in self.options.iter().enumerate() {
            let label = text::fit(&format!("{}. {option}", i + 1), width.saturating_sub(5));
            rows.push(if i == self.selected { format!("{} {}", paint(CYAN, "❯"), paint(CYAN, &label)) } else { format!("  {label}") });
        }
        rows.push(paint(DIM, &super::input::rule(width)));
        rows
    }
}

/// Asks in the terminal through `out`, where keys reach it.
pub struct TerminalAsker {
    out: Output,
    /// One question at a time.
    turn: tokio::sync::Mutex<()>,
    waiting: AtomicUsize,
}

impl TerminalAsker {
    pub fn new(out: Output) -> Arc<Self> {
        Arc::new(Self { out, turn: tokio::sync::Mutex::new(()), waiting: AtomicUsize::new(0) })
    }
}

#[async_trait]
impl Asker for TerminalAsker {
    async fn ask(&self, request: &Request) -> Option<Answer> {
        if !self.out.keys_routed() {
            return None;
        }
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let turn = self.turn.lock().await;
        let waiting = self.waiting.fetch_sub(1, Ordering::SeqCst) - 1;
        // Closed when answered, or when the call that asks is dropped.
        struct Close<'a>(&'a Output);
        impl Drop for Close<'_> {
            fn drop(&mut self) {
                self.0.set_question(None);
            }
        }
        let (tx, rx) = oneshot::channel();
        self.out.set_question(Some(Question::new(request, waiting, tx)));
        let _close = Close(&self.out);
        let chosen = rx.await.ok();
        drop(turn);
        match chosen? {
            0 => Some(Answer::Allow),
            1 => Some(Answer::AllowAlways),
            _ => Some(Answer::Deny(None)),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn question() -> (Question, oneshot::Receiver<usize>) {
        let request = Request {
            tool: "edit".into(),
            input: json!({"path": "a.rs", "old_string": "one\ntwo", "new_string": "three"}),
            reason: "it changes things".into(),
            suggestion: "Edit".into(),
            agent: Some("general-purpose agent_1".into()),
        };
        let (tx, rx) = oneshot::channel();
        (Question::new(&request, 2, tx), rx)
    }

    #[test]
    fn shows_the_call_and_takes_an_answer() {
        let (mut q, mut rx) = question();
        let rows = q.render(80, &|_, s| s.to_string());
        assert!(rows[1].starts_with("? Allow edit(a.rs)?") && rows[1].contains("general-purpose agent_1"), "{rows:?}");
        assert!(rows.iter().any(|r| r == "  - two") && rows.iter().any(|r| r == "  + three"), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("2 more calls wait")), "{rows:?}");
        assert!(rows.iter().any(|r| r.starts_with("❯ 1. Yes")));
        assert!(!q.key(&Key::Down) && !q.key(&Key::Text("x".into())));
        assert!(q.key(&Key::Enter));
        assert_eq!(rx.try_recv(), Ok(1), "the second option");
        let (mut q, mut rx) = question();
        assert!(q.key(&Key::Esc));
        assert_eq!(rx.try_recv(), Ok(2));
        let (mut q, mut rx) = question();
        assert!(q.key(&Key::Text("1".into())));
        assert_eq!(rx.try_recv(), Ok(0));
    }
}
