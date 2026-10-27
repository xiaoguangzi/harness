//! The settings file, `~/.harness/settings.json`: the model and provider
//! setup, MCP servers, the sandbox, hooks and plugins in one place.
//!
//! ```json
//! {
//!   "model": "relay-a/claude-opus-5",
//!   "effort": "high",
//!   "providers": { "relay-a": { "protocol": "anthropic", "base_url": "...", "api_key": "${RELAY_A_KEY}" } },
//!   "mcpServers": { "exa": { "url": "https://mcp.exa.ai/mcp" } },
//!   "sandbox": { "enabled": true, "network": false },
//!   "hooks": { "PostToolUse": [{ "matcher": "Edit|Write", "hooks": [{ "type": "command", "command": "fmt.sh" }] }] },
//!   "enabledPlugins": { "commit-commands@claude-plugins-official": true }
//! }
//! ```
//!
//! `--settings FILE` or `$HARNESS_SETTINGS` reads another file instead.
//! `harness plugin …` changes it through [`edit`], which keeps the rest of
//! the file as it is.
//! Nothing is read from the project: a repository could otherwise send
//! requests carrying your API keys to its own server, start MCP commands or
//! hooks, or loosen the sandbox.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::llm::Effort;
use crate::llm::config::ProviderConfig;
use crate::tools::bash;
use crate::util::env_refs;
use crate::{hooks, mcp, plugins, sandbox};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// `<provider>/<model>`, or a bare model name.
    pub model: Option<String>,
    pub subagent_model: Option<String>,
    pub effort: Option<Effort>,
    pub max_tokens: Option<u32>,
    pub context_window: Option<u32>,
    /// Compact a long conversation while it waits for the user, before its
    /// prompt cache expires. Default: on.
    pub idle_compaction: Option<bool>,
    /// The tokens of context from which an idle conversation is compacted.
    pub idle_compaction_min_tokens: Option<u64>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: BTreeMap<String, mcp::ServerConfig>,
    #[serde(default)]
    pub sandbox: sandbox::Config,
    #[serde(default)]
    pub permissions: crate::permissions::Config,
    #[serde(default)]
    pub shell: bash::Config,
    #[serde(default)]
    pub hooks: hooks::Config,
    /// `"<plugin>@<marketplace>": true` for each plugin to load.
    #[serde(default, rename = "enabledPlugins")]
    pub enabled_plugins: BTreeMap<String, bool>,
    /// Marketplaces to find plugins in, by name, besides the official one.
    #[serde(default, rename = "extraKnownMarketplaces")]
    pub extra_known_marketplaces: BTreeMap<String, plugins::Marketplace>,
    /// The line under the REPL's input box, from a command.
    #[serde(rename = "statusLine")]
    pub status_line: Option<crate::cli::status_line::Config>,
    /// Each plugin's answers to its `userConfig`, by plugin id; the
    /// sensitive ones are kept elsewhere.
    #[serde(default, rename = "pluginConfigs")]
    pub plugin_configs: BTreeMap<String, PluginConfig>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginConfig {
    #[serde(default)]
    pub options: Map<String, Value>,
}

static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Takes `--settings FILE` (or `--settings=FILE`) out of the command line.
pub fn parse_flag(args: Vec<String>) -> Result<(Option<PathBuf>, Vec<String>)> {
    let mut path = None;
    let mut rest = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--settings" {
            let Some(file) = args.next() else { bail!("--settings needs a file") };
            path = Some(PathBuf::from(file));
        } else if let Some(file) = arg.strip_prefix("--settings=") {
            path = Some(PathBuf::from(file));
        } else {
            rest.push(arg);
        }
    }
    Ok((path, rest))
}

/// Chooses the settings file: `flag`, else `$HARNESS_SETTINGS`, else
/// `~/.harness/settings.json`. Call once, before [`path`].
pub fn set_path(flag: Option<PathBuf>) {
    let env = std::env::var_os("HARNESS_SETTINGS").filter(|v| !v.is_empty()).map(PathBuf::from);
    let home = || crate::util::harness_home().map(|dir| dir.join("settings.json"));
    let _ = PATH.set(flag.or(env).or_else(home));
}

/// The settings file, or `None` without one (no home directory).
pub fn path() -> Option<&'static Path> {
    PATH.get_or_init(|| None).as_deref()
}

impl Settings {
    /// Reads the settings file; a missing or empty one is all defaults.
    pub fn load() -> Result<Self> {
        let Some(path) = path() else { return Ok(Self::default()) };
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_str(text)?)
    }

    /// Leaves out what `--bare` does not load: MCP servers, hooks and the
    /// enabled plugins.
    pub fn strip_for_bare(&mut self) {
        if crate::util::bare() {
            self.mcp_servers.clear();
            self.hooks.clear();
            self.enabled_plugins.clear();
        }
    }

    /// The variables holding credentials the harness sends: the default
    /// providers' keys, and the ones the providers' `api_key` and headers
    /// and the MCP servers' headers refer to, but those `shell.keep_env`
    /// keeps. The commands the model runs don't get them.
    pub fn credential_vars(&self) -> Vec<String> {
        let providers = self.providers.values().flat_map(|p| p.api_key.iter().chain(p.headers.values()));
        let mcp = self.mcp_servers.values().flat_map(|s| s.headers.values());
        let referred = providers.chain(mcp).flat_map(|v| env_refs(v));
        let mut names: Vec<String> = bash::CREDENTIAL_VARS.into_iter().chain(referred).map(String::from).collect();
        names.retain(|n| !self.shell.keep_env.contains(n));
        names.sort();
        names.dedup();
        names
    }
}

/// Changes the settings file with `change`, which gets it as JSON (an
/// empty object if there is none). Keys keep their order and the ones
/// `change` leaves alone stay as they are; the result must still be valid
/// settings, or nothing is written.
pub fn edit<T>(change: impl FnOnce(&mut Map<String, Value>) -> Result<T>) -> Result<T> {
    let path = path().context("no settings file: HOME is not set")?;
    edit_file(path, change)
}

pub(crate) fn edit_file<T>(path: &Path, change: impl FnOnce(&mut Map<String, Value>) -> Result<T>) -> Result<T> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut value = match text.trim() {
        "" => Map::new(),
        text => match serde_json::from_str(text).with_context(|| format!("parsing {}", path.display()))? {
            Value::Object(map) => map,
            _ => bail!("{} does not hold a JSON object", path.display()),
        },
    };
    let result = change(&mut value)?;
    let text = serde_json::to_string_pretty(&value)? + "\n";
    Settings::parse(&text).with_context(|| format!("the change would make {} invalid", path.display()))?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    write_private(path, &text).with_context(|| format!("writing {}", path.display()))?;
    Ok(result)
}

/// The object at `key` in `map`, made one if it is missing or something
/// else.
pub fn object<'a>(map: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    let value = map.entry(key).or_insert_with(|| Value::Object(Map::new()));
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value.as_object_mut().unwrap()
}

/// Writes a file only its owner can read, as it may hold API keys; an
/// existing file is narrowed to that too.
pub fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(text.as_bytes())
}

/// Config files earlier versions read and this one ignores, so their
/// settings don't silently stop applying.
pub fn legacy_files() -> Vec<PathBuf> {
    let mut files = vec![PathBuf::from(".harness/providers.json")];
    if let Some(dir) = crate::util::harness_home() {
        files.extend(["providers.json", "mcp.json", "sandbox.json"].map(|f| dir.join(f)));
    }
    files.retain(|f| f.exists());
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_vars_are_the_keys_the_harness_sends() {
        let s = Settings::parse(
            r#"{
              "providers": {"gw": {"protocol": "anthropic", "base_url": "${GW_URL}", "api_key": "${GW_KEY}",
                                   "headers": {"X-Team": "${TEAM:-t}"}}},
              "mcpServers": {"exa": {"url": "https://mcp.exa.ai/mcp", "headers": {"Authorization": "Bearer ${EXA_KEY}"}},
                             "gh": {"command": "gh-mcp", "env": {"GITHUB_TOKEN": "${GITHUB_TOKEN}"}}},
              "shell": {"keep_env": ["OPENAI_API_KEY"]}
            }"#,
        )
        .unwrap();
        // Not the URL, nor what MCP servers get in their environment.
        assert_eq!(
            s.credential_vars(),
            ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "EXA_KEY", "GW_KEY", "HARNESS_WEB_TOKEN", "TEAM"]
        );
    }

    #[test]
    fn parses_every_section() {
        let s = Settings::parse(
            r#"{
              "model": "gw/m", "effort": "xhigh", "max_tokens": 1000,
              "providers": {"gw": {"protocol": "anthropic", "base_url": "https://gw.example.com"}},
              "mcpServers": {"exa": {"url": "https://mcp.exa.ai/mcp"}},
              "sandbox": {"enabled": true, "allow_write": ["~/.cache"]},
              "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "true"}]}]},
              "enabledPlugins": {"fmt@team": true, "old@team": false},
              "extraKnownMarketplaces": {"team": {"source": {"source": "github", "repo": "org/plugins"}, "autoUpdate": true}},
              "pluginConfigs": {"fmt@team": {"options": {"width": 100}}}
            }"#,
        )
        .unwrap();
        assert_eq!(s.model.as_deref(), Some("gw/m"));
        assert_eq!(s.effort, Some(Effort::XHigh));
        assert_eq!(s.max_tokens, Some(1000));
        assert_eq!(s.providers.len(), 1);
        assert_eq!(s.mcp_servers.keys().collect::<Vec<_>>(), ["exa"]);
        assert_eq!(s.sandbox.enabled, Some(true));
        assert_eq!(s.hooks.len(), 1);
        assert_eq!(s.enabled_plugins.len(), 2);
        assert_eq!(s.extra_known_marketplaces.keys().collect::<Vec<_>>(), ["team"]);
        assert_eq!(s.plugin_configs["fmt@team"].options["width"], 100);

        assert!(Settings::parse("  ").unwrap().providers.is_empty());
        assert!(Settings::parse(r#"{"default_model": "gw/m"}"#).is_err(), "unknown keys are rejected");
        assert!(Settings::parse(r#"{"sandbox": {"netwrok": true}}"#).is_err());
        assert!(Settings::parse(r#"{"hooks": {"Stop": [{"hooks": [{"type": "command"}]}]}}"#).is_err());
        let market = |source: &str| format!(r#"{{"extraKnownMarketplaces": {{"m": {{"source": {source}}}}}}}"#);
        assert!(Settings::parse(&market(r#"{"source": "git", "url": "u", "ref": "main"}"#)).is_ok());
        assert!(Settings::parse(&market(r#"{"source": "directory", "path": "~/m"}"#)).is_ok());
        assert!(Settings::parse(&market(r#"{"source": "npm", "package": "p"}"#)).is_err());
        assert!(Settings::parse(&market(r#"{"source": "github", "repo": "o/r", "rev": "x"}"#)).is_err());
    }

    #[test]
    fn edits_keep_the_rest_of_the_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/settings.json");
        edit_file(&path, |s| {
            object(s, "enabledPlugins").insert("a@m".into(), true.into());
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::write(&path, r#"{"model": "gw/m", "providers": {"gw": {"protocol": "anthropic", "base_url": "u"}}, "enabledPlugins": {"a@m": true}}"#)
            .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        edit_file(&path, |s| {
            object(s, "enabledPlugins").insert("b@m".into(), false.into());
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "narrowed");
        let text = std::fs::read_to_string(&path).unwrap();
        let keys: Vec<String> = serde_json::from_str::<Map<String, Value>>(&text).unwrap().keys().cloned().collect();
        assert_eq!(keys, ["model", "providers", "enabledPlugins"]);
        assert!(text.contains(r#""b@m": false"#), "{text}");
        let err = edit_file(&path, |s| {
            s.insert("nonsense".into(), 1.into());
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("unknown field"), "{err:#}");
        assert!(!std::fs::read_to_string(&path).unwrap().contains("nonsense"), "nothing written");
    }

    #[test]
    fn settings_flag() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (path, rest) = parse_flag(args(&["--settings", "a.json", "-c"])).unwrap();
        assert_eq!((path, rest), (Some(PathBuf::from("a.json")), args(&["-c"])));
        let (path, rest) = parse_flag(args(&["hi", "--settings=b.json"])).unwrap();
        assert_eq!((path, rest), (Some(PathBuf::from("b.json")), args(&["hi"])));
        assert!(parse_flag(args(&["--settings"])).is_err());
    }
}
