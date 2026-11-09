//! `harness provider …` (and the REPL's `/provider …`): adding gateways and
//! the models listed on them, and choosing the default model, by editing
//! the settings file's `providers` and `model`. The REPL switches to the
//! changed setup at once; otherwise it applies at the next start.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};

use super::config::{Effort, ModelFilter, Protocol, ProviderConfig, Providers};
use crate::extensions::plugins::manage::Ask;
use crate::settings::{self, Settings};

pub const USAGE: &str = "\
Usage: harness provider <command>

Commands:
  list                             The providers, their models and the default model
  add <name> [options]             Add a gateway, or change one; asks for what the options leave out
      --protocol P                 anthropic or openai-responses
      --base-url URL
      --api-key KEY                The key, or '${VAR}' to read it from the environment ('' for none)
      --model MODEL                List a model on it (repeatable)
  remove <name>                    Remove a gateway and its models
  model add <provider>/<model> [--context-window N] [--max-tokens N] [--effort LEVEL] [--default]
                                   List a model (/model offers it), with its own settings
  model remove <provider>/<model>  Take it off the list
  sync <name> [--include PATTERN] [--exclude PATTERN]
                                   List the models the gateway's /v1/models gives, each keeping its
                                   settings; the patterns (* and ?, repeatable) are kept for later syncs
  default <provider>/<model>       Make it the default model

Changes apply at the next start; /provider … in the REPL switches to them at once.
";

/// Whether `args` change the settings (rather than list them).
pub fn changes(args: &[String]) -> bool {
    matches!(args.first().map(String::as_str), Some("add" | "remove" | "rm" | "model" | "sync" | "default"))
}

/// Runs `harness provider <args>`; returns what to print.
pub fn run(args: &[String], ask: &mut dyn Ask) -> Result<Vec<String>> {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    Manager { settings_file: None, env: &env, fetch: &list_models }.run(args, ask)
}

/// Where the commands act: the settings file (the configured one unless
/// set, for tests), with the environment the providers are built with.
struct Manager<'a> {
    settings_file: Option<&'a Path>,
    env: &'a dyn Fn(&str) -> Option<String>,
    /// The entries of the model list a gateway gives.
    fetch: &'a dyn Fn(&ProviderConfig) -> Result<Vec<Value>>,
}

impl Manager<'_> {
    fn run(&self, args: &[String], ask: &mut dyn Ask) -> Result<Vec<String>> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match args.as_slice() {
            ["help" | "-h" | "--help"] => Ok(USAGE.lines().map(String::from).collect()),
            [] | ["list"] => self.list(),
            ["add", name, options @ ..] => self.add(name, options, ask),
            ["remove" | "rm", name] => self.remove(name),
            ["model", "add", spec, options @ ..] => self.add_model(spec, options),
            ["model", "remove" | "rm", spec] => self.remove_model(spec),
            ["sync", name, options @ ..] => self.sync(name, options),
            ["default", spec] => self.set_default(spec),
            _ => bail!("unknown provider command `{}`; see `harness provider help`", args.join(" ")),
        }
    }

    fn settings(&self) -> Result<Settings> {
        match self.settings_file {
            None => Settings::load(),
            Some(file) => Settings::parse(&std::fs::read_to_string(file).unwrap_or_default()),
        }
    }

    /// Changes the settings with `change`, then adds to its lines why the
    /// result cannot be used yet, if it cannot.
    fn edit(&self, change: impl FnOnce(&mut Map<String, Value>) -> Result<Vec<String>>) -> Result<Vec<String>> {
        let mut lines = match self.settings_file {
            None => settings::edit(change),
            Some(file) => settings::edit_file(file, change),
        }?;
        if let Err(e) = self.providers(&self.settings()?) {
            lines.push(format!("not usable yet: {e:#}"));
        }
        Ok(lines)
    }

    fn providers(&self, settings: &Settings) -> Result<Providers> {
        Providers::from_parts(settings, self.env)
    }

    fn list(&self) -> Result<Vec<String>> {
        let settings = self.settings()?;
        let mut lines = Vec::new();
        let providers = self.providers(&settings);
        let implicit = providers.iter().flat_map(|p| p.names()).filter(|n| !settings.providers.contains_key(*n));
        for name in implicit {
            lines.push(format!("{name}  (from the environment)"));
        }
        for (name, p) in &settings.providers {
            let key = match p.api_key.as_deref() {
                Some(k) if k.contains("${") => format!(", key {k}"),
                Some(_) => ", key set".to_string(),
                None => String::new(),
            };
            lines.push(format!("{name}  {} {}{key}", p.protocol.as_str(), p.base_url));
            for model in p.models.keys() {
                lines.push(format!("  {name}/{model}"));
            }
        }
        match providers {
            Ok(p) => {
                lines.push(format!("default model: {}", p.default_model));
                if let Some(sub) = &p.subagent_model {
                    lines.push(format!("subagent model: {sub}"));
                }
            }
            Err(e) => lines.push(format!("not usable yet: {e:#}")),
        }
        Ok(lines)
    }

    fn add(&self, name: &str, args: &[&str], ask: &mut dyn Ask) -> Result<Vec<String>> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
            bail!("`{name}`: a provider name is letters, digits, `.`, `_` and `-`");
        }
        let options = options(args, &["protocol", "base-url", "api-key", "model"], &[])?;
        let given = |key: &str| options.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| v.to_string());
        let mut protocol = given("protocol").map(|p| parse_protocol(&p)).transpose()?;
        let mut base_url = given("base-url");
        let mut api_key = given("api-key");
        let mut models: Vec<String> = options.iter().filter(|(k, _)| *k == "model").map(|(_, v)| v.to_string()).collect();

        let settings = self.settings()?;
        let existing = settings.providers.get(name);
        // A new provider asks for what the options leave out; an existing
        // one only when there are no options, to change it.
        if existing.is_none() || options.is_empty() {
            if protocol.is_none() {
                let current = existing.map_or(Protocol::Anthropic, |p| p.protocol);
                let answer = ask.ask(&format!("Protocol, anthropic or openai-responses [{}]: ", current.as_str()), false);
                protocol = Some(answer.filter(|a| !a.trim().is_empty()).map_or(Ok(current), |a| parse_protocol(&a))?);
            }
            if base_url.is_none() {
                let current = existing.map(|p| p.base_url.clone()).unwrap_or_else(|| default_url(protocol.unwrap()).into());
                let answer = ask.ask(&format!("Base URL [{current}]: "), false);
                base_url = Some(answer.map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).unwrap_or(current));
            }
            if api_key.is_none() {
                let keep = if existing.is_some_and(|p| p.api_key.is_some()) { "keeps the current one" } else { "for none" };
                let question = format!("API key, or ${{VAR}} to read it from the environment (empty {keep}): ");
                let answer = ask.ask(&question, true);
                api_key = answer.map(|a| a.trim().to_string()).filter(|a| !a.is_empty());
            }
            if models.is_empty() {
                let answer = ask.ask("Models to list, separated by spaces (optional): ", false).unwrap_or_default();
                models = answer.split_whitespace().map(String::from).collect();
            }
        }

        self.edit(|s| {
            let had_default = self.providers(&parse(s)?).is_ok();
            let p = settings::object(settings::object(s, "providers"), name);
            let mut changed = Vec::new();
            let protocol = protocol.map(|p| p.as_str().to_string());
            for (key, value) in [("protocol", protocol), ("base_url", base_url), ("api_key", api_key)] {
                match value {
                    // `--api-key ''`: none.
                    Some(value) if value.is_empty() => {
                        if p.shift_remove(key).is_some() {
                            changed.push(key);
                        }
                    }
                    Some(value) => {
                        p.insert(key.into(), value.into());
                        changed.push(key);
                    }
                    None => {}
                }
            }
            if !models.is_empty() {
                let listed = settings::object(p, "models");
                for model in &models {
                    listed.entry(model.as_str()).or_insert_with(|| json!({}));
                }
            }
            let mut lines = vec![match existing {
                None => format!("added provider {name}"),
                Some(_) if changed.is_empty() && models.is_empty() => format!("provider {name} is unchanged"),
                Some(_) => format!("changed provider {name}"),
            }];
            if !models.is_empty() {
                lines.push(format!("models: {}", models.iter().map(|m| format!("{name}/{m}")).collect::<Vec<_>>().join(", ")));
            }
            // The first model becomes the default when there is none to
            // keep, so the setup works straight away.
            if !had_default && let Some(model) = models.first() {
                s.insert("model".into(), format!("{name}/{model}").into());
                lines.push(format!("default model: {name}/{model}"));
            }
            Ok(lines)
        })
    }

    fn remove(&self, name: &str) -> Result<Vec<String>> {
        self.edit(|s| {
            let providers = settings::object(s, "providers");
            if providers.shift_remove(name).is_none() {
                bail!("no provider `{name}` in the settings");
            }
            if providers.is_empty() {
                s.shift_remove("providers");
            }
            let mut lines = vec![format!("removed provider {name}")];
            for key in ["model", "subagent_model"] {
                let on_it = s.get(key).and_then(Value::as_str).and_then(|m| m.split_once('/')).is_some_and(|(p, _)| p == name);
                if on_it {
                    s.shift_remove(key);
                    lines.push(format!("`{key}` was one of its models, and is unset now"));
                }
            }
            Ok(lines)
        })
    }

    fn add_model(&self, spec: &str, args: &[&str]) -> Result<Vec<String>> {
        let (provider, model) = self.split_spec(spec)?;
        let options = options(args, &["context-window", "max-tokens", "effort"], &["default"])?;
        let mut config = Map::new();
        for (key, value) in &options {
            match *key {
                "context-window" | "max-tokens" => {
                    let n: u32 = value.parse().map_err(|_| anyhow!("--{key} takes a number of tokens, not `{value}`"))?;
                    config.insert(key.replace('-', "_"), n.into());
                }
                "effort" => {
                    config.insert("effort".into(), Effort::parse(value)?.as_str().into());
                }
                _ => {}
            }
        }
        let default = options.iter().any(|(k, _)| *k == "default");
        self.edit(|s| {
            let models = settings::object(settings::object(settings::object(s, "providers"), provider), "models");
            let added = !models.contains_key(model);
            settings::object(models, model).extend(config);
            let mut lines = vec![format!("{} {provider}/{model}", if added { "listed" } else { "changed" })];
            if default {
                s.insert("model".into(), format!("{provider}/{model}").into());
                lines.push(format!("default model: {provider}/{model}"));
            }
            Ok(lines)
        })
    }

    fn remove_model(&self, spec: &str) -> Result<Vec<String>> {
        let (provider, model) = self.split_spec(spec)?;
        self.edit(|s| {
            let p = settings::object(settings::object(s, "providers"), provider);
            let models = settings::object(p, "models");
            if models.shift_remove(model).is_none() {
                bail!("{provider}/{model} is not listed");
            }
            if models.is_empty() {
                p.shift_remove("models");
            }
            Ok(vec![format!("took {provider}/{model} off the list (it can still be used by name)")])
        })
    }

    /// Lists on `name` the models its gateway gives that its filter admits
    /// (the options' patterns, which replace the saved ones, or those): a
    /// listed model keeps its settings, one without a context window gets
    /// the gateway's (unless the provider sets one for all), and those it
    /// no longer gives go.
    fn sync(&self, name: &str, args: &[&str]) -> Result<Vec<String>> {
        let options = options(args, &["include", "exclude"], &[])?;
        let given = |key: &str| options.iter().filter(|(k, _)| *k == key).map(|(_, v)| v.to_string()).collect();
        let settings = self.settings()?;
        let mut config = settings.providers.get(name).cloned().ok_or_else(|| {
            anyhow!("no provider `{name}` in the settings; add it with `harness provider add {name}`")
        })?;
        let filter = if options.is_empty() {
            config.sync.clone().unwrap_or_default()
        } else {
            ModelFilter { include: given("include"), exclude: given("exclude") }
        };
        config.expand();
        let offered = (self.fetch)(&config)?;
        let offered: Vec<(&str, &Value)> = offered
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).map(|id| (id, m)))
            .collect();
        let found: BTreeMap<String, Option<u32>> = offered
            .iter()
            .filter(|(id, _)| filter.admits(id))
            .map(|(id, model)| (id.to_string(), context_window(model)))
            .collect();
        let patterns = describe(&filter);
        if found.is_empty() {
            let matching = if patterns.is_empty() { String::new() } else { format!(" matching {patterns}") };
            bail!("{} lists no models{matching} ({} in all); nothing changed", config.url("models"), offered.len());
        }

        self.edit(|s| {
            let p = settings::object(settings::object(s, "providers"), name);
            let mut changed = false;
            if !options.is_empty() {
                let mut saved = Map::new();
                for (key, patterns) in [("include", &filter.include), ("exclude", &filter.exclude)] {
                    if !patterns.is_empty() {
                        saved.insert(key.into(), json!(patterns));
                    }
                }
                let saved = (!saved.is_empty()).then_some(Value::Object(saved));
                changed = p.get("sync") != saved.as_ref();
                match saved {
                    Some(saved) => p.insert("sync".into(), saved),
                    None => p.shift_remove("sync"),
                };
            }
            let old = p.get("models").and_then(Value::as_object).cloned().unwrap_or_default();
            let mut models = Map::new();
            for (id, window) in &found {
                let mut entry = old.get(id).cloned().unwrap_or_else(|| json!({}));
                if let (Some(n), Some(e)) = (window, entry.as_object_mut())
                    && config.context_window.is_none()
                    && !e.contains_key("context_window")
                {
                    e.insert("context_window".into(), (*n).into());
                }
                models.insert(id.clone(), entry);
            }
            changed |= old != models;
            p.insert("models".into(), Value::Object(models));

            let spec = |m: &String| format!("{name}/{m}");
            let added: Vec<String> = found.keys().filter(|m| !old.contains_key(*m)).map(spec).collect();
            let removed: Vec<String> = old.keys().filter(|m| !found.contains_key(*m)).map(spec).collect();
            let of = format!("{} of the {} models it lists", found.len(), offered.len());
            let by = if patterns.is_empty() { String::new() } else { format!(", by {patterns}") };
            let state = if changed { "synced provider" } else { "unchanged provider" };
            let mut lines = vec![format!("{state} {name}: {of}{by}")];
            for (what, specs) in [("added", &added), ("removed", &removed)] {
                if !specs.is_empty() {
                    lines.push(format!("{what}: {}", specs.join(", ")));
                }
            }
            for key in ["model", "subagent_model"] {
                if let Some(set) = s.get(key).and_then(Value::as_str).filter(|m| removed.iter().any(|r| r == m)) {
                    lines.push(format!("`{key}` is {set}, which the gateway no longer lists"));
                }
            }
            Ok(lines)
        })
    }

    fn set_default(&self, spec: &str) -> Result<Vec<String>> {
        let mut settings = self.settings()?;
        settings.model = Some(spec.to_string());
        let resolved = self.providers(&settings)?.default_model;
        self.edit(|s| {
            s.insert("model".into(), resolved.clone().into());
            Ok(vec![format!("default model: {resolved}")])
        })
    }

    /// `<provider>/<model>`, for a provider in the settings.
    fn split_spec<'s>(&self, spec: &'s str) -> Result<(&'s str, &'s str)> {
        let (provider, model) = spec.split_once('/').filter(|(p, m)| !p.is_empty() && !m.is_empty()).ok_or_else(|| {
            anyhow!("`{spec}` is not <provider>/<model>")
        })?;
        if !self.settings()?.providers.contains_key(provider) {
            bail!("no provider `{provider}` in the settings; add it with `harness provider add {provider}`");
        }
        Ok((provider, model))
    }
}

/// The settings `s` holds.
fn parse(s: &Map<String, Value>) -> Result<Settings> {
    Ok(serde_json::from_value(Value::Object(s.clone()))?)
}

fn parse_protocol(s: &str) -> Result<Protocol> {
    serde_json::from_value(json!(s.trim())).map_err(|_| anyhow!("unknown protocol `{s}` (anthropic or openai-responses)"))
}

fn default_url(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Anthropic => "https://api.anthropic.com",
        Protocol::OpenAiResponses => "https://api.openai.com/v1",
    }
}

/// The context window an entry of a gateway's model list gives, under one
/// of the names gateways (magpie, OpenRouter, LiteLLM…) give it.
fn context_window(model: &Value) -> Option<u32> {
    let fields = ["/context_window", "/context_length", "/max_input_tokens", "/top_provider/context_length"];
    let n = fields.iter().find_map(|f| model.pointer(f).and_then(Value::as_u64))?;
    u32::try_from(n).ok().filter(|&n| n > 0)
}

/// `include a, b; exclude c`, or nothing for a filter that takes all.
fn describe(filter: &ModelFilter) -> String {
    let parts = [("include", &filter.include), ("exclude", &filter.exclude)];
    let parts = parts.iter().filter(|(_, p)| !p.is_empty()).map(|(what, p)| format!("{what} {}", p.join(", ")));
    parts.collect::<Vec<_>>().join("; ")
}

/// The entries of the model list the gateway gives at `/v1/models`, page
/// after page where it pages them as the Anthropic API does.
fn list_models(config: &ProviderConfig) -> Result<Vec<Value>> {
    // A runtime of its own, as the callers block their thread.
    std::thread::scope(|s| {
        let listing = s.spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            runtime.block_on(fetch_model_pages(config))
        });
        listing.join().unwrap_or_else(|_| Err(anyhow!("listing the gateway's models failed")))
    })
}

async fn fetch_model_pages(config: &ProviderConfig) -> Result<Vec<Value>> {
    let http = super::http_client()?;
    let mut headers = config.headers();
    if config.protocol == Protocol::Anthropic {
        headers.push(("anthropic-version".into(), super::anthropic::API_VERSION.into()));
    }
    let url = config.url("models");
    let mut models = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let mut page_url = reqwest::Url::parse(&url).with_context(|| format!("`{url}` is not a URL"))?;
        if config.protocol == Protocol::Anthropic {
            // The official API gives 20 a page unless asked for more.
            page_url.query_pairs_mut().append_pair("limit", "1000");
            if let Some(id) = &after {
                page_url.query_pairs_mut().append_pair("after_id", id);
            }
        }
        let mut request = http.get(page_url).timeout(Duration::from_secs(30));
        for (name, value) in &headers {
            request = request.header(name, value);
        }
        let response = request.send().await.with_context(|| format!("asking {url} for its models"))?;
        let status = response.status();
        let body = response.text().await.with_context(|| format!("reading {url}'s model list"))?;
        if !status.is_success() {
            bail!("{url} answered {status}: {}", crate::util::one_line(&body, 300));
        }
        let page: Value = serde_json::from_str(&body).with_context(|| format!("{url} answered with no JSON"))?;
        let Some(data) = page.get("data").and_then(Value::as_array) else {
            bail!("{url} answered with no `data` list of models");
        };
        models.extend(data.iter().cloned());
        let last = page.get("last_id").and_then(Value::as_str).map(String::from);
        let more = page.get("has_more").and_then(Value::as_bool) == Some(true);
        if config.protocol != Protocol::Anthropic || !more || data.is_empty() || last.is_none() || last == after {
            return Ok(models);
        }
        after = last;
    }
}

/// `--name value` / `--name=value` options for the names in `valued`, and
/// `--name` alone for those in `flags` (with an empty value).
fn options<'a>(args: &[&'a str], valued: &[&str], flags: &[&str]) -> Result<Vec<(&'a str, &'a str)>> {
    let mut found = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(option) = arg.strip_prefix("--") else { bail!("unexpected `{arg}`") };
        let (name, value) = match option.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (option, None),
        };
        if flags.contains(&name) && value.is_none() {
            found.push((name, ""));
        } else if valued.contains(&name) {
            let value = value.or_else(|| args.next().copied()).ok_or_else(|| anyhow!("--{name} needs a value"))?;
            found.push((name, value));
        } else {
            bail!("unknown option `--{name}`");
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers questions from a list, and remembers them.
    struct Answers(Vec<&'static str>, Vec<String>);

    impl Ask for Answers {
        fn ask(&mut self, question: &str, _secret: bool) -> Option<String> {
            self.1.push(question.to_string());
            (!self.0.is_empty()).then(|| self.0.remove(0).to_string())
        }
    }

    fn run(m: &Manager, args: &str, ask: &mut dyn Ask) -> Result<Vec<String>> {
        let args: Vec<String> = args.split_whitespace().map(String::from).collect();
        m.run(&args, ask)
    }

    fn read(file: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap()
    }

    fn no_fetch(_: &ProviderConfig) -> Result<Vec<Value>> {
        bail!("no network in tests")
    }

    #[test]
    fn adds_providers_and_models() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("settings.json");
        let no_env = |_: &str| None;
        let m = Manager { settings_file: Some(&file), env: &no_env, fetch: &no_fetch };
        let none = &mut Answers(Vec::new(), Vec::new());

        assert!(run(&m, "list", none).unwrap().last().unwrap().starts_with("not usable yet: no LLM provider"));
        // Asked for everything: an empty answer takes the default.
        let ask = &mut Answers(vec!["openai-responses", "", "${RELAY_KEY}", "gpt-5 gpt-5-mini"], Vec::new());
        let lines = run(&m, "add relay", ask).unwrap();
        assert_eq!(lines, ["added provider relay", "models: relay/gpt-5, relay/gpt-5-mini", "default model: relay/gpt-5"]);
        assert_eq!(ask.1[1], "Base URL [https://api.openai.com/v1]: ");
        assert_eq!(
            read(&file),
            json!({
                "providers": {"relay": {
                    "protocol": "openai-responses", "base_url": "https://api.openai.com/v1", "api_key": "${RELAY_KEY}",
                    "models": {"gpt-5": {}, "gpt-5-mini": {}}
                }},
                "model": "relay/gpt-5"
            })
        );

        // Options leave nothing to ask; the default stays.
        let lines = run(&m, "add gw --protocol=anthropic --base-url https://gw.example.com --api-key k --model opus", none);
        assert_eq!(lines.unwrap(), ["added provider gw", "models: gw/opus"]);
        assert!(none.1.is_empty(), "{:?}", none.1);
        assert_eq!(read(&file)["model"], "relay/gpt-5");
        let lines = run(&m, "model add gw/sonnet --context-window 1000000 --effort MAX --default", none).unwrap();
        assert_eq!(lines, ["listed gw/sonnet", "default model: gw/sonnet"]);
        assert_eq!(read(&file)["providers"]["gw"]["models"]["sonnet"], json!({"context_window": 1000000, "effort": "max"}));
        assert_eq!(run(&m, "model add gw/sonnet --max-tokens 8000", none).unwrap(), ["changed gw/sonnet"]);
        assert_eq!(read(&file)["providers"]["gw"]["models"]["sonnet"]["context_window"], 1000000, "kept");
        assert_eq!(run(&m, "default relay/gpt-5-mini", none).unwrap(), ["default model: relay/gpt-5-mini"]);
        assert!(run(&m, "default gpt-6", none).is_err(), "names no provider");
        assert!(run(&m, "default nope/x", none).is_err());
        assert_eq!(
            run(&m, "list", none).unwrap(),
            [
                "gw  anthropic https://gw.example.com, key set",
                "  gw/opus",
                "  gw/sonnet",
                "relay  openai-responses https://api.openai.com/v1, key ${RELAY_KEY}",
                "  relay/gpt-5",
                "  relay/gpt-5-mini",
                "default model: relay/gpt-5-mini",
            ]
        );

        // Changing one with options asks nothing and keeps the rest.
        run(&m, "add gw --base-url https://gw2.example.com", none).unwrap();
        assert_eq!(read(&file)["providers"]["gw"]["api_key"], "k");
        assert_eq!(read(&file)["providers"]["gw"]["base_url"], "https://gw2.example.com");
        // An empty key is none.
        assert_eq!(run(&m, "add gw --api-key=", none).unwrap(), ["changed provider gw"]);
        assert!(read(&file)["providers"]["gw"].get("api_key").is_none());
        run(&m, "add gw --api-key k", none).unwrap();

        assert!(run(&m, "model add nope/x", none).is_err());
        assert!(run(&m, "model add gw/x --context-window lots", none).is_err());
        assert!(run(&m, "add a/b", none).is_err());
        assert!(run(&m, "add x --protocol grpc", none).is_err());
        assert!(run(&m, "add x --colour red", none).is_err());

        assert_eq!(run(&m, "model remove gw/opus", none).unwrap().len(), 1);
        assert!(run(&m, "model remove gw/opus", none).is_err());
        let lines = run(&m, "remove relay", none).unwrap();
        assert_eq!(lines, ["removed provider relay", "`model` was one of its models, and is unset now"]);
        assert_eq!(run(&m, "list", none).unwrap().last().unwrap(), "default model: gw/sonnet", "the sole provider's first");
        run(&m, "model remove gw/sonnet", none).unwrap();
        assert!(run(&m, "list", none).unwrap().last().unwrap().starts_with("not usable yet: no default model"));
        assert_eq!(read(&file)["providers"].as_object().unwrap().keys().collect::<Vec<_>>(), ["gw"]);
        assert!(run(&m, "remove relay", none).is_err());
    }

    #[test]
    fn syncs_the_models_a_gateway_lists() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("settings.json");
        let settings = json!({
            "model": "gw/zcode/glm",
            "providers": {
                "gw": {
                    "protocol": "anthropic", "base_url": "https://gw.example.com/",
                    "models": {"claude/opus": {"effort": "max"}, "zcode/glm": {}}
                },
                "flat": {"protocol": "anthropic", "base_url": "https://flat.example.com", "context_window": 128000}
            }
        });
        std::fs::write(&file, settings.to_string()).unwrap();
        let asked = std::cell::RefCell::new(Vec::new());
        let fetch = |config: &ProviderConfig| -> Result<Vec<Value>> {
            asked.borrow_mut().push(config.url("models"));
            Ok(vec![
                json!({"id": "claude/opus", "context_window": 1000000}),
                json!({"id": "claude/haiku", "context_length": 200000}),
                json!({"id": "codex/gpt", "top_provider": {"context_length": 272000}}),
                json!({"id": "claude/opus-20250101"}),
                json!({"object": "model"}),
            ])
        };
        let no_env = |_: &str| None;
        let m = Manager { settings_file: Some(&file), env: &no_env, fetch: &fetch };
        let none = &mut Answers(Vec::new(), Vec::new());
        let gw = || read(&file)["providers"]["gw"].clone();

        // Listed models keep their settings and get the gateway's context
        // window; those it doesn't list go; the patterns are kept.
        let lines = run(&m, "sync gw --exclude codex/* --exclude *-2025????", none).unwrap();
        assert_eq!(
            lines,
            [
                "synced provider gw: 2 of the 4 models it lists, by exclude codex/*, *-2025????",
                "added: gw/claude/haiku",
                "removed: gw/zcode/glm",
                "`model` is gw/zcode/glm, which the gateway no longer lists",
            ]
        );
        assert_eq!(asked.borrow().as_slice(), ["https://gw.example.com/v1/models"]);
        assert_eq!(
            gw()["models"],
            json!({
                "claude/haiku": {"context_window": 200000},
                "claude/opus": {"effort": "max", "context_window": 1000000}
            })
        );
        assert_eq!(gw()["sync"], json!({"exclude": ["codex/*", "*-2025????"]}));

        // Without options, the saved patterns.
        let lines = run(&m, "sync gw", none).unwrap();
        assert_eq!(lines, ["unchanged provider gw: 2 of the 4 models it lists, by exclude codex/*, *-2025????"]);
        // New ones replace them; case doesn't matter.
        let lines = run(&m, "sync gw --include CODEX/*", none).unwrap();
        assert_eq!(lines[0], "synced provider gw: 1 of the 4 models it lists, by include CODEX/*");
        assert_eq!(gw()["models"], json!({"codex/gpt": {"context_window": 272000}}));
        assert_eq!(gw()["sync"], json!({"include": ["CODEX/*"]}));

        // Nothing matching changes nothing.
        let err = run(&m, "sync gw --include gemini-*", none).unwrap_err().to_string();
        assert_eq!(err, "https://gw.example.com/v1/models lists no models matching include gemini-* (4 in all); nothing changed");
        assert_eq!(gw()["sync"], json!({"include": ["CODEX/*"]}));

        // A provider with a context window of its own gets none per model.
        let lines = run(&m, "sync flat", none).unwrap();
        assert_eq!(lines[0], "synced provider flat: 4 of the 4 models it lists");
        let flat = &read(&file)["providers"]["flat"];
        assert_eq!(flat["models"]["codex/gpt"], json!({}));
        assert!(flat.get("sync").is_none());

        assert!(run(&m, "sync nope", none).is_err());
        assert!(run(&m, "sync gw --colour red", none).is_err());
        assert!(changes(&["sync".to_string()]));
    }

    #[test]
    fn lists_providers_from_the_environment() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("settings.json");
        let env = |k: &str| (k == "ANTHROPIC_API_KEY").then(|| "sk".to_string());
        let m = Manager { settings_file: Some(&file), env: &env, fetch: &no_fetch };
        let none = &mut Answers(Vec::new(), Vec::new());
        assert_eq!(run(&m, "", none).unwrap(), ["anthropic  (from the environment)", "default model: anthropic/claude-opus-5"]);
        // A default already works, so a new provider's model does not take
        // its place.
        let lines = run(&m, "add gw --protocol anthropic --base-url https://gw.example.com --model m", none).unwrap();
        assert_eq!(lines, ["added provider gw", "models: gw/m"]);
        assert!(read(&file).get("model").is_none());
    }
}
