//! `/model`: preview main/subagent models and reasoning effort together.

use std::cell::Cell;

use super::Output;
use super::input::Draft;
use super::keys::Key;
use super::text;
use crate::llm::{Client, Effort};

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Main,
    Subagents,
}

pub(super) struct Picker {
    original: Client,
    pending: Client,
    models: Vec<String>,
    target: Target,
    query: Draft,
    selected: usize,
    /// The first model shown.
    top: Cell<usize>,
    error: Option<String>,
}

pub async fn pick(out: &Output, client: &Client) -> Option<Client> {
    super::dialog::run(out, Picker::new(client), Picker::handle_key).await
}

impl Picker {
    fn new(client: &Client) -> Self {
        let mut models = client.providers().listed_models();
        for spec in [Some(client.spec()), client.subagent_model()].into_iter().flatten() {
            if !models.iter().any(|m| m == spec) { models.push(spec.into()); }
        }
        let mut picker = Self {
            original: client.clone(), pending: client.clone(), models,
            target: Target::Main, query: Draft::default(), selected: 0, top: Cell::new(0), error: None,
        };
        picker.focus_current();
        picker
    }

    fn choices(&self) -> Vec<Option<String>> {
        let query = self.query.text().trim().to_lowercase();
        let mut choices = Vec::new();
        if self.target == Target::Subagents && "follow main model inherit".contains(&query) {
            choices.push(None);
        }
        choices.extend(self.models.iter().filter(|m| m.to_lowercase().contains(&query)).cloned().map(Some));
        choices
    }

    fn focus_current(&mut self) {
        let current = match self.target {
            Target::Main => Some(self.pending.spec()),
            Target::Subagents => self.pending.subagent_model(),
        };
        self.selected = self.choices().iter().position(|m| m.as_deref() == current).unwrap_or(0);
    }

    fn preview_choice(&mut self) -> bool {
        let choices = self.choices();
        let Some(choice) = choices.get(self.selected) else { return false };
        let result = match self.target {
            Target::Main => self.pending.switch(choice.as_deref().unwrap()).map(|client| self.pending = client),
            Target::Subagents => self.pending.set_subagent_model(choice.as_deref()),
        };
        self.error = result.err().map(|e| format!("{e:#}"));
        self.error.is_none()
    }

    fn effort_index(&self) -> usize {
        self.pending.effort_override().and_then(|effort| Effort::ALL.iter().position(|e| *e == effort)).map_or(0, |i| i + 1)
    }

    /// Changes are staged until Enter; cancellation never touches the live
    /// client. Only ↑/↓ and Enter take the highlighted model: typing to
    /// filter or changing the effort leaves the staged models alone.
    fn handle_key(&mut self, key: Key) -> Option<Option<Client>> {
        match key {
            Key::Esc | Key::Eof => return Some(None),
            Key::Enter => {
                if self.preview_choice() { return Some(Some(self.pending.clone())); }
            }
            Key::Tab => {
                self.target = if self.target == Target::Main { Target::Subagents } else { Target::Main };
                self.query = Draft::default();
                self.error = None;
                self.focus_current();
            }
            Key::Up | Key::Down => {
                let count = self.choices().len();
                if count > 0 {
                    let step = if key == Key::Up { -1 } else { 1 };
                    self.selected = (self.selected as isize + step).rem_euclid(count as isize) as usize;
                    self.preview_choice();
                }
            }
            Key::Left | Key::Right => {
                let index = self.effort_index();
                let next = if key == Key::Left { index.saturating_sub(1) } else { (index + 1).min(Effort::ALL.len()) };
                self.pending.set_effort(next.checked_sub(1).map(|i| Effort::ALL[i]));
            }
            Key::Newline | Key::PasteImage | Key::Position(_) => {}
            key => {
                self.query.edit(key);
                self.focus_current();
            }
        }
        None
    }

    pub(super) fn render(
        &self, width: usize, height: usize, paint: impl Fn(&str, &str) -> String,
    ) -> (Vec<String>, Option<(usize, usize)>) {
        let width = width.saturating_sub(1).max(3);
        let room = width.saturating_sub(2);
        let row = |content: String| format!("  {}", text::clip(&content, room));
        let mut rows = vec![row(paint("1;35", "Model settings"))];
        let main = if self.target == Target::Main { paint("1;35", "[ Main model ]") } else { paint("2", "  Main model  ") };
        let sub = if self.target == Target::Subagents { paint("1;35", "[ Subagents ]") } else { paint("2", "  Subagents  ") };
        rows.push(row(format!("{main}  {sub}  {}", paint("2", "Tab to switch"))));
        rows.push(row(format!("Main: {}", text::fit_tail(&text::printable(self.pending.spec()), room.saturating_sub(6)))));
        let sub = self.pending.subagent_model().map_or_else(
            || format!("follow main ({})", self.pending.spec()), String::from,
        );
        rows.push(row(format!("Subagents: {}", text::fit_tail(&text::printable(&sub), room.saturating_sub(11)))));
        rows.push(String::new());
        let query = if self.query.is_empty() { paint("2", "type to filter models") } else { text::printable(self.query.text()) };
        let cursor = (rows.len(), (10 + text::width(&self.query.text()[..self.query.position()])).min(width - 1));
        rows.push(row(format!("Search: {query}")));
        let choices = self.choices();
        let count = height.saturating_sub(15).clamp(1, 6);
        let list_at = rows.len();
        let start = super::dialog::window(&self.top, self.selected, count, choices.len());
        let original = match self.target { Target::Main => Some(self.original.spec()), Target::Subagents => self.original.subagent_model() };
        for (i, choice) in choices.iter().enumerate().skip(start).take(count) {
            let current = choice.as_deref() == original;
            let mark = if i == self.selected { "❯" } else { " " };
            let name = choice.as_deref().unwrap_or("Follow main model");
            let suffix = if current { " ● current" } else { "" };
            let name = text::fit_tail(&text::printable(name), room.saturating_sub(2 + text::width(suffix)));
            rows.push(row(paint(if i == self.selected { "1;35" } else { "0" }, &format!("{mark} {name}{suffix}"))));
        }
        if choices.is_empty() { rows.push(row(paint("2", "No matching models"))); }
        while rows.len() < list_at + count { rows.push(String::new()); }
        let range = if choices.len() > count { format!("{}–{} of {} models", start + 1, (start + count).min(choices.len()), choices.len()) } else { String::new() };
        rows.push(row(paint("2", &range)));
        let preview = match self.target {
            Target::Subagents => self.pending.subagent_model().and_then(|spec| self.pending.switch(spec).ok()).unwrap_or_else(|| self.pending.clone()),
            Target::Main => self.pending.clone(),
        };
        let protocol = preview.spec().split_once('/').and_then(|(p, _)| preview.providers().protocol_of(p)).map_or("?", |p| p.as_str());
        rows.push(row(paint("2", &format!("{protocol} · configured context {} · max output {}", amount(preview.context_window()), amount(preview.max_tokens())))));
        rows.push(String::new());
        let mut effort = std::iter::once("auto").chain(Effort::ALL.iter().map(|e| e.as_str())).enumerate().map(|(i, name)| {
            if i == self.effort_index() { paint("1;35", &format!("[{name}]")) } else { paint("2", name) }
        }).collect::<Vec<_>>().join("  ");
        if text::width(&effort) + 8 > room {
            let selected = self.pending.effort_override().map_or("auto", |e| e.as_str());
            effort = format!("← {} →", paint("1;35", &format!("[{selected}]")));
        }
        rows.push(row(format!("Effort: {effort}")));
        let help = match self.pending.effort_override() {
            Some(effort) => format!("{} · {}", effort.as_str(), super::effort_help(effort)),
            None => format!("auto → {} · uses each model's configured effort", preview.effort().as_str()),
        };
        rows.push(row(paint("2", &help)));
        let note = match &self.error {
            Some(error) => paint("31", &text::printable(error)),
            None if self.target == Target::Subagents => paint("2", "New agents only; agent-specific models take priority"),
            None => paint("2", "Preview only · Enter applies to this session"),
        };
        rows.push(row(note));
        let controls = if room >= 55 {
            ["↑/↓ model · ←/→ effort · Tab main/subagents", "Enter apply · Esc cancel"]
        } else {
            ["↑/↓ model · ←/→ effort", "Tab target · Enter apply · Esc cancel"]
        };
        for control in controls { rows.push(row(paint("2", control))); }
        (rows, Some(cursor))
    }
}

impl super::dialog::Panel for Picker {
    fn render(&self, width: usize, height: usize, paint: &dyn Fn(&str, &str) -> String) -> super::dialog::Frame {
        Picker::render(self, width, height, paint)
    }
}

fn amount(n: u64) -> String {
    if n.is_multiple_of(1000) { format!("{}k", n / 1000) } else { n.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Providers;
    use crate::llm::config::tests::{file, no_env};
    use serde_json::json;

    fn client() -> Client {
        Client::new(Providers::from_parts(&file(json!({
            "model": "gw/beta", "subagent_model": "gw/alpha", "effort": "high",
            "providers": {"gw": {"protocol": "anthropic", "base_url": "http://127.0.0.1:9",
                "models": {"alpha": {"effort": "low"}, "beta": {}, "gamma": {}}}}
        })), &no_env).unwrap()).unwrap()
    }

    #[test]
    fn previews_main_subagent_and_effort_then_applies_together() {
        let live = client();
        let mut picker = Picker::new(&live);
        picker.handle_key(Key::Down);
        assert_eq!(picker.pending.spec(), "gw/gamma");
        picker.handle_key(Key::Tab);
        picker.handle_key(Key::Down);
        assert_eq!(picker.pending.subagent_model(), Some("gw/beta"));
        picker.handle_key(Key::Right);
        assert_eq!(picker.pending.effort_override(), Some(Effort::Low));
        let selected = picker.handle_key(Key::Enter).unwrap().unwrap();
        assert_eq!((selected.spec(), selected.subagent_model(), selected.effort()), ("gw/gamma", Some("gw/beta"), Effort::Low));
        assert_eq!((live.spec(), live.subagent_model(), live.effort()), ("gw/beta", Some("gw/alpha"), Effort::High));
    }

    #[test]
    fn follow_main_clears_configured_subagent_model_and_cancel_discards_preview() {
        let live = client();
        let mut picker = Picker::new(&live);
        picker.handle_key(Key::Tab);
        picker.handle_key(Key::Up);
        assert_eq!(picker.pending.subagent_model(), None);
        assert!(picker.handle_key(Key::Esc).unwrap().is_none());
        assert_eq!(live.subagent_model(), Some("gw/alpha"));
        let selected = picker.handle_key(Key::Enter).unwrap().unwrap();
        assert_eq!(selected.subagent_model(), None);
    }

    #[test]
    fn filters_models_and_keeps_empty_results_open() {
        let mut picker = Picker::new(&client());
        picker.handle_key(Key::Text("GAM".into()));
        assert_eq!(picker.pending.spec(), "gw/beta", "filtering only moves the highlight");
        picker.handle_key(Key::Tab);
        picker.handle_key(Key::Tab);
        assert_eq!(picker.pending.spec(), "gw/beta", "switching targets doesn't take the highlighted model");
        picker.handle_key(Key::Text("GAM".into()));
        assert_eq!(picker.handle_key(Key::Enter).unwrap().unwrap().spec(), "gw/gamma", "Enter takes it");
        picker.handle_key(Key::Text("unknown".into()));
        assert!(picker.handle_key(Key::Enter).is_none());
        let rows = picker.render(80, 24, |_, s| s.into()).0;
        assert!(rows.iter().any(|r| r.contains("No matching models")));
        picker.handle_key(Key::KillToStart);
        assert!(picker.handle_key(Key::Enter).unwrap().is_some());
    }

    #[test]
    fn effort_auto_uses_selected_model_config_and_existing_override_is_preserved() {
        let mut live = client();
        live.set_effort(Some(Effort::Max));
        let mut picker = Picker::new(&live);
        picker.handle_key(Key::Up);
        assert_eq!(picker.pending.effort(), Effort::Max);
        for _ in 0..5 { picker.handle_key(Key::Left); }
        assert_eq!(picker.pending.effort_override(), None);
        assert_eq!(picker.pending.effort(), Effort::Low);
        picker.handle_key(Key::Tab);
        picker.handle_key(Key::Tab);
        assert_eq!(picker.pending.effort_override(), None);
    }

    #[test]
    fn includes_unlisted_models_and_adapts_panel_to_terminal_size() {
        let mut live = client().switch("gw/custom").unwrap();
        live.set_subagent_model(Some("gw/other-custom")).unwrap();
        let mut picker = Picker::new(&live);
        assert_eq!(picker.choices()[picker.selected].as_deref(), Some("gw/custom"));
        picker.handle_key(Key::Tab);
        assert_eq!(picker.choices()[picker.selected].as_deref(), Some("gw/other-custom"));
        for (width, height) in [(80, 24), (55, 20), (36, 18)] {
            let (rows, _) = picker.render(width, height, |_, s| s.into());
            assert!(rows.iter().all(|r| text::width(r) < width));
            assert!(rows.len() < height);
            assert!(rows.iter().any(|r| r.contains("Effort:")));
        }
    }
}
