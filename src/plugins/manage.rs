//! `harness plugin …` (and the REPL's `/plugins …`): installing plugins,
//! adding marketplaces and answering plugins' settings, by editing the
//! settings file's `enabledPlugins`, `extraKnownMarketplaces` and
//! `pluginConfigs` (the sensitive answers go to
//! `~/.harness/plugins/secrets.json` instead). Changes apply at the next
//! start, or after `/reload-plugins`.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde_json::{Map, Value, json};

use super::{
    Catalog, Context, Loaded, MarketplaceSource, OFFICIAL, OptionSpec, Plugin, home, read_json, read_secrets, sanitize,
    secrets_file, valid_marketplace_name,
};
use crate::settings::{self, Settings};

pub const USAGE: &str = "\
Usage: harness plugin <command>

Commands:
  list [--available]               The plugins in the settings; with --available, also what the
                                   known marketplaces offer
  install <plugin>@<marketplace>   Fetch a plugin, enable it and ask for the settings it needs
  uninstall <plugin>@<marketplace> Take it out of the settings and delete its files
                                   (--keep-data keeps its data directory)
  enable <plugin>@<marketplace>    Turn an installed plugin on
  disable <plugin>@<marketplace>   Turn it off, keeping it installed
  configure <plugin>@<marketplace> Answer the settings it asks for again
  update                           Fetch the latest version of every plugin and marketplace
  marketplace add <source> [--auto-update]
                                   Add a marketplace: owner/repo, a git URL (#ref picks a branch
                                   or tag), a directory or a marketplace.json; --auto-update
                                   fetches its latest in the background at each start
  marketplace remove <name>        Remove it, and the plugins installed from it
  marketplace list                 The marketplaces the harness knows

Changes apply at the next start, or after /reload-plugins in the REPL.
";

/// Asks the user a question, for a plugin's settings; `None` if there is
/// nobody to answer.
pub trait Ask {
    fn ask(&mut self, question: &str, secret: bool) -> Option<String>;
}

/// Asks on the terminal: the question on stderr, the answer from stdin,
/// not echoed if it is secret.
pub struct TerminalAsk;

impl Ask for TerminalAsk {
    fn ask(&mut self, question: &str, secret: bool) -> Option<String> {
        eprint!("{question}");
        let _ = std::io::stderr().flush();
        let terminal = std::io::stdin().is_terminal();
        let hidden = secret && terminal && stty("-echo");
        let mut line = String::new();
        let read = std::io::stdin().lock().read_line(&mut line);
        if hidden {
            stty("echo");
        }
        // What was typed ended the line, unless it was hidden or piped in.
        if hidden || !terminal {
            eprintln!();
        }
        match read {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_string()),
        }
    }
}

fn stty(mode: &str) -> bool {
    std::process::Command::new("stty")
        .arg(mode)
        .stdin(std::fs::File::open("/dev/tty").map_or(std::process::Stdio::null(), Into::into))
        .status()
        .is_ok_and(|s| s.success())
}

/// Runs `harness plugin <args>`; returns what to print.
pub fn run(args: &[String], ask: &mut dyn Ask) -> Result<Vec<String>> {
    let home = home();
    let personal = std::env::var_os("HOME").map(|h| Path::new(&h).join(".agents/plugins/marketplace.json"));
    let manager = Manager { home: &home, personal: personal.as_deref(), settings_file: None };
    manager.run(args, ask)
}

/// Where the commands act: the plugins' home, Codex's personal marketplace,
/// and the settings file (the configured one unless set, for tests).
struct Manager<'a> {
    home: &'a Path,
    personal: Option<&'a Path>,
    settings_file: Option<&'a Path>,
}

impl Manager<'_> {
    fn run(&self, args: &[String], ask: &mut dyn Ask) -> Result<Vec<String>> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match args.as_slice() {
            [] | ["help" | "-h" | "--help"] => Ok(USAGE.lines().map(String::from).collect()),
            ["list"] => self.list(false),
            ["list", "--available"] => self.list(true),
            ["install", id] => self.install(id, ask),
            ["uninstall" | "remove", id] => self.uninstall(id, false),
            ["uninstall" | "remove", id, "--keep-data"] | ["uninstall" | "remove", "--keep-data", id] => {
                self.uninstall(id, true)
            }
            ["enable", id] => self.switch(id, true),
            ["disable", id] => self.switch(id, false),
            ["configure", id] => self.configure(id, ask),
            ["update"] => self.update(),
            ["marketplace", "add", source] => self.add_marketplace(source, false),
            ["marketplace", "add", source, "--auto-update"] | ["marketplace", "add", "--auto-update", source] => {
                self.add_marketplace(source, true)
            }
            ["marketplace", "remove" | "rm", name] => self.remove_marketplace(name),
            ["marketplace", "list"] | ["marketplace"] => self.list_marketplaces(),
            _ => bail!("unknown plugin command `{}`; see `harness plugin help`", args.join(" ")),
        }
    }

    fn settings(&self) -> Result<Settings> {
        match self.settings_file {
            None => Settings::load(),
            Some(file) => Settings::parse(&std::fs::read_to_string(file).unwrap_or_default()),
        }
    }

    fn edit<T>(&self, change: impl FnOnce(&mut Map<String, Value>) -> Result<T>) -> Result<T> {
        match self.settings_file {
            None => settings::edit(change),
            Some(file) => settings::edit_file(file, change),
        }
    }

    fn context<'a>(&'a self, settings: &'a Settings) -> Context<'a> {
        Context { settings, home: self.home, personal: self.personal, secrets: read_secrets(self.home) }
    }

    /// Fetches plugin `id` from its marketplace and loads it, to see what
    /// it has.
    fn locate(&self, id: &str, settings: &Settings) -> Result<Plugin> {
        let (name, market) = split_id(id)?;
        let context = self.context(settings);
        let catalog = Catalog::open(market, &context).map_err(anyhow::Error::msg)?;
        let (root, entry, _) = catalog.locate(name, self.home).map_err(anyhow::Error::msg)?;
        let mut loaded = Loaded::default();
        loaded.add(Some(id), &root, Some(&entry), &context).map_err(anyhow::Error::msg)?;
        Ok(loaded.plugins.list.remove(0))
    }

    fn list(&self, available: bool) -> Result<Vec<String>> {
        let settings = self.settings()?;
        let mut lines = Vec::new();
        if settings.enabled_plugins.is_empty() {
            lines.push("no plugins installed (harness plugin install <plugin>@<marketplace>)".to_string());
        }
        for (id, on) in &settings.enabled_plugins {
            let state = if *on { "enabled" } else { "disabled" };
            lines.push(format!("{} {id}  {state}", if *on { "●" } else { "○" }));
        }
        if !available {
            return Ok(lines);
        }
        let context = self.context(&settings);
        for name in self.marketplace_names(&settings) {
            lines.push(String::new());
            match Catalog::open(&name, &context) {
                Err(e) => lines.push(format!("{name}: {e}")),
                Ok(catalog) => {
                    lines.push(format!("{name} ({} plugins)", catalog.plugins.len()));
                    for entry in &catalog.plugins {
                        let plugin = entry["name"].as_str().unwrap_or("?");
                        let id = format!("{plugin}@{name}");
                        let mark = match settings.enabled_plugins.get(&id) {
                            Some(true) => " [enabled]",
                            Some(false) => " [disabled]",
                            None => "",
                        };
                        let description = entry["description"].as_str().map(|d| format!(": {d}")).unwrap_or_default();
                        lines.push(format!("  {id}{mark}{}", crate::util::ellipsize(&description, 100)));
                    }
                }
            }
        }
        Ok(lines)
    }

    /// The marketplaces the harness knows: the settings', the built-in
    /// ones, and Codex's personal one.
    fn marketplace_names(&self, settings: &Settings) -> Vec<String> {
        let mut names: Vec<String> = settings.extra_known_marketplaces.keys().cloned().collect();
        for (name, _) in OFFICIAL {
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
        if let Some(name) = self.personal.and_then(|f| read_json(f).ok()).and_then(|c| c["name"].as_str().map(String::from))
            && !names.contains(&name)
        {
            names.push(name);
        }
        names
    }

    fn install(&self, id: &str, ask: &mut dyn Ask) -> Result<Vec<String>> {
        let settings = self.settings()?;
        let plugin = self.locate(id, &settings)?;
        let mut lines = vec![describe("installed", &plugin)];
        let (options, secrets) = ask_options(&plugin, &settings, self.home, ask);
        self.edit(|s| {
            settings::object(s, "enabledPlugins").insert(id.to_string(), true.into());
            save_options(s, id, options);
            Ok(())
        })?;
        write_secrets(self.home, id, secrets)?;
        let plugin = self.locate(id, &self.settings()?)?;
        lines.extend(plugin.warnings.iter().map(|w| format!("  ! {w}")));
        lines.extend(plugin.ignored.iter().map(|w| format!("  - not used: {w}")));
        lines.push(format!("enabled {id}; it loads at the next start (or after /reload-plugins)"));
        Ok(lines)
    }

    fn uninstall(&self, id: &str, keep_data: bool) -> Result<Vec<String>> {
        let (name, market) = split_id(id)?;
        let settings = self.settings()?;
        if !settings.enabled_plugins.contains_key(id) {
            bail!("{id} is not installed");
        }
        self.edit(|s| {
            settings::object(s, "enabledPlugins").remove(id);
            if let Some(Value::Object(configs)) = s.get_mut("pluginConfigs") {
                configs.remove(id);
            }
            Ok(())
        })?;
        write_secrets(self.home, id, Map::new())?;
        let mut lines = vec![format!("uninstalled {id}")];
        let cache = self.home.join("cache").join(market).join(sanitize(name));
        if cache.is_dir() {
            std::fs::remove_dir_all(&cache).with_context(|| format!("deleting {}", cache.display()))?;
        }
        let data = self.home.join("data").join(sanitize(id));
        if keep_data && data.is_dir() {
            lines.push(format!("kept its data in {}", data.display()));
        } else if data.is_dir() {
            std::fs::remove_dir_all(&data).with_context(|| format!("deleting {}", data.display()))?;
        }
        Ok(lines)
    }

    fn switch(&self, id: &str, on: bool) -> Result<Vec<String>> {
        split_id(id)?;
        if !self.settings()?.enabled_plugins.contains_key(id) {
            bail!("{id} is not installed; install it with `harness plugin install {id}`");
        }
        self.edit(|s| {
            settings::object(s, "enabledPlugins").insert(id.to_string(), on.into());
            Ok(())
        })?;
        Ok(vec![format!("{} {id}", if on { "enabled" } else { "disabled" })])
    }

    fn configure(&self, id: &str, ask: &mut dyn Ask) -> Result<Vec<String>> {
        let settings = self.settings()?;
        let plugin = self.locate(id, &settings)?;
        if plugin.options.is_empty() {
            return Ok(vec![format!("{id} has no settings")]);
        }
        let (options, secrets) = ask_options(&plugin, &settings, self.home, ask);
        self.edit(|s| {
            save_options(s, id, options);
            Ok(())
        })?;
        write_secrets(self.home, id, secrets)?;
        Ok(vec![format!("saved the settings of {id}")])
    }

    fn update(&self) -> Result<Vec<String>> {
        let settings = self.settings()?;
        let loaded = super::load_from(&settings, &[], self.home, self.personal);
        let mut lines: Vec<String> = loaded.plugins.update().into_iter().map(|r| r.unwrap_or_else(|e| e)).collect();
        if lines.is_empty() {
            lines.push("no installed plugin or marketplace is fetched from git or npm".into());
        }
        Ok(lines)
    }

    fn add_marketplace(&self, spec: &str, auto_update: bool) -> Result<Vec<String>> {
        let source = parse_source(spec)?;
        let settings = self.settings()?;
        // Fetched under a name of its own first: the catalog names it.
        let staging = format!(".adding-{}", std::process::id());
        let catalog = Catalog::read(&staging, "the marketplace", &source, self.home).map_err(anyhow::Error::msg)?;
        let staged = self.home.join("marketplaces").join(&staging);
        let name = read_json(&catalog.file).ok().and_then(|c| c["name"].as_str().map(String::from)).unwrap_or_default();
        let result = (|| {
            if !valid_marketplace_name(&name) {
                bail!("the marketplace calls itself `{name}`, which cannot be a marketplace name");
            }
            if OFFICIAL.iter().any(|(known, _)| *known == name) {
                bail!("`{name}` is a marketplace the harness knows already");
            }
            if let Some(known) = settings.extra_known_marketplaces.get(&name)
                && known.source != source
            {
                bail!("a marketplace named `{name}` is added already, from elsewhere; remove it first");
            }
            if catalog.checkout.is_some() {
                let target = self.home.join("marketplaces").join(&name);
                let _ = std::fs::remove_dir_all(&target);
                std::fs::rename(&staged, &target).with_context(|| format!("moving it to {}", target.display()))?;
            }
            let mut entry = json!({"source": source_json(&source)});
            if auto_update {
                entry["autoUpdate"] = true.into();
            }
            self.edit(|s| {
                settings::object(s, "extraKnownMarketplaces").insert(name.clone(), entry);
                Ok(())
            })
        })();
        let _ = std::fs::remove_dir_all(&staged);
        result?;
        let names: Vec<&str> = catalog.plugins.iter().filter_map(|p| p["name"].as_str()).collect();
        let mut lines = vec![format!("added marketplace {name} with {} plugins", names.len())];
        if !names.is_empty() {
            lines.push(format!("  {}", crate::util::ellipsize(&names.join(", "), 300)));
            lines.push(format!("install one with `harness plugin install <plugin>@{name}`"));
        }
        Ok(lines)
    }

    fn remove_marketplace(&self, name: &str) -> Result<Vec<String>> {
        let settings = self.settings()?;
        if !settings.extra_known_marketplaces.contains_key(name) {
            bail!("no marketplace `{name}` in the settings");
        }
        let suffix = format!("@{name}");
        let installed: Vec<String> = settings.enabled_plugins.keys().filter(|id| id.ends_with(&suffix)).cloned().collect();
        self.edit(|s| {
            settings::object(s, "extraKnownMarketplaces").remove(name);
            let enabled = settings::object(s, "enabledPlugins");
            for id in &installed {
                enabled.remove(id);
            }
            if let Some(Value::Object(configs)) = s.get_mut("pluginConfigs") {
                configs.retain(|id, _| !id.ends_with(&suffix));
            }
            Ok(())
        })?;
        for id in &installed {
            write_secrets(self.home, id, Map::new())?;
        }
        for dir in [self.home.join("marketplaces").join(name), self.home.join("cache").join(name)] {
            let _ = std::fs::remove_dir_all(dir);
        }
        let mut lines = vec![format!("removed marketplace {name}")];
        if !installed.is_empty() {
            lines.push(format!("and the plugins installed from it: {}", installed.join(", ")));
        }
        Ok(lines)
    }

    fn list_marketplaces(&self) -> Result<Vec<String>> {
        let settings = self.settings()?;
        let mut lines = Vec::new();
        for (name, market) in &settings.extra_known_marketplaces {
            let auto = if market.auto_update { ", updated automatically" } else { "" };
            lines.push(format!("{name}  {}{auto}", describe_source(&market.source)));
        }
        for (name, repo) in OFFICIAL.iter().filter(|(n, _)| !settings.extra_known_marketplaces.contains_key(*n)) {
            lines.push(format!("{name}  github {repo} (built in)"));
        }
        if let Some(file) = self.personal
            && let Ok(catalog) = read_json(file)
            && let Some(name) = catalog["name"].as_str()
        {
            lines.push(format!("{name}  {} (Codex's personal marketplace)", file.display()));
        }
        Ok(lines)
    }
}

/// `<plugin>@<marketplace>`, split.
fn split_id(id: &str) -> Result<(&str, &str)> {
    match id.rsplit_once('@') {
        Some((name, market)) if !name.is_empty() && valid_marketplace_name(market) => Ok((name, market)),
        _ => bail!("expected `<plugin>@<marketplace>`, not `{id}`"),
    }
}

/// A line saying what `plugin` is and adds.
fn describe(what: &str, plugin: &Plugin) -> String {
    let version = plugin.version.as_ref().map(|v| format!(" v{v}")).unwrap_or_default();
    let parts = match &plugin.outcome {
        Ok(parts) if parts.is_empty() => "nothing the harness can use".to_string(),
        Ok(parts) => parts.join(", "),
        Err(e) => e.clone(),
    };
    format!("{what} {}{version}: {parts}", plugin.id)
}

/// A marketplace source as `marketplace add` takes it: `owner/repo`, a git
/// URL, a directory or a catalog file; `#ref` after a repository picks a
/// branch or tag.
fn parse_source(spec: &str) -> Result<MarketplaceSource> {
    let path = super::expand_home(spec);
    if path.is_dir() {
        let path = std::path::absolute(&path)?;
        return Ok(MarketplaceSource::Directory { path: path.display().to_string() });
    }
    if path.is_file() {
        let path = std::path::absolute(&path)?;
        return Ok(MarketplaceSource::File { path: path.display().to_string() });
    }
    let (repo, git_ref) = match spec.split_once('#') {
        Some((repo, git_ref)) if !git_ref.is_empty() => (repo, Some(git_ref.to_string())),
        _ => (spec, None),
    };
    let url_like = ["https://", "http://", "ssh://", "git://", "file://", "git@"].iter().any(|p| repo.starts_with(p));
    if url_like || repo.ends_with(".git") {
        return Ok(MarketplaceSource::Git { url: repo.to_string(), git_ref, path: None });
    }
    let shorthand = repo.split('/').count() == 2
        && repo.split('/').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)));
    if shorthand {
        return Ok(MarketplaceSource::Github { repo: repo.to_string(), git_ref, path: None });
    }
    bail!("`{spec}` is no directory, marketplace.json, git URL or GitHub owner/repo")
}

/// A source as the settings hold it.
fn source_json(source: &MarketplaceSource) -> Value {
    let mut value = match source {
        MarketplaceSource::Github { repo, git_ref, path } => json!({"source": "github", "repo": repo, "ref": git_ref, "path": path}),
        MarketplaceSource::Git { url, git_ref, path } => json!({"source": "git", "url": url, "ref": git_ref, "path": path}),
        MarketplaceSource::Directory { path } => json!({"source": "directory", "path": path}),
        MarketplaceSource::File { path } => json!({"source": "file", "path": path}),
    };
    if let Value::Object(map) = &mut value {
        map.retain(|_, v| !v.is_null());
    }
    value
}

fn describe_source(source: &MarketplaceSource) -> String {
    let at = |git_ref: &Option<String>| git_ref.as_ref().map(|r| format!("#{r}")).unwrap_or_default();
    match source {
        MarketplaceSource::Github { repo, git_ref, .. } => format!("github {repo}{}", at(git_ref)),
        MarketplaceSource::Git { url, git_ref, .. } => format!("git {url}{}", at(git_ref)),
        MarketplaceSource::Directory { path } => format!("directory {path}"),
        MarketplaceSource::File { path } => format!("file {path}"),
    }
}

/// Asks for each of `plugin`'s settings, offering what is set now (or the
/// default); returns the answers for the settings file and the sensitive
/// ones, each as they all are now.
fn ask_options(plugin: &Plugin, settings: &Settings, home: &Path, ask: &mut dyn Ask) -> (Map<String, Value>, Map<String, Value>) {
    let mut options = settings.plugin_configs.get(&plugin.id).map(|c| c.options.clone()).unwrap_or_default();
    let mut secrets = read_secrets(home).get(&plugin.id).and_then(Value::as_object).cloned().unwrap_or_default();
    if !plugin.options.is_empty() {
        eprintln!("{} asks for {} setting{}:", plugin.id, plugin.options.len(), if plugin.options.len() == 1 { "" } else { "s" });
    }
    for option in &plugin.options {
        let store = if option.sensitive { &mut secrets } else { &mut options };
        let current = store.get(&option.key).cloned().or_else(|| option.default.clone());
        let Some(value) = ask_option(option, current.as_ref(), ask) else { break };
        if let Some(value) = value {
            store.insert(option.key.clone(), value);
        }
    }
    (options, secrets)
}

/// Asks for one setting: `Some(None)` keeps what it is, `None` if nobody
/// answers.
fn ask_option(option: &OptionSpec, current: Option<&Value>, ask: &mut dyn Ask) -> Option<Option<Value>> {
    let title = option.title.as_deref().unwrap_or(&option.key);
    if let Some(description) = &option.description {
        eprintln!("  {description}");
    }
    let shown = match current {
        Some(_) if option.sensitive => " [set; Enter keeps it]".to_string(),
        Some(value) => format!(" [{}; Enter keeps it]", super::option_text(value)),
        None => String::new(),
    };
    for _ in 0..3 {
        let answer = ask.ask(&format!("  {title} ({}){shown}: ", option.kind), option.sensitive)?;
        let answer = answer.trim();
        if answer.is_empty() {
            return Some(None);
        }
        let value = match option.kind.as_str() {
            "number" => answer.parse::<f64>().ok().map(|n| match n.fract() == 0.0 && n.abs() < 1e15 {
                true => json!(n as i64),
                false => json!(n),
            }),
            "boolean" => match answer.to_ascii_lowercase().as_str() {
                "y" | "yes" | "true" | "1" | "on" => Some(json!(true)),
                "n" | "no" | "false" | "0" | "off" => Some(json!(false)),
                _ => None,
            },
            "directory" | "file" => Some(json!(super::expand_home(answer).display().to_string())),
            _ => Some(json!(answer)),
        };
        match value {
            Some(value) => return Some(Some(value)),
            None => eprintln!("  that is not a {}", option.kind),
        }
    }
    Some(None)
}

/// Sets plugin `id`'s `options` in the settings (as JSON).
fn save_options(settings: &mut Map<String, Value>, id: &str, options: Map<String, Value>) {
    let configs = settings::object(settings, "pluginConfigs");
    if options.is_empty() {
        configs.remove(id);
    } else {
        configs.insert(id.to_string(), json!({"options": options}));
    }
    if configs.is_empty() {
        settings.remove("pluginConfigs");
    }
}

/// Sets plugin `id`'s sensitive answers; none removes them.
fn write_secrets(home: &Path, id: &str, answers: Map<String, Value>) -> Result<()> {
    let mut secrets = read_secrets(home);
    if answers.is_empty() && !secrets.contains_key(id) {
        return Ok(());
    }
    if answers.is_empty() {
        secrets.remove(id);
    } else {
        secrets.insert(id.to_string(), Value::Object(answers));
    }
    let file: PathBuf = secrets_file(home);
    std::fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
    let text = serde_json::to_string_pretty(&Value::Object(secrets))? + "\n";
    settings::write_private(&file, &text).with_context(|| format!("writing {}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::fetch::tests::repo;

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

    #[test]
    fn installs_configures_and_removes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let file = tmp.path().join("settings.json");
        std::fs::write(&file, r#"{"model": "gw/m"}"#).unwrap();
        let market = tmp.path().join("market");
        repo(&market, &[
            (
                ".claude-plugin/marketplace.json",
                r#"{"name": "team", "plugins": [{"name": "fmt", "source": "./fmt", "description": "Formats"}]}"#,
            ),
            (
                "fmt/.claude-plugin/plugin.json",
                r#"{"name": "fmt", "version": "1.0.0", "userConfig": {
                    "width": {"type": "number", "title": "Line width", "default": 80},
                    "token": {"type": "string", "sensitive": true},
                    "style": {"type": "string", "required": false}},
                    "monitors": "./monitors.json"}"#,
            ),
            ("fmt/commands/go.md", "Format with ${user_config.width} and ${user_config.token}\n"),
        ]);
        let m = Manager { home: &home, personal: None, settings_file: Some(&file) };
        let none = &mut Answers(Vec::new(), Vec::new());

        let url = format!("file://{}", market.display());
        let lines = run(&m, &format!("marketplace add {url}"), none).unwrap();
        assert_eq!(lines[0], "added marketplace team with 1 plugins");
        assert!(home.join("marketplaces/team/current").is_file());
        assert!(std::fs::read_dir(home.join("marketplaces")).unwrap().count() == 1, "no staging copy left");
        assert!(run(&m, &format!("marketplace add {url}"), none).is_ok(), "adding it again is fine");
        assert!(run(&m, "marketplace list", none).unwrap()[0].starts_with("team  git file://"));

        let ask = &mut Answers(vec!["100", "s3cret"], Vec::new());
        let lines = run(&m, "install fmt@team", ask).unwrap();
        assert_eq!(lines[0], "installed fmt@team v1.0.0: 1 command");
        assert!(lines.iter().any(|l| l.contains("not used: monitors")), "{lines:?}");
        assert!(ask.1[0].contains("Line width (number) [80; Enter keeps it]"), "{:?}", ask.1);
        let settings = Settings::parse(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(settings.model.as_deref(), Some("gw/m"));
        assert!(settings.enabled_plugins["fmt@team"]);
        assert_eq!(settings.plugin_configs["fmt@team"].options["width"], 100);
        assert!(!std::fs::read_to_string(&file).unwrap().contains("s3cret"), "secrets stay out of the settings");
        assert_eq!(read_secrets(&home)["fmt@team"]["token"], "s3cret");

        // Loaded, the command gets the answers.
        let loaded = super::super::load_from(&settings, &[], &home, None);
        let plugin = &loaded.plugins.list[0];
        assert!(plugin.unconfigured.is_empty() && plugin.warnings.is_empty(), "{plugin:?}");
        let command = &loaded.skills[0];
        let dirs = command.plugin.as_ref().unwrap();
        assert_eq!(dirs.expand("${user_config.width} ${user_config.token}"), "100 s3cret");
        assert_eq!(dirs.expand_prompt("${user_config.width} ${user_config.token}"), "100 ${user_config.token}");
        assert!(dirs.env().contains(&("CLAUDE_PLUGIN_OPTION_TOKEN".into(), "s3cret".into())));

        assert_eq!(run(&m, "disable fmt@team", none).unwrap(), ["disabled fmt@team"]);
        assert!(run(&m, "list", none).unwrap()[0].contains("○ fmt@team  disabled"));
        assert!(run(&m, "enable nope@team", none).is_err());
        assert!(run(&m, "install nope@team", none).unwrap_err().to_string().contains("no plugin `nope`"));

        let lines = run(&m, "marketplace remove team", none).unwrap();
        assert_eq!(lines, ["removed marketplace team", "and the plugins installed from it: fmt@team"]);
        let settings = Settings::parse(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert!(settings.enabled_plugins.is_empty() && settings.extra_known_marketplaces.is_empty());
        assert!(settings.plugin_configs.is_empty());
        assert!(read_secrets(&home).is_empty());
        assert!(!home.join("marketplaces/team").exists());
    }

    #[test]
    fn missing_settings_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("p");
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::write(
            root.join(".claude-plugin/plugin.json"),
            r#"{"name": "p", "userConfig": {"key": {"type": "string", "sensitive": true}}, "outputStyles": "./styles"}"#,
        )
        .unwrap();
        let loaded = super::super::load_from(&Settings::default(), &[root], &tmp.path().join("home"), None);
        let plugin = &loaded.plugins.list[0];
        assert_eq!(plugin.unconfigured, ["key"]);
        assert_eq!(plugin.ignored, ["output styles"]);
        assert!(loaded.warnings[0].contains("run `harness plugin configure p@inline`"), "{:?}", loaded.warnings);
    }

    #[test]
    fn sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().display().to_string();
        assert_eq!(parse_source(&path).unwrap(), MarketplaceSource::Directory { path: path.clone() });
        let github = |repo: &str, git_ref: Option<&str>| MarketplaceSource::Github {
            repo: repo.into(),
            git_ref: git_ref.map(String::from),
            path: None,
        };
        assert_eq!(parse_source("org/plugins").unwrap(), github("org/plugins", None));
        assert_eq!(parse_source("org/plugins#v2").unwrap(), github("org/plugins", Some("v2")));
        let git = parse_source("https://git.example.com/x/y.git#main").unwrap();
        assert_eq!(git, MarketplaceSource::Git { url: "https://git.example.com/x/y.git".into(), git_ref: Some("main".into()), path: None });
        assert!(parse_source("not a source").is_err());
        assert_eq!(source_json(&github("o/r", None)), json!({"source": "github", "repo": "o/r"}));
        assert!(split_id("a@b").is_ok() && split_id("ab").is_err() && split_id("@b").is_err());
    }
}
