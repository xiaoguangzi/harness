//! `/model`: preview main/subagent models and reasoning effort together.

use std::cell::Cell;

use super::Output;
use super::dialog::{self, LIST_ROWS};
use super::input::Draft;
use super::keys::Key;
use super::text;
use super::style::{ACCENT, DIM, GREEN, PLAIN, RED};
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
    dialog::run(out, Picker::new(client), Picker::handle_key).await
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
        if self.target == Target::Subagents && "follow the main model inherit".contains(&query) {
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
            key if self.filterable() => {
                self.query.edit(key);
                self.focus_current();
            }
            _ => {}
        }
        None
    }

    /// Whether typing filters the models: when there are too many to show
    /// whole.
    fn filterable(&self) -> bool {
        self.models.len() + 1 > LIST_ROWS
    }

    pub(super) fn render(&self, width: usize, height: usize, paint: dialog::Paint) -> dialog::Frame {
        let width = dialog::span(width);
        let room = width - 2;
        let row = |content: String| dialog::row(&content, room);
        let choices = self.choices();
        // As many rows as this list has unfiltered, so typing doesn't resize it.
        let all = self.models.len() + usize::from(self.target == Target::Subagents);
        let count = height.saturating_sub(12 + usize::from(self.filterable())).clamp(1, LIST_ROWS).min(all);
        let start = dialog::window(&self.top, self.selected, count, choices.len());
        let position = if choices.len() > count {
            format!("{}–{} of {}", start + 1, (start + count).min(choices.len()), choices.len())
        } else { String::new() };
        let mut rows = vec![dialog::heading("Model", &position, width, paint)];

        // The two models being set, the one the list is for marked.
        let values = [self.pending.spec(), self.pending.subagent_model().unwrap_or("follow the main model")];
        let names = ["Main model", "Subagents"];
        let value_room = room.saturating_sub(names.iter().map(|n| n.len() + 3).sum::<usize>() + 3) / 2;
        let tabs = names.iter().zip(values).enumerate().map(|(i, (name, value))| {
            let active = (self.target == Target::Main) == (i == 0);
            let value = text::fit_tail(&text::printable(value), value_room.max(1));
            let name = if active { paint(ACCENT, &format!("● {name}")) } else { paint(DIM, &format!("○ {name}")) };
            format!("{name} {}", paint(if active { PLAIN } else { DIM }, &value))
        });
        rows.extend([row(tabs.collect::<Vec<_>>().join("   ")), String::new()]);

        let mut cursor = None;
        if self.filterable() {
            let query = if self.query.is_empty() { paint(DIM, "type to filter") } else { text::printable(self.query.text()) };
            cursor = Some((rows.len(), (4 + text::width(&self.query.text()[..self.query.position()])).min(room + 1)));
            rows.push(row(format!("{} {query}", paint(DIM, "⌕"))));
        }
        let list_at = rows.len();
        let original = match self.target { Target::Main => Some(self.original.spec()), Target::Subagents => self.original.subagent_model() };
        for (i, choice) in choices.iter().enumerate().skip(start).take(count) {
            let chosen = i == self.selected;
            let mark = if choice.as_deref() == original { paint(GREEN, "✓") } else { " ".to_string() };
            let name = text::fit_tail(&text::printable(choice.as_deref().unwrap_or("Follow the main model")), room.saturating_sub(4));
            let lead = if chosen { paint(ACCENT, "❯") } else { " ".to_string() };
            rows.push(row(format!("{lead} {mark} {}", paint(if chosen { ACCENT } else { PLAIN }, &name))));
        }
        if choices.is_empty() { rows.push(row(paint(DIM, "No matching models"))); }
        while rows.len() < list_at + count { rows.push(String::new()); }
        rows.push(String::new());

        let preview = match self.target {
            Target::Subagents => self.pending.subagent_model().and_then(|spec| self.pending.switch(spec).ok()).unwrap_or_else(|| self.pending.clone()),
            Target::Main => self.pending.clone(),
        };
        let levels: Vec<&str> = std::iter::once("auto").chain(Effort::ALL.iter().map(|e| e.as_str())).collect();
        let mut effort = dialog::radio(&levels, self.effort_index(), paint);
        if text::width(&effort) + 8 > room {
            effort = format!("← {} →", paint(ACCENT, &format!("● {}", levels[self.effort_index()])));
        }
        rows.push(row(format!("{}  {effort}", paint(DIM, "Effort"))));
        let help = match self.pending.effort_override() {
            Some(effort) => super::parse::effort_help(effort).to_string(),
            None => format!("{}, as configured for the model", preview.effort().as_str()),
        };
        rows.push(row(format!("        {}", paint(DIM, &help))));

        let protocol = preview.spec().split_once('/').and_then(|(p, _)| preview.providers().protocol_of(p)).map_or("?", |p| p.as_str());
        let limits = format!("{protocol} · {} context · {} output", amount(preview.context_window()), amount(preview.max_tokens()));
        rows.push(row(match &self.error {
            Some(error) => paint(RED, &text::printable(error)),
            None if self.target == Target::Subagents => paint(DIM, &format!("{limits} · for new agents without a model of their own")),
            None => paint(DIM, &limits),
        }));
        rows.push(dialog::rule(width, paint));
        let other = if self.target == Target::Main { "subagents" } else { "main model" };
        let moves = if choices.len() > 1 { "↑↓" } else { "" };
        rows.push(dialog::hints(&[(moves, "model"), ("←→", "effort"), ("tab", other), ("enter", "apply"), ("esc", "cancel")], room, paint));
        (rows, cursor)
    }
}

impl dialog::Panel for Picker {
    fn render(&self, width: usize, height: usize, paint: dialog::Paint) -> dialog::Frame {
        Picker::render(self, width, height, paint)
    }
}

/// `200000` -> `200k`, `1000000` -> `1M`.
fn amount(n: u64) -> String {
    if n > 0 && n.is_multiple_of(1_000_000) { format!("{}M", n / 1_000_000) }
    else if n.is_multiple_of(1000) { format!("{}k", n / 1000) } else { n.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Providers;
    use crate::llm::config::tests::{file, no_env};
    use serde_json::json;

    fn client() -> Client {
        with_models(json!({"alpha": {"effort": "low"}, "beta": {}, "gamma": {}}))
    }

    fn with_models(models: serde_json::Value) -> Client {
        Client::new(Providers::from_parts(&file(json!({
            "model": "gw/beta", "subagent_model": "gw/alpha", "effort": "high",
            "providers": {"gw": {"protocol": "anthropic", "base_url": "http://127.0.0.1:9", "models": models}}
        })), &no_env).unwrap()).unwrap()
    }

    /// Enough models for the list to scroll, and so to filter.
    fn many() -> Client {
        let extra = (0..10).map(|i| (format!("m{i:02}"), json!({})));
        with_models([("alpha", json!({"effort": "low"})), ("beta", json!({})), ("gamma", json!({}))]
            .into_iter().map(|(name, model)| (name.to_string(), model)).chain(extra).collect::<serde_json::Map<_, _>>().into())
    }

    fn plain(picker: &Picker, width: usize, height: usize) -> Vec<String> {
        picker.render(width, height, &|_, s| s.into()).0.iter().map(|row| text::strip_ansi(row)).collect()
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
        let mut picker = Picker::new(&many());
        picker.handle_key(Key::Text("GAM".into()));
        assert_eq!(picker.pending.spec(), "gw/beta", "filtering only moves the highlight");
        picker.handle_key(Key::Tab);
        picker.handle_key(Key::Tab);
        assert_eq!(picker.pending.spec(), "gw/beta", "switching targets doesn't take the highlighted model");
        picker.handle_key(Key::Text("GAM".into()));
        assert_eq!(picker.handle_key(Key::Enter).unwrap().unwrap().spec(), "gw/gamma", "Enter takes it");
        picker.handle_key(Key::Text("unknown".into()));
        assert!(picker.handle_key(Key::Enter).is_none());
        assert!(plain(&picker, 80, 24).iter().any(|r| r.contains("No matching models")));
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
            let rows = plain(&picker, width, height);
            assert!(rows.iter().all(|r| text::width(r) < width), "{rows:?}");
            assert!(rows.len() < height);
            assert!(rows.iter().any(|r| r.contains("Effort")));
        }
    }

    #[test]
    fn shows_both_models_the_list_and_the_effort() {
        let mut picker = Picker::new(&client());
        assert_eq!(plain(&picker, 80, 24), [
            &format!("── Model {}", "─".repeat(70)),
            "  ● Main model gw/beta   ○ Subagents gw/alpha",
            "",
            "      gw/alpha",
            "  ❯ ✓ gw/beta",
            "      gw/gamma",
            "",
            "  Effort  ● auto  ○ low  ○ medium  ○ high  ○ xhigh  ○ max",
            "          high, as configured for the model",
            "  anthropic · 200k context · 64k output",
            &"─".repeat(79),
            "  ↑↓ model · ←→ effort · tab subagents · enter apply · esc cancel",
        ]);
        picker.handle_key(Key::Text("gam".into()));
        assert_eq!(picker.choices().len(), 3, "a list shown whole takes no filter");
        picker.handle_key(Key::Tab);
        picker.handle_key(Key::Right);
        let rows = plain(&picker, 80, 24);
        assert_eq!(rows[1], "  ○ Main model gw/beta   ● Subagents gw/alpha");
        assert_eq!(rows[3..5], ["      Follow the main model", "  ❯ ✓ gw/alpha"]);
        assert_eq!(rows[8..10], ["  Effort  ○ auto  ● low  ○ medium  ○ high  ○ xhigh  ○ max", "          Fastest and cheapest; simple tasks"]);
        assert!(rows[rows.len() - 1].contains("tab main model"));
        assert!(plain(&Picker::new(&many()), 80, 24).iter().any(|r| r == "  ⌕ type to filter"));
    }
}
