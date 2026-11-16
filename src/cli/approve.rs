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

/// Rows of a command (or a script) shown with the question; Ctrl+O shows
/// the rest.
const COMMAND_ROWS: usize = 12;

/// A question waiting for the user's answer.
pub struct Question {
    title: String,
    detail: Vec<String>,
    options: Vec<String>,
    /// Whether the second option allows calls like it for the session.
    always: bool,
    selected: usize,
    answer: Option<oneshot::Sender<Answer>>,
}

impl Question {
    fn new(request: &Request, waiting: usize, answer: oneshot::Sender<Answer>) -> Self {
        let width = text::terminal_width().saturating_sub(16).max(20);
        let mut title = format!("Allow {}?", call_label(&request.tool, &request.input, width));
        if let Some(agent) = &request.agent {
            title.push_str(&format!("  (from the {agent} subagent)"));
        }
        let mut detail = vec![format!("It needs asking: {}.", request.reason)];
        let script = match request.tool.as_str() {
            "bash" => request.input["command"].as_str(),
            "code" => request.input["code"].as_str(),
            _ => None,
        };
        // The whole command, unless the title already shows all of it.
        if let Some(script) = script
            && (script.trim().lines().count() > 1 || call_label(&request.tool, &request.input, width) != call_label(&request.tool, &request.input, usize::MAX))
        {
            detail.extend(script_rows(script, text::terminal_width().saturating_sub(7).max(20)));
        }
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
        // Offered only when its rules cover the whole call.
        let always = !request.suggestions.is_empty();
        let mut options = vec!["Yes".to_string()];
        if always {
            options.push(format!("Yes, and don't ask again this session for {}", request.suggestions.join(", ")));
        }
        options.push("No, and tell the model to do something else (Esc)".to_string());
        Self { title, detail, options, always, selected: 0, answer: Some(answer) }
    }

    /// What option `chosen` answers.
    fn answer_for(&self, chosen: usize) -> Answer {
        match chosen {
            0 => Answer::Allow,
            1 if self.always => Answer::AllowAlways,
            _ => Answer::Deny(None),
        }
    }

    /// Takes `key`; true once it is answered.
    pub fn key(&mut self, key: &Key) -> bool {
        let chosen: Option<usize> = match key {
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
            let _ = answer.send(self.answer_for(chosen));
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

/// `script` wrapped into rows indented under the question, at most
/// [`COMMAND_ROWS`] of them, saying how many more there are.
fn script_rows(script: &str, width: usize) -> Vec<String> {
    let mut rows: Vec<String> = script.trim().lines().flat_map(|l| text::wrap(&text::printable(l), width)).map(|r| format!("  {r}")).collect();
    if rows.len() > COMMAND_ROWS {
        let more = rows.len() - (COMMAND_ROWS - 1);
        rows.truncate(COMMAND_ROWS - 1);
        rows.push(format!("  … {more} more {} (ctrl+o, then i, shows them)", text::plural(more, "row")));
    }
    rows
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
    async fn question(&self, request: &crate::tools::question::Request, agent: Option<&str>) -> Option<crate::tools::question::Answer> {
        if !self.out.keys_routed() { return None; }
        let _turn = self.turn.lock().await;
        if !self.out.keys_routed() { return None; }
        struct Close<'a>(&'a Output);
        impl Drop for Close<'_> {
            fn drop(&mut self) { self.0.set_user_question(None); }
        }
        let (tx, rx) = oneshot::channel();
        self.out.set_user_question(Some(super::question::Question::new(request.clone(), agent.map(str::to_owned), tx)));
        let _close = Close(&self.out);
        rx.await.ok()
    }

    async fn ask(&self, request: &Request) -> Option<Answer> {
        if !self.out.keys_routed() {
            return None;
        }
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let _turn = self.turn.lock().await;
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
        rx.await.ok()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn question() -> (Question, oneshot::Receiver<Answer>) {
        let request = Request {
            tool: "edit".into(),
            input: json!({"path": "a.rs", "old_string": "one\ntwo", "new_string": "three"}),
            reason: "it changes things".into(),
            suggestions: vec!["Edit".into()],
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
        assert_eq!(rx.try_recv(), Ok(Answer::AllowAlways), "the second option");
        let (mut q, mut rx) = question();
        assert!(q.key(&Key::Esc));
        assert_eq!(rx.try_recv(), Ok(Answer::Deny(None)));
        let (mut q, mut rx) = question();
        assert!(q.key(&Key::Text("1".into())));
        assert_eq!(rx.try_recv(), Ok(Answer::Allow));
    }

    #[test]
    fn shows_the_whole_command() {
        let ask = |command: &str| {
            let request = Request {
                tool: "bash".into(),
                input: json!({"command": command}),
                reason: "it changes things".into(),
                suggestions: Vec::new(),
                agent: None,
            };
            Question::new(&request, 0, oneshot::channel().0).render(200, &|_, s| s.to_string())
        };
        let rows = ask("make");
        assert!(!rows.iter().any(|r| r.starts_with("    ")), "the title has it all: {rows:?}");
        let rows = ask("echo safe\nrm -rf ~\x1b[2K");
        assert!(rows.iter().any(|r| r == "    echo safe") && rows.iter().any(|r| r == "    rm -rf ~"), "escapes dropped: {rows:?}");
        let long: Vec<String> = (0..30).map(|i| format!("step {i}")).collect();
        let rows = ask(&long.join("\n"));
        assert!(rows.iter().any(|r| r.contains("… 19 more rows")) && !rows.iter().any(|r| r.contains("step 11")), "{rows:?}");
    }

    #[test]
    fn offers_no_rule_that_would_not_cover_the_call() {
        let request = Request {
            tool: "bash".into(),
            input: json!({"command": "make $(cat target)"}),
            reason: "it changes things".into(),
            suggestions: Vec::new(),
            agent: None,
        };
        let (tx, mut rx) = oneshot::channel();
        let mut q = Question::new(&request, 0, tx);
        let rows = q.render(80, &|_, s| s.to_string());
        assert!(!rows.iter().any(|r| r.contains("don't ask again")), "{rows:?}");
        assert!(!q.key(&Key::Down) && q.key(&Key::Enter));
        assert_eq!(rx.try_recv(), Ok(Answer::Deny(None)), "the second option is No");
    }
}
