//! Plugins in Claude Code's format: a directory bundling skills, commands,
//! agents, hooks and MCP servers, usually installed from a marketplace (a
//! repository listing plugins in `.claude-plugin/marketplace.json`). Codex's
//! and Cursor's plugins are much the same and load too: their manifests
//! (`.codex-plugin/plugin.json`, an Agent Plugins `plugin.json`) and
//! marketplaces (`.agents/plugins/marketplace.json`) are found as well.
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
use crate::skills::{self, PluginDirs, Skill, markdown_files, scoped, stem};
use crate::subagent::{self, AgentDef};
pub use fetch::Checkout;

/// The marketplaces Claude Code and Codex know without being told, and
/// their GitHub repositories; the harness knows them too.
const OFFICIAL: &[(&str, &str)] =
    &[("claude-plugins-official", "anthropics/claude-plugins-official"), ("openai-curated", "openai/plugins")];
/// Where a marketplace keeps its catalog: Claude Code's place, Codex's,
/// Cursor's. The directory they are in is the marketplace's root.
const CATALOG_FILES: &[&str] =
    &[".claude-plugin/marketplace.json", ".agents/plugins/marketplace.json", ".cursor-plugin/marketplace.json"];
/// Where a plugin keeps its manifest, in the same order. An Agent Plugins
/// `plugin.json` at the root comes after Claude Code's.
const MANIFEST_FILES: &[&str] = &[".claude-plugin/plugin.json", ".codex-plugin/plugin.json", ".cursor-plugin/plugin.json"];
/// The schemas an Agent Plugins manifest (Codex's portable format) names.
const AGENT_PLUGINS_SCHEMA: &str = "https://agent-plugins.org/schemas/";

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
        /// The marketplace file in it, if not in one of the usual places.
        path: Option<String>,
    },
    /// Any git repository.
    Git {
        url: String,
        #[serde(rename = "ref")]
        git_ref: Option<String>,
        path: Option<String>,
    },
    /// A directory holding `.claude-plugin/marketplace.json` (or Codex's
    /// `.agents/plugins/marketplace.json`), read in place.
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
    let personal = std::env::var_os("HOME").map(|h| Path::new(&h).join(".agents/plugins/marketplace.json"));
    load_from(settings, dirs, &home(), personal.as_deref())
}

/// `personal` is Codex's personal marketplace, known by the name it gives
/// itself.
fn load_from(settings: &Settings, dirs: &[PathBuf], home: &Path, personal: Option<&Path>) -> Loaded {
    let mut loaded = Loaded::default();
    let mut catalogs: BTreeMap<String, Result<Catalog, String>> = BTreeMap::new();
    for id in settings.enabled_plugins.iter().filter(|(_, on)| **on).map(|(id, _)| id) {
        let found = match id.rsplit_once('@') {
            None => Err("expected `<plugin>@<marketplace>`".to_string()),
            Some((name, market)) => {
                let catalog = catalogs.entry(market.to_string()).or_insert_with(|| {
                    let catalog = Catalog::open(market, settings, home, personal);
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
    fn open(name: &str, settings: &Settings, home: &Path, personal: Option<&Path>) -> Result<Self, String> {
        if name.is_empty() || name.starts_with('.') || !name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
            return Err(format!("invalid marketplace name `{name}`"));
        }
        let official = OFFICIAL.iter().find(|(known, _)| *known == name);
        let personal = personal.filter(|file| read_json(file).is_ok_and(|catalog| catalog["name"] == name));
        let source = match (settings.extra_known_marketplaces.get(name), official, personal) {
            (Some(market), _, _) => market.source.clone(),
            (None, Some((_, repo)), _) => MarketplaceSource::Github { repo: repo.to_string(), git_ref: None, path: None },
            (None, None, Some(file)) => MarketplaceSource::File { path: file.display().to_string() },
            (None, None, None) => {
                return Err(format!("unknown marketplace `{name}`: declare it under `extraKnownMarketplaces`"));
            }
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
            let file = match path {
                Some(path) => inside(&checkout.dir, path)?,
                None => catalog_file(&checkout.dir)?,
            };
            Ok::<_, String>((file, Some(checkout)))
        };
        let (file, checkout) = match &source {
            MarketplaceSource::Github { repo, git_ref, path } => clone(github_url(repo), git_ref, path)?,
            MarketplaceSource::Git { url, git_ref, path } => clone(url.clone(), git_ref, path)?,
            MarketplaceSource::Directory { path } => (catalog_file(&expand_home(path))?, None),
            MarketplaceSource::File { path } => (expand_home(path), None),
        };
        let catalog = read_json(&file)?;
        let root = catalog_root(&file);
        Ok(Self {
            name: name.to_string(),
            root,
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
        // A path in the marketplace: a string, or Codex's `local` source.
        let local = match &entry["source"] {
            Value::String(path) => Some(path.as_str()),
            Value::Object(source) if source.get("source").is_some_and(|s| s == "local") => {
                Some(source.get("path").and_then(Value::as_str).ok_or("plugin source needs `path`")?)
            }
            _ => None,
        };
        let (root, checkout) = match (local, &entry["source"]) {
            (Some(path), _) => {
                let bare = !path.contains('/') && path != ".";
                let path = match &self.plugin_root {
                    Some(plugin_root) if bare => format!("{}/{path}", plugin_root.trim_end_matches('/')),
                    _ => path.to_string(),
                };
                (inside(&self.root, &path)?, None)
            }
            (None, Value::Object(source)) => {
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
        // Codex's may name a directory in it.
        Some("url") => (required("url")?, field("path")),
        Some("git-subdir") => {
            let url = required("url")?;
            // `owner/repo` is GitHub's.
            let shorthand = !url.contains(':') && url.matches('/').count() == 1;
            (if shorthand { github_url(&url) } else { url }, Some(required("path")?))
        }
        Some(other) => {
            return Err(format!(
                "plugin source `{other}` is not supported (only relative paths, `local`, `github`, `url` and `git-subdir`)"
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
        let manifest = match (read_manifest(&root)?, entry) {
            (Some(mut manifest), entry) => {
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
            (None, Some(entry)) => entry.clone(),
            (None, None) => Value::Object(Map::new()),
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
        if manifest.get("apps").is_some() || root.join(".app.json").is_file() {
            warnings.push("its apps (Codex's ChatGPT connectors) are not supported; left out".into());
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
        let mut count = 0;
        for (server, mut config) in servers {
            expand_strings(&mut config, dirs);
            let Some(fields) = config.as_object_mut() else {
                warnings.push(format!("MCP server `{server}`: expected an object; left out"));
                continue;
            };
            if fields.contains_key("oauth") || fields.contains_key("oauth_resource") {
                warnings.push(format!("MCP server `{server}` signs in with OAuth, which the harness does not do; left out"));
                continue;
            }
            // Codex's: comments, the variables it may see (it sees them all),
            // its timeouts (the harness has its own), and a directory in the
            // plugin that a `./` command starts from too.
            for key in ["note", "description", "env_vars", "startup_timeout_sec", "tool_timeout_sec"] {
                fields.remove(key);
            }
            // Joined without their `.` parts.
            let join = |dir: &Path, path: &str| dir.join(path).components().collect::<PathBuf>();
            if let Some(cwd) = fields.get("cwd").and_then(Value::as_str).map(|cwd| join(&dirs.root, cwd)) {
                if let Some(command) = fields.get("command").and_then(Value::as_str).filter(|c| c.starts_with("./")) {
                    fields.insert("command".into(), join(&cwd, command).display().to_string().into());
                }
                fields.insert("cwd".into(), cwd.display().to_string().into());
            }
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
                    count += 1;
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
        match Skill::command(scoped(&prefix, &stem(&file)), &file, Some(plugin.clone())) {
            Ok(skill) => out.push(skill),
            Err(e) => warnings.push(format!("{}: {e}", file.display())),
        }
    }
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

/// The catalog in marketplace directory `dir`, in whichever usual place.
fn catalog_file(dir: &Path) -> Result<PathBuf, String> {
    let found = CATALOG_FILES.iter().map(|f| dir.join(f)).find(|f| f.is_file());
    found.ok_or_else(|| format!("{} has no {}", dir.display(), CATALOG_FILES.join(" or ")))
}

/// The marketplace root of catalog `file`: the directory holding the usual
/// place it is in, else the one it is in.
fn catalog_root(file: &Path) -> PathBuf {
    let root = CATALOG_FILES
        .iter()
        .find_map(|layout| {
            let depth = Path::new(layout).components().count();
            file.ends_with(layout).then(|| file.ancestors().nth(depth)).flatten()
        })
        .or_else(|| file.parent())
        .unwrap_or(Path::new("."));
    std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf())
}

/// The plugin's manifest, if it has one: Claude Code's, an Agent Plugins
/// `plugin.json` at the root (Codex's portable format, as Claude Code's
/// shape), or Codex's or Cursor's, which have Claude Code's shape already.
fn read_manifest(root: &Path) -> Result<Option<Value>, String> {
    let file = |f: &str| Some(root.join(f)).filter(|f| f.is_file());
    if let Some(file) = file(MANIFEST_FILES[0]) {
        return read_json(&file).map(Some);
    }
    if let Some(portable) = file("plugin.json") {
        let manifest = read_json(&portable)?;
        if manifest["$schema"].as_str().is_some_and(|s| s.starts_with(AGENT_PLUGINS_SCHEMA)) {
            return Ok(Some(agent_plugin_manifest(root, manifest)?));
        }
    }
    match MANIFEST_FILES[1..].iter().find_map(|f| file(f)) {
        Some(file) => read_json(&file).map(Some),
        None => Ok(None),
    }
}

/// An Agent Plugins manifest as Claude Code's: skills in `skills/`, MCP
/// servers in `mcp.json`, and Codex's hooks from its `com.openai` extension,
/// else from a `.codex-plugin/plugin.json` beside it.
fn agent_plugin_manifest(root: &Path, manifest: Value) -> Result<Value, String> {
    let mut converted = Map::new();
    for key in ["name", "version", "description"] {
        if let Some(value) = manifest.get(key) {
            converted.insert(key.into(), value.clone());
        }
    }
    if root.join("mcp.json").is_file() {
        converted.insert("mcpServers".into(), "./mcp.json".into());
    }
    let extension = &manifest["extensions"]["com.openai"];
    let overlay = root.join(MANIFEST_FILES[1]);
    let extension = match extension.is_object() || !overlay.is_file() {
        true => extension.clone(),
        false => read_json(&overlay)?,
    };
    for key in ["hooks", "apps"] {
        if let Some(value) = extension.get(key).filter(|v| !v.is_null()) {
            converted.insert(key.into(), value.clone());
        }
    }
    Ok(Value::Object(converted))
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
        let loaded = load_from(&Settings::default(), std::slice::from_ref(&root), &home, None);

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
        let loaded = load_from(&Settings::default(), &[root], &tmp.path().join("home"), None);
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
        let loaded = load_from(&s, &[], &home, None);
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
        let loaded = load_from(&s, &[], &home, None);
        assert_eq!(loaded.plugins.list[0].root.as_deref(), Some(market.join("tools/fmt").as_path()));
        assert!(loaded.plugins.checkouts.is_empty());
    }

    #[test]
    fn loads_codex_plugins_and_marketplaces() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        // Codex's layout: the catalog in `.agents/plugins/`, `local` sources,
        // a `.codex-plugin/plugin.json` manifest, and an Agent Plugins one.
        let market = tmp.path().join("codex-market");
        write(&market, &[
            (
                ".agents/plugins/marketplace.json",
                r#"{"name": "codex-team", "plugins": [
                    {"name": "legacy", "source": {"source": "local", "path": "./plugins/legacy"}, "policy": {"installation": "AVAILABLE"}},
                    {"name": "portable", "source": {"source": "local", "path": "./plugins/portable"}}]}"#,
            ),
            (
                "plugins/legacy/.codex-plugin/plugin.json",
                r#"{"name": "legacy", "skills": "./skills/", "mcpServers": "./.mcp.json", "apps": "./.app.json",
                    "hooks": "./hooks.json", "interface": {"displayName": "Legacy"}}"#,
            ),
            ("plugins/legacy/skills/go/SKILL.md", "---\ndescription: Go\n---\nGo.\n"),
            (
                "plugins/legacy/.mcp.json",
                r#"{"mcpServers": {"api": {"type": "http", "url": "https://e.com/mcp", "note": "a comment", "description": "d"},
                    "tool": {"command": "./bin/srv", "cwd": ".", "env_vars": ["HOME"]},
                    "signed": {"type": "http", "url": "https://e.com/oauth", "oauth_resource": "https://e.com"}}}"#,
            ),
            ("plugins/legacy/.app.json", r#"{"apps": {"x": {"id": "connector_1"}}}"#),
            (
                "plugins/legacy/hooks.json",
                r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "\"${PLUGIN_ROOT}/stop.sh\""}]}]}}"#,
            ),
            (
                "plugins/portable/plugin.json",
                r#"{"$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json", "name": "portable", "version": "2.0.0",
                    "extensions": {"com.openai": {"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "true"}]}]}}}}"#,
            ),
            ("plugins/portable/skills/hi/SKILL.md", "---\ndescription: Hi\n---\nHi from ${PLUGIN_ROOT}.\n"),
            ("plugins/portable/mcp.json", r#"{"mcpServers": {"local": {"command": "${PLUGIN_ROOT}/srv"}}}"#),
        ]);
        let s = settings(&format!(
            r#"{{"enabledPlugins": {{"legacy@codex-team": true, "portable@codex-team": true}},
                "extraKnownMarketplaces": {{"codex-team": {{"source": {{"source": "directory", "path": "{}"}}}}}}}}"#,
            market.display()
        ));
        let loaded = load_from(&s, &[], &home, None);
        let outcomes: Vec<(&str, &Vec<String>)> =
            loaded.plugins.list.iter().map(|p| (p.id.as_str(), p.outcome.as_ref().unwrap())).collect();
        assert_eq!(outcomes, [
            ("legacy@codex-team", &vec!["1 skill".to_string(), "2 MCP servers".into(), "hooks (Stop)".into()]),
            ("portable@codex-team", &vec!["1 skill".to_string(), "1 MCP server".into(), "hooks (SessionStart)".into()]),
        ]);
        assert_eq!(loaded.plugins.list[1].version.as_deref(), Some("2.0.0"));
        let servers = loaded.mcp_servers.keys().collect::<Vec<_>>();
        assert_eq!(servers, ["plugin:legacy:api", "plugin:legacy:tool", "plugin:portable:local"]);
        let tool = format!("{:?}", loaded.mcp_servers["plugin:legacy:tool"]);
        let legacy = market.join("plugins/legacy");
        assert!(tool.contains(&format!("command: Some({:?})", legacy.join("bin/srv"))), "{tool}");
        assert!(tool.contains(&format!("cwd: Some({:?})", legacy)), "{tool}");
        let root = market.join("plugins/portable");
        assert_eq!(loaded.skills[1].plugin.as_ref().unwrap().expand("${PLUGIN_ROOT}"), root.display().to_string());
        assert_eq!(loaded.warnings.len(), 2, "{:?}", loaded.warnings);
        assert!(loaded.warnings[0].contains("`signed` signs in with OAuth"), "{:?}", loaded.warnings);
        assert!(loaded.warnings[1].contains("apps"), "{:?}", loaded.warnings);

        // Codex's personal marketplace is known by its own name; its paths
        // start from the directory holding `.agents/`.
        let user = tmp.path().join("user");
        write(&user, &[
            (".agents/plugins/marketplace.json", r#"{"name": "mine", "plugins": [{"name": "p", "source": "./.codex/plugins/p"}]}"#),
            (".codex/plugins/p/commands/c.md", "C\n"),
        ]);
        let personal = user.join(".agents/plugins/marketplace.json");
        let s = settings(r#"{"enabledPlugins": {"p@mine": true}}"#);
        let loaded = load_from(&s, &[], &home, Some(&personal));
        assert_eq!(loaded.skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["p:c"]);
        let loaded = load_from(&s, &[], &home, None);
        assert!(loaded.plugins.list[0].outcome.as_ref().unwrap_err().contains("unknown marketplace `mine`"));
    }

    #[test]
    fn catalog_places() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for layout in CATALOG_FILES {
            assert_eq!(catalog_root(&root.join(layout)), root);
        }
        assert_eq!(catalog_root(&root.join("x/catalog.json")), root.join("x"));
        assert!(catalog_file(root).unwrap_err().contains(".agents/plugins/marketplace.json"));
        write(root, &[(".agents/plugins/marketplace.json", "{}")]);
        assert_eq!(catalog_file(root).unwrap(), root.join(".agents/plugins/marketplace.json"));
    }

    #[test]
    fn plugin_dir_flag() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (dirs, rest) = parse_flag(args(&["--plugin-dir", "a", "hi", "--plugin-dir=b"])).unwrap();
        assert_eq!((dirs, rest), (vec![PathBuf::from("a"), PathBuf::from("b")], args(&["hi"])));
        assert!(parse_flag(args(&["--plugin-dir"])).is_err());
    }
}
