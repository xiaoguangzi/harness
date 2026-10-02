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
//! Nothing is read from the project: a repository could otherwise send
//! requests carrying your API keys to its own server, start MCP commands or
//! hooks, or loosen the sandbox.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::llm::Effort;
use crate::llm::config::ProviderConfig;
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
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: BTreeMap<String, mcp::ServerConfig>,
    #[serde(default)]
    pub sandbox: sandbox::Config,
    #[serde(default)]
    pub hooks: hooks::Config,
    /// `"<plugin>@<marketplace>": true` for each plugin to load.
    #[serde(default, rename = "enabledPlugins")]
    pub enabled_plugins: BTreeMap<String, bool>,
    /// Marketplaces to find plugins in, by name, besides the official one.
    #[serde(default, rename = "extraKnownMarketplaces")]
    pub extra_known_marketplaces: BTreeMap<String, plugins::Marketplace>,
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
    let home = || std::env::var_os("HOME").map(|h| Path::new(&h).join(".harness/settings.json"));
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
}

/// Config files earlier versions read and this one ignores, so their
/// settings don't silently stop applying.
pub fn legacy_files() -> Vec<PathBuf> {
    let mut files = vec![PathBuf::from(".harness/providers.json")];
    if let Some(home) = std::env::var_os("HOME") {
        let dir = Path::new(&home).join(".harness");
        files.extend(["providers.json", "mcp.json", "sandbox.json"].map(|f| dir.join(f)));
    }
    files.retain(|f| f.exists());
    files
}

#[cfg(test)]
mod tests {
    use super::*;

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
              "extraKnownMarketplaces": {"team": {"source": {"source": "github", "repo": "org/plugins"}}}
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
    fn settings_flag() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (path, rest) = parse_flag(args(&["--settings", "a.json", "-c"])).unwrap();
        assert_eq!((path, rest), (Some(PathBuf::from("a.json")), args(&["-c"])));
        let (path, rest) = parse_flag(args(&["hi", "--settings=b.json"])).unwrap();
        assert_eq!((path, rest), (Some(PathBuf::from("b.json")), args(&["hi"])));
        assert!(parse_flag(args(&["--settings"])).is_err());
    }
}
