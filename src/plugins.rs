//! Plugins in Claude Code's format: a directory bundling skills, commands,
//! agents, hooks and MCP servers, usually installed from a marketplace (a
//! repository listing plugins in `.claude-plugin/marketplace.json`).
//!
//! The settings enable plugins by id, under Claude Code's keys:
//!
//! ```json
//! "enabledPlugins": { "commit-commands@claude-plugins-official": true, "fmt@team": true },
//! "extraKnownMarketplaces": { "team": { "source": { "source": "github", "repo": "org/plugins" } } }
//! ```
//!
//! Marketplaces and plugins in git repositories are cloned into
//! `~/.harness/plugins` the first time they are needed, and only move on
//! with `/plugins update`. A plugin's components are named after it, as in
//! Claude Code: its skill or command `review` is `/<plugin>:review`, its
//! agents `<plugin>:<name>`, its MCP servers `plugin:<plugin>:<server>`
//! (tools `mcp__plugin_<plugin>_<server>__<tool>`), and its hooks run with
//! the user's. `--plugin-dir DIR` loads a plugin from a directory for one
//! run.
//!
//! Only the settings and the command line enable plugins, never the
//! project: a plugin's hooks and MCP servers run commands.

mod fetch;

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::hooks;
use crate::mcp::ServerConfig;
use crate::settings::Settings;
use crate::skills::{self, PluginDirs, Skill};
use crate::subagent::{self, AgentDef};
pub use fetch::Checkout;

/// The marketplace Claude Code knows without being told; so does the
/// harness.
const OFFICIAL: (&str, &str) = ("claude-plugins-official", "anthropics/claude-plugins-official");

/// A marketplace under `extraKnownMarketplaces`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Marketplace {
    source: MarketplaceSource,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase", deny_unknown_fields)]
enum MarketplaceSource {
    /// A GitHub repository, `owner/repo`.
    Github {
        repo: String,
        #[serde(rename = "ref")]
        git_ref: Option<String>,
        /// The marketplace file in it, if not `.claude-plugin/marketplace.json`.
        path: Option<String>,
    },
    /// Any git repository.
    Git {
        url: String,
        #[serde(rename = "ref")]
        git_ref: Option<String>,
        path: Option<String>,
    },
    /// A directory holding `.claude-plugin/marketplace.json`, read in place.
    Directory { path: String },
    /// A `marketplace.json`, read in place.
    File { path: String },
}

/// An enabled plugin, as `/plugins` shows it.
#[derive(Debug, Clone)]
pub struct Plugin {
    /// `<plugin>@<marketplace>`, or `<plugin>@inline` for `--plugin-dir`.
    pub id: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub root: Option<PathBuf>,
    /// What it adds (`2 skills`, `hooks`, …), or why it was not loaded.
    pub outcome: Result<Vec<String>, String>,
}

/// The enabled plugins, and the git checkouts they came from.
#[derive(Debug, Clone, Default)]
pub struct Plugins {
    pub list: Vec<Plugin>,
    checkouts: Vec<Checkout>,
}

impl Plugins {
    /// Moves every marketplace and plugin cloned from git to the latest
    /// commit of its branch: a line on each. They apply at the next start.
    pub fn update(&self) -> Vec<Result<String, String>> {
        self.checkouts.iter().map(|c| c.update().map_err(|e| format!("{}: {e}", c.label))).collect()
    }

    /// The list as the web UI shows it.
    pub fn to_json(&self) -> Value {
        let list = self.list.iter().map(|p| {
            json!({
                "id": p.id,
                "description": p.description,
                "version": p.version,
                "root": p.root.as_ref().map(|r| r.display().to_string()),
                "parts": p.outcome.as_ref().ok(),
                "error": p.outcome.as_ref().err(),
            })
        });
        Value::Array(list.collect())
    }
}

/// What the enabled plugins add, to go with the user's own.
#[derive(Debug, Default)]
pub struct Loaded {
    pub plugins: Plugins,
    pub skills: Vec<Skill>,
    pub agents: Vec<AgentDef>,
    pub hooks: hooks::Config,
    pub mcp_servers: BTreeMap<String, ServerConfig>,
    /// Their `bin/` directories, for the model's commands' `PATH`.
    pub bin_dirs: Vec<PathBuf>,
    /// The parts of plugins that were left out.
    pub warnings: Vec<String>,
}

/// Takes `--plugin-dir DIR` (any number of them) out of the command line.
pub fn parse_flag(args: Vec<String>) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let mut dirs = Vec::new();
    let mut rest = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--plugin-dir" {
            let Some(dir) = args.next() else { bail!("--plugin-dir needs a directory") };
            dirs.push(PathBuf::from(dir));
        } else if let Some(dir) = arg.strip_prefix("--plugin-dir=") {
            dirs.push(PathBuf::from(dir));
        } else {
            rest.push(arg);
        }
    }
    Ok((dirs, rest))
}

/// `~/.harness/plugins`: the clones, and the plugins' data directories.
fn home() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => Path::new(&home).join(".harness/plugins"),
        None => std::env::temp_dir().join("harness-plugins"),
    }
}

/// Loads the plugins the settings enable, fetching what is missing, and
/// those in `dirs`. A plugin that cannot be loaded is listed with its
/// error; one that has parts the harness cannot use is loaded without them.
pub fn load(settings: &Settings, dirs: &[PathBuf]) -> Loaded {
    load_from(settings, dirs, &home())
}

fn load_from(settings: &Settings, dirs: &[PathBuf], home: &Path) -> Loaded {
    let mut loaded = Loaded::default();
    let mut catalogs: BTreeMap<String, Result<Catalog, String>> = BTreeMap::new();
    for id in settings.enabled_plugins.iter().filter(|(_, on)| **on).map(|(id, _)| id) {
        let found = match id.rsplit_once('@') {
            None => Err("expected `<plugin>@<marketplace>`".to_string()),
            Some((name, market)) => {
                let catalog = catalogs.entry(market.to_string()).or_insert_with(|| {
                    let catalog = Catalog::open(market, settings, home);
                    if let Ok(catalog) = &catalog {
                        loaded.plugins.checkouts.extend(catalog.checkout.clone());
                    }
                    catalog
                });
                match catalog {
                    Ok(catalog) => catalog.locate(name, home),
                    Err(e) => Err(e.clone()),
                }
            }
        };
        let result = found.and_then(|(root, entry, checkout)| {
            loaded.plugins.checkouts.extend(checkout);
            loaded.add(Some(id), &root, Some(&entry), home)
        });
        if let Err(e) = result {
            loaded.plugins.list.push(Plugin { id: id.clone(), description: None, version: None, root: None, outcome: Err(e) });
        }
    }
    for dir in dirs {
        if let Err(e) = loaded.add(None, dir, None, home) {
            let id = format!("{}@inline", dir.display());
            loaded.plugins.list.push(Plugin { id, description: None, version: None, root: None, outcome: Err(e) });
        }
    }
    loaded
}

/// A marketplace's `marketplace.json`.
struct Catalog {
    name: String,
    /// The directory holding `.claude-plugin/`, which relative plugin
    /// sources start from.
    root: PathBuf,
    plugins: Vec<Value>,
    /// `metadata.pluginRoot`: where plugins named by a bare name are.
    plugin_root: Option<String>,
    checkout: Option<Checkout>,
}

/// A plugin source that is a git repository: `github`, `url` or
/// `git-subdir`.
struct GitSource {
    url: String,
    git_ref: Option<String>,
    sha: Option<String>,
    /// The plugin's directory in it, for `git-subdir`.
    path: Option<String>,
}

impl Catalog {
    fn open(name: &str, settings: &Settings, home: &Path) -> Result<Self, String> {
        if name.is_empty() || name.starts_with('.') || !name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
            return Err(format!("invalid marketplace name `{name}`"));
        }
        let source = match settings.extra_known_marketplaces.get(name) {
            Some(market) => market.source.clone(),
            None if name == OFFICIAL.0 => MarketplaceSource::Github { repo: OFFICIAL.1.into(), git_ref: None, path: None },
            None => return Err(format!("unknown marketplace `{name}`: declare it under `extraKnownMarketplaces`")),
        };
        let clone = |url: String, git_ref: &Option<String>, path: &Option<String>| {
            let checkout = Checkout {
                label: format!("marketplace {name}"),
                dir: home.join("marketplaces").join(name),
                url,
                git_ref: git_ref.clone(),
                sha: None,
            };
            checkout.ensure()?;
            let file = inside(&checkout.dir, path.as_deref().unwrap_or(".claude-plugin/marketplace.json"))?;
            Ok::<_, String>((file, Some(checkout)))
        };
        let (file, checkout) = match &source {
            MarketplaceSource::Github { repo, git_ref, path } => clone(github_url(repo), git_ref, path)?,
            MarketplaceSource::Git { url, git_ref, path } => clone(url.clone(), git_ref, path)?,
            MarketplaceSource::Directory { path } => (expand_home(path).join(".claude-plugin/marketplace.json"), None),
            MarketplaceSource::File { path } => (expand_home(path), None),
        };
        let catalog = read_json(&file)?;
        let dir = file.parent().unwrap_or(Path::new("."));
        let root = if dir.ends_with(".claude-plugin") { dir.parent().unwrap_or(dir) } else { dir };
        Ok(Self {
            name: name.to_string(),
            root: std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf()),
            plugins: catalog["plugins"].as_array().cloned().unwrap_or_default(),
            plugin_root: catalog["metadata"]["pluginRoot"].as_str().map(String::from),
            checkout,
        })
    }

    /// Plugin `name`: its directory, its entry, and the checkout it is in
    /// unless that is the marketplace's.
    fn locate(&self, name: &str, home: &Path) -> Result<(PathBuf, Value, Option<Checkout>), String> {
        let entry = self
            .plugins
            .iter()
            .find(|p| p["name"] == name)
            .ok_or_else(|| format!("marketplace `{}` has no plugin `{name}`", self.name))?;
        let (root, checkout) = match &entry["source"] {
            Value::String(path) => {
                let bare = !path.contains('/') && path != ".";
                let path = match &self.plugin_root {
                    Some(plugin_root) if bare => format!("{}/{path}", plugin_root.trim_end_matches('/')),
                    _ => path.clone(),
                };
                (inside(&self.root, &path)?, None)
            }
            Value::Object(source) => {
                let source = git_source(source)?;
                let checkout = Checkout {
                    label: format!("plugin {name}@{}", self.name),
                    dir: home.join("cache").join(&self.name).join(sanitize(name)),
                    url: source.url,
                    git_ref: source.git_ref,
                    sha: source.sha,
                };
                checkout.ensure()?;
                let root = match &source.path {
                    Some(path) => inside(&checkout.dir, path)?,
                    None => checkout.dir.clone(),
                };
                (root, Some(checkout))
            }
            _ => return Err("its marketplace entry has no valid `source`".into()),
        };
        if !root.is_dir() {
            return Err(format!("source path does not exist: {}", root.display()));
        }
        Ok((root, entry.clone(), checkout))
    }
}

fn git_source(source: &Map<String, Value>) -> Result<GitSource, String> {
    let field = |key: &str| source.get(key).and_then(Value::as_str).map(String::from);
    let required = |key: &str| field(key).ok_or_else(|| format!("plugin source needs `{key}`"));
    let (url, path) = match field("source").as_deref() {
        Some("github") => (github_url(&required("repo")?), None),
        Some("url") => (required("url")?, None),
        Some("git-subdir") => {
            let url = required("url")?;
            // `owner/repo` is GitHub's.
            let shorthand = !url.contains(':') && url.matches('/').count() == 1;
            (if shorthand { github_url(&url) } else { url }, Some(required("path")?))
        }
        Some(other) => {
            return Err(format!(
                "plugin source `{other}` is not supported (only relative paths, `github`, `url` and `git-subdir`)"
            ));
        }
        None => return Err("plugin source has no `source` type".into()),
    };
    Ok(GitSource { url, git_ref: field("ref"), sha: field("sha"), path })
}

fn github_url(repo: &str) -> String {
    format!("https://github.com/{repo}.git")
}

impl Loaded {
    /// Loads the plugin in `root`, with its marketplace `entry` (which
    /// stands in for a missing `plugin.json`), as `id` (else
    /// `<name>@inline`).
    fn add(&mut self, id: Option<&str>, root: &Path, entry: Option<&Value>, home: &Path) -> Result<(), String> {
        let root = std::path::absolute(root).map_err(|e| e.to_string())?;
        if !root.is_dir() {
            return Err(format!("{} is not a directory", root.display()));
        }
        let manifest_file = root.join(".claude-plugin/plugin.json");
        let manifest = match (manifest_file.is_file(), entry) {
            (true, entry) => {
                let mut manifest = read_json(&manifest_file)?;
                // A marketplace entry may add skills to the plugin's own.
                if let Some(extra) = entry.filter(|e| e["strict"] != false).map(|e| paths(&e["skills"]))
                    && !extra.is_empty()
                {
                    let mut skills = paths(&manifest["skills"]);
                    skills.extend(extra);
                    manifest["skills"] = Value::from(skills);
                }
                manifest
            }
            (false, Some(entry)) => entry.clone(),
            (false, None) => Value::Object(Map::new()),
        };
        let dir_name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let name = manifest["name"].as_str().map_or(dir_name, String::from);
        if name.is_empty() || name.chars().any(|c| c == ':' || c == '@' || c == '/' || c.is_whitespace()) {
            return Err(format!("invalid plugin name `{name}`"));
        }
        let id = id.map_or_else(|| format!("{name}@inline"), String::from);
        let data = home.join("data").join(sanitize(&id));
        let _ = std::fs::create_dir_all(&data);
        let dirs = PluginDirs { root: root.clone(), data };

        let mut warnings = Vec::new();
        let mut parts = Vec::new();
        let mut count = |n: usize, what: &str| {
            if n > 0 {
                parts.push(format!("{n} {what}{}", if n == 1 { "" } else { "s" }));
            }
        };
        let skills = self.add_skills(&manifest, &name, &dirs, &mut warnings);
        count(skills, "skill");
        let commands = self.add_commands(&manifest, &name, &dirs, &mut warnings);
        count(commands, "command");
        let agents = self.add_agents(&manifest, &name, &dirs, &mut warnings);
        count(agents, "agent");
        let servers = self.add_mcp_servers(&manifest, &name, &dirs, &mut warnings);
        count(servers, "MCP server");
        let events = self.add_hooks(&manifest, &dirs, &mut warnings);
        if !events.is_empty() {
            parts.push(format!("hooks ({})", events.join(", ")));
        }
        if root.join("bin").is_dir() {
            self.bin_dirs.push(root.join("bin"));
            parts.push("bin/".into());
        }
        self.warnings.extend(warnings.into_iter().map(|w| format!("plugin {id}: {w}")));
        self.plugins.list.push(Plugin {
            id,
            description: manifest["description"].as_str().map(String::from),
            version: manifest["version"].as_str().map(String::from),
            root: Some(root),
            outcome: Ok(parts),
        });
        Ok(())
    }

    /// `skills/` and the manifest's `skills` directories, which add to it;
    /// without either, a `SKILL.md` in the plugin's root.
    fn add_skills(&mut self, manifest: &Value, name: &str, dirs: &PluginDirs, warnings: &mut Vec<String>) -> usize {
        let root = &dirs.root;
        let mut roots = vec![root.join("skills")];
        for path in paths(&manifest["skills"]) {
            match inside(root, &path) {
                Ok(dir) => roots.push(dir),
                Err(e) => warnings.push(e),
            }
        }
        if roots.len() == 1 && !roots[0].is_dir() && root.join("SKILL.md").is_file() {
            roots = vec![root.clone()];
        }
        let before = self.skills.len();
        for dir in roots.iter().filter(|d| d.is_dir()) {
            let (skills, skipped) = skills::discover_in_plugin(dir, name, dirs);
            warnings.extend(skipped);
            for skill in skills {
                if !self.skills.iter().any(|s| s.name == skill.name) {
                    self.skills.push(skill);
                }
            }
        }
        self.skills.len() - before
    }

    /// `commands/`, or what the manifest's `commands` names instead: files,
    /// directories, or a map from name to a `source` file or `content`.
    fn add_commands(&mut self, manifest: &Value, name: &str, dirs: &PluginDirs, warnings: &mut Vec<String>) -> usize {
        let mut commands = Vec::new();
        match &manifest["commands"] {
            Value::Null => scan_commands(&dirs.root.join("commands"), name, dirs, &mut commands, warnings),
            Value::Object(map) => {
                for (command, spec) in map {
                    let full = format!("{name}:{command}");
                    let description = spec["description"].as_str();
                    let made = match (spec["source"].as_str(), spec["content"].as_str()) {
                        (Some(source), None) => inside(&dirs.root, source).and_then(|file| {
                            let mut skill = Skill::command(full, &file, Some(dirs.clone()))?;
                            if let Some(description) = description {
                                skill.description = description.to_string();
                            }
                            Ok(skill)
                        }),
                        (None, Some(content)) => Ok(Skill::inline(full, content, description, Some(dirs.clone()))),
                        _ => Err("needs one of `source` and `content`".into()),
                    };
                    match made {
                        Ok(skill) => commands.push(skill),
                        Err(e) => warnings.push(format!("command `{command}`: {e}")),
                    }
                }
            }
            listed => {
                for path in paths(listed) {
                    match inside(&dirs.root, &path) {
                        Ok(dir) if dir.is_dir() => scan_commands(&dir, name, dirs, &mut commands, warnings),
                        Ok(file) => match Skill::command(format!("{name}:{}", stem(&file)), &file, Some(dirs.clone())) {
                            Ok(skill) => commands.push(skill),
                            Err(e) => warnings.push(format!("{}: {e}", file.display())),
                        },
                        Err(e) => warnings.push(e),
                    }
                }
            }
        }
        let before = self.skills.len();
        for command in commands {
            if !self.skills.iter().any(|s| s.name == command.name) {
                self.skills.push(command);
            }
        }
        self.skills.len() - before
    }

    /// `agents/` (its subdirectories name the agents too), or the files the
    /// manifest's `agents` lists instead.
    fn add_agents(&mut self, manifest: &Value, name: &str, dirs: &PluginDirs, warnings: &mut Vec<String>) -> usize {
        let mut files = Vec::new();
        match &manifest["agents"] {
            Value::Null => markdown_files(&dirs.root.join("agents"), name, &mut files),
            listed => {
                for path in paths(listed) {
                    match inside(&dirs.root, &path) {
                        Ok(file) => files.push((file, name.to_string())),
                        Err(e) => warnings.push(e),
                    }
                }
            }
        }
        let before = self.agents.len();
        for (file, prefix) in files {
            match subagent::plugin_def(&file, &prefix, dirs) {
                Ok(def) if !self.agents.iter().any(|a| a.name == def.name) => self.agents.push(def),
                Ok(_) => {}
                Err(e) => warnings.push(format!("{}: {e}", file.display())),
            }
        }
        self.agents.len() - before
    }

    /// `hooks/hooks.json` and what the manifest's `hooks` adds: files
    /// (`{"hooks": {...}}`) or the events inline. Returns the events.
    fn add_hooks(&mut self, manifest: &Value, dirs: &PluginDirs, warnings: &mut Vec<String>) -> Vec<String> {
        let default = dirs.root.join("hooks/hooks.json");
        let mut files = vec![default.clone()];
        let mut sets = Vec::new();
        if default.is_file() {
            match read_json(&default) {
                Ok(file) => sets.push(file["hooks"].clone()),
                Err(e) => warnings.push(e),
            }
        }
        for item in items(&manifest["hooks"]) {
            match item {
                Value::String(path) => match inside(&dirs.root, path) {
                    Ok(file) if files.contains(&file) => {}
                    Ok(file) => {
                        match read_json(&file) {
                            Ok(file) => sets.push(file["hooks"].clone()),
                            Err(e) => warnings.push(e),
                        }
                        files.push(file);
                    }
                    Err(e) => warnings.push(e),
                },
                events => sets.push(events.clone()),
            }
        }
        let mut events = Vec::new();
        for set in sets {
            for (event, groups) in hooks::plugin_config(&set, &dirs.env(), warnings) {
                if !events.contains(&event.as_str().to_string()) {
                    events.push(event.as_str().to_string());
                }
                self.hooks.entry(event).or_default().extend(groups);
            }
        }
        events
    }

    /// `.mcp.json` and what the manifest's `mcpServers` adds (files or the
    /// servers inline; a later server replaces one of the same name), as
    /// `plugin:<plugin>:<server>`.
    fn add_mcp_servers(&mut self, manifest: &Value, name: &str, dirs: &PluginDirs, warnings: &mut Vec<String>) -> usize {
        let mut servers = Map::new();
        let default = dirs.root.join(".mcp.json");
        if default.is_file() {
            match read_json(&default) {
                Ok(file) => servers.extend(server_map(file)),
                Err(e) => warnings.push(e),
            }
        }
        for item in items(&manifest["mcpServers"]) {
            match item {
                Value::String(path) if path.ends_with(".mcpb") || path.ends_with(".dxt") => {
                    warnings.push(format!("MCP bundle {path} is not supported; left out"));
                }
                Value::String(path) => match inside(&dirs.root, path).and_then(|file| read_json(&file)) {
                    Ok(file) => servers.extend(server_map(file)),
                    Err(e) => warnings.push(e),
                },
                inline => servers.extend(server_map(inline.clone())),
            }
        }
        let count = servers.len();
        for (server, mut config) in servers {
            expand_strings(&mut config, dirs);
            if config.get("command").is_some() {
                let env = config.as_object_mut().unwrap().entry("env").or_insert_with(|| Value::Object(Map::new()));
                if let Some(env) = env.as_object_mut() {
                    for (key, value) in dirs.env() {
                        env.entry(key).or_insert(Value::String(value));
                    }
                }
            }
            match serde_json::from_value::<ServerConfig>(config) {
                Ok(config) => {
                    self.mcp_servers.insert(format!("plugin:{name}:{server}"), config);
                }
                Err(e) => warnings.push(format!("MCP server `{server}`: {e}; left out")),
            }
        }
        count
    }
}

/// Commands in `dir` and below: `<prefix>:<subdirectory>:<file name>`.
fn scan_commands(dir: &Path, prefix: &str, plugin: &PluginDirs, out: &mut Vec<Skill>, warnings: &mut Vec<String>) {
    let mut files = Vec::new();
    markdown_files(dir, prefix, &mut files);
    for (file, prefix) in files {
        match Skill::command(format!("{prefix}:{}", stem(&file)), &file, Some(plugin.clone())) {
            Ok(skill) => out.push(skill),
            Err(e) => warnings.push(format!("{}: {e}", file.display())),
        }
    }
}

/// The Markdown files in `dir` and below, sorted, each with `prefix` and
/// the subdirectories it is in joined by `:`.
fn markdown_files(dir: &Path, prefix: &str, out: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            markdown_files(&path, &format!("{prefix}:{}", stem(&path)), out);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push((path, prefix.to_string()));
        }
    }
}

fn stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// A manifest value that is one item or an array of them.
fn items(value: &Value) -> Vec<&Value> {
    match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items.iter().collect(),
        item => vec![item],
    }
}

/// A manifest value that is one path or an array of them.
fn paths(value: &Value) -> Vec<String> {
    items(value).into_iter().filter_map(Value::as_str).map(String::from).collect()
}

/// The servers in an MCP config: its `mcpServers`, or the file itself
/// without that wrapper.
fn server_map(config: Value) -> Map<String, Value> {
    match config {
        Value::Object(mut map) => match map.remove("mcpServers") {
            Some(Value::Object(servers)) => servers,
            _ => map,
        },
        _ => Map::new(),
    }
}

/// `${CLAUDE_PLUGIN_ROOT}` and `${CLAUDE_PLUGIN_DATA}` replaced in every
/// string in `value`.
fn expand_strings(value: &mut Value, plugin: &PluginDirs) {
    match value {
        Value::String(s) => *s = plugin.expand(s),
        Value::Array(items) => items.iter_mut().for_each(|v| expand_strings(v, plugin)),
        Value::Object(map) => map.values_mut().for_each(|v| expand_strings(v, plugin)),
        _ => {}
    }
}

/// `path` in `root`, refusing an absolute path or one with `..`.
fn inside(root: &Path, path: &str) -> Result<PathBuf, String> {
    let relative = Path::new(path);
    if relative.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        return Err(format!("path `{path}` leaves the plugin's directory"));
    }
    Ok(root.join(relative))
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => PathBuf::from(path),
    }
}

fn read_json(file: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("reading {}: {e}", file.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("parsing {}: {e}", file.display()))
}

/// A name as a directory name: `[A-Za-z0-9_-]`, anything else as `-`.
fn sanitize(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::Event;
    use fetch::tests::repo;

    fn write(dir: &Path, files: &[(&str, &str)]) {
        for (path, text) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    fn settings(json: &str) -> Settings {
        Settings::parse(json).unwrap()
    }

    /// A plugin with one of each component.
    const FULL: &[(&str, &str)] = &[
        (".claude-plugin/plugin.json", r#"{"name": "kit", "version": "1.2.0", "description": "A kit"}"#),
        ("skills/review/SKILL.md", "---\ndescription: Review code\n---\nLook at ${CLAUDE_PLUGIN_ROOT}/rules.md\n"),
        ("commands/commit.md", "---\ndescription: Commit\n---\nCommit $ARGUMENTS\n"),
        ("commands/db/migrate.md", "Run the migrations\n"),
        ("agents/reviewer.md", "---\ndescription: Reviews\ntools: Read, Grep, Glob, LS\nmodel: sonnet\n---\nUse ${CLAUDE_PLUGIN_ROOT}.\n"),
        (
            "hooks/hooks.json",
            r#"{"description": "x", "hooks": {"PostToolUse": [{"matcher": "Write", "hooks": [{"type": "command", "command": "\"${CLAUDE_PLUGIN_ROOT}/fmt.sh\""}]}], "Notification": []}}"#,
        ),
        (".mcp.json", r#"{"db": {"command": "${CLAUDE_PLUGIN_ROOT}/server", "args": ["--data", "${CLAUDE_PLUGIN_DATA}"]}}"#),
        ("bin/kit-tool", "#!/bin/sh\n"),
    ];

    #[test]
    fn loads_every_component_of_a_plugin_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("kit-src");
        write(&root, FULL);
        let home = tmp.path().join("home");
        let loaded = load_from(&Settings::default(), std::slice::from_ref(&root), &home);

        let plugin = &loaded.plugins.list[0];
        assert_eq!(plugin.id, "kit@inline");
        assert_eq!(plugin.version.as_deref(), Some("1.2.0"));
        assert_eq!(
            plugin.outcome.as_ref().unwrap(),
            &["1 skill", "2 commands", "1 agent", "1 MCP server", "hooks (PostToolUse)", "bin/"]
        );
        let names: Vec<&str> = loaded.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["kit:review", "kit:commit", "kit:db:migrate"]);
        let migrate = &loaded.skills[2];
        assert_eq!(migrate.description, "Run the migrations");

        let agent = &loaded.agents[0];
        assert_eq!(agent.name, "kit:reviewer");
        assert_eq!(agent.tools.as_deref().unwrap(), ["read", "grep", "glob"]);
        assert_eq!(agent.permission, subagent::Permission::Write, "plugin agents may write unless they say otherwise");
        assert_eq!(agent.model, None, "Claude Code's alias means the default model");
        assert_eq!(agent.prompt, format!("Use {}.", root.display()));

        assert_eq!(loaded.hooks[&Event::PostToolUse].len(), 1);
        assert_eq!(loaded.mcp_servers.keys().collect::<Vec<_>>(), ["plugin:kit:db"]);
        assert_eq!(loaded.bin_dirs, [root.join("bin")]);
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
        assert!(loaded.warnings[0].starts_with("plugin kit@inline: hook event `Notification`"));
        assert!(home.join("data/kit-inline").is_dir());
    }

    #[test]
    fn manifest_paths_replace_or_add_to_the_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("p");
        write(&root, &[
            (
                ".claude-plugin/plugin.json",
                r#"{"name": "p", "skills": "./more", "commands": {"about": {"content": "Say what p does", "description": "About"}},
                    "agents": ["./custom/a.md"], "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "true"}]}]},
                    "mcpServers": [{"web": {"url": "https://e.com/mcp"}}, "./extra.mcpb"]}"#,
            ),
            ("skills/one/SKILL.md", "---\ndescription: one\n---\n1\n"),
            ("more/two/SKILL.md", "---\ndescription: two\n---\n2\n"),
            ("commands/ignored.md", "not loaded: the manifest names the commands\n"),
            ("custom/a.md", "---\ndescription: A\n---\nprompt\n"),
            ("agents/ignored.md", "---\ndescription: B\n---\nprompt\n"),
        ]);
        let loaded = load_from(&Settings::default(), &[root], &tmp.path().join("home"));
        let names: Vec<&str> = loaded.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["p:one", "p:two", "p:about"]);
        assert_eq!(loaded.agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["p:a"]);
        assert_eq!(loaded.hooks[&Event::Stop].len(), 1);
        assert_eq!(loaded.mcp_servers.keys().collect::<Vec<_>>(), ["plugin:p:web"]);
        assert!(loaded.warnings[0].contains("extra.mcpb"), "{:?}", loaded.warnings);
    }

    #[test]
    fn installs_from_marketplaces() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        // A plugin in a repository of its own.
        let solo = tmp.path().join("solo");
        repo(&solo, &[("skills/hi/SKILL.md", "---\ndescription: hi\n---\nhi\n")]);
        // A marketplace in git with a plugin inside it, one by a bare name,
        // and one elsewhere.
        let market = tmp.path().join("market");
        let catalog = serde_json::json!({
            "name": "team",
            "owner": {"name": "t"},
            "metadata": {"pluginRoot": "./plugins"},
            "plugins": [
                {"name": "fmt", "source": "./tools/fmt", "description": "Formats"},
                {"name": "bare", "source": "bare"},
                {"name": "solo", "source": {"source": "url", "url": format!("file://{}", solo.display())}},
                {"name": "npm-one", "source": {"source": "npm", "package": "x"}},
                {"name": "escape", "source": "./../x"}
            ]
        });
        repo(&market, &[
            (".claude-plugin/marketplace.json", &catalog.to_string()),
            ("tools/fmt/commands/go.md", "Format\n"),
            ("plugins/bare/commands/b.md", "Bare\n"),
        ]);
        let s = settings(&format!(
            r#"{{"enabledPlugins": {{"fmt@team": true, "bare@team": true, "solo@team": true, "npm-one@team": true,
                 "escape@team": true, "off@team": false, "x@nowhere": true, "noat": true}},
                "extraKnownMarketplaces": {{"team": {{"source": {{"source": "git", "url": "file://{}"}}}}}}}}"#,
            market.display()
        ));
        let loaded = load_from(&s, &[], &home);
        let outcomes: BTreeMap<&str, Result<&Vec<String>, &String>> =
            loaded.plugins.list.iter().map(|p| (p.id.as_str(), p.outcome.as_ref())).collect();
        assert_eq!(outcomes["fmt@team"], Ok(&vec!["1 command".to_string()]));
        assert_eq!(outcomes["bare@team"], Ok(&vec!["1 command".to_string()]));
        assert_eq!(outcomes["solo@team"], Ok(&vec!["1 skill".to_string()]));
        assert!(outcomes["npm-one@team"].unwrap_err().contains("`npm` is not supported"));
        assert!(outcomes["escape@team"].unwrap_err().contains("leaves the plugin's directory"));
        assert!(outcomes["x@nowhere"].unwrap_err().contains("unknown marketplace `nowhere`"));
        assert!(outcomes["noat"].unwrap_err().contains("<plugin>@<marketplace>"));
        assert!(!outcomes.contains_key("off@team"));
        let names: Vec<&str> = loaded.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["bare:b", "fmt:go", "solo:hi"]);
        assert!(home.join("marketplaces/team/.claude-plugin/marketplace.json").is_file());
        assert!(home.join("cache/team/solo/skills").is_dir());
        // The marketplace once, however many plugins come from it, and the
        // plugin in a repository of its own.
        let labels: Vec<&str> = loaded.plugins.checkouts.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["marketplace team", "plugin solo@team"]);
        assert!(loaded.plugins.update().iter().all(|r| r.as_ref().is_ok_and(|l| l.contains("up to date"))));

        // A local marketplace is read in place.
        let s = settings(&format!(
            r#"{{"enabledPlugins": {{"fmt@local": true}},
                "extraKnownMarketplaces": {{"local": {{"source": {{"source": "directory", "path": "{}"}}}}}}}}"#,
            market.display()
        ));
        let loaded = load_from(&s, &[], &home);
        assert_eq!(loaded.plugins.list[0].root.as_deref(), Some(market.join("tools/fmt").as_path()));
        assert!(loaded.plugins.checkouts.is_empty());
    }

    #[test]
    fn plugin_dir_flag() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (dirs, rest) = parse_flag(args(&["--plugin-dir", "a", "hi", "--plugin-dir=b"])).unwrap();
        assert_eq!((dirs, rest), (vec![PathBuf::from("a"), PathBuf::from("b")], args(&["hi"])));
        assert!(parse_flag(args(&["--plugin-dir"])).is_err());
    }
}
