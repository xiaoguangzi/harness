//! `harness provider …` (and the REPL's `/provider …`): adding gateways and
//! the models listed on them, and choosing the default model, by editing
//! the settings file's `providers` and `model`. The REPL switches to the
//! changed setup at once; otherwise it applies at the next start.

use std::path::Path;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

use super::config::{Effort, Protocol, Providers};
use crate::plugins::manage::Ask;
use crate::settings::{self, Settings};

pub const USAGE: &str = "\
Usage: harness provider <command>

Commands:
  list                             The providers, their models and the default model
  add <name> [options]             Add a gateway, or change one; asks for what the options leave out
      --protocol P                 anthropic or openai-responses
      --base-url URL
      --api-key KEY                The key, or '${VAR}' to read it from the environment
      --model MODEL                List a model on it (repeatable)
  remove <name>                    Remove a gateway and its models
  model add <provider>/<model> [--context-window N] [--max-tokens N] [--effort LEVEL] [--default]
                                   List a model (/model offers it), with its own settings
  model remove <provider>/<model>  Take it off the list
  default <provider>/<model>       Make it the default model

Changes apply at the next start; /provider … in the REPL switches to them at once.
";

/// Whether `args` change the settings (rather than list them).
pub fn changes(args: &[String]) -> bool {
    matches!(args.first().map(String::as_str), Some("add" | "remove" | "rm" | "model" | "default"))
}

/// Runs `harness provider <args>`; returns what to print.
pub fn run(args: &[String], ask: &mut dyn Ask) -> Result<Vec<String>> {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    Manager { settings_file: None, env: &env }.run(args, ask)
}

/// Where the commands act: the settings file (the configured one unless
/// set, for tests), with the environment the providers are built with.
struct Manager<'a> {
    settings_file: Option<&'a Path>,
    env: &'a dyn Fn(&str) -> Option<String>,
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
                if let Some(value) = value {
                    p.insert(key.into(), value.into());
                    changed.push(key);
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

    #[test]
    fn adds_providers_and_models() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("settings.json");
        let no_env = |_: &str| None;
        let m = Manager { settings_file: Some(&file), env: &no_env };
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
    fn lists_providers_from_the_environment() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("settings.json");
        let env = |k: &str| (k == "ANTHROPIC_API_KEY").then(|| "sk".to_string());
        let m = Manager { settings_file: Some(&file), env: &env };
        let none = &mut Answers(Vec::new(), Vec::new());
        assert_eq!(run(&m, "", none).unwrap(), ["anthropic  (from the environment)", "default model: anthropic/claude-opus-5"]);
        // A default already works, so a new provider's model does not take
        // its place.
        let lines = run(&m, "add gw --protocol anthropic --base-url https://gw.example.com --model m", none).unwrap();
        assert_eq!(lines, ["added provider gw", "models: gw/m"]);
        assert!(read(&file).get("model").is_none());
    }
}
