//! Provider (gateway) configuration.
//!
//! `providers.json` declares the gateways to talk to and which protocol each
//! speaks. Models are addressed as `<provider>/<model>`:
//!
//! ```json
//! {
//!   "default_model": "relay-a/claude-opus-5",
//!   "subagent_model": "relay-b/gpt-5-mini",
//!   "providers": {
//!     "relay-a": {
//!       "protocol": "anthropic",
//!       "base_url": "https://relay-a.example.com",
//!       "api_key": "${RELAY_A_KEY}"
//!     },
//!     "relay-b": {
//!       "protocol": "openai-responses",
//!       "base_url": "https://relay-b.example.com/v1",
//!       "api_key": "${RELAY_B_KEY}",
//!       "models": { "gpt-5": { "extra_body": { "reasoning": { "effort": "high" } } } }
//!     }
//!   }
//! }
//! ```
//!
//! Files: `~/.harness/providers.json`, then `./.harness/providers.json`
//! (later files override earlier ones by provider name), or only
//! `$HARNESS_PROVIDERS` when set. `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN`
//! and `OPENAI_API_KEY` add implicit `anthropic` and `openai` providers when
//! no provider of that name is configured.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::util::{expand_env, merge_json};

const DEFAULT_ANTHROPIC_MODEL: &str = "claude-opus-5";
const DEFAULT_MAX_TOKENS: u32 = 64_000;
const OFFICIAL_ANTHROPIC_HOST: &str = "api.anthropic.com";

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub enum Protocol {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai-responses", alias = "openai", alias = "responses")]
    OpenAiResponses,
}

impl Protocol {
    /// The name used in config files.
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Anthropic => "anthropic",
            Protocol::OpenAiResponses => "openai-responses",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub enum AuthStyle {
    #[serde(rename = "x-api-key")]
    XApiKey,
    #[serde(rename = "bearer")]
    Bearer,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub protocol: Protocol,
    /// With or without a trailing `/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    /// Default: `x-api-key` for Anthropic, `bearer` for OpenAI.
    pub auth: Option<AuthStyle>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub max_tokens: Option<u32>,
    /// Merged into every request body; a `null` value removes a key.
    pub extra_body: Option<Map<String, Value>>,
    /// Anthropic only: server-side refusal fallbacks. Default: on for the
    /// official API, off for gateways.
    pub fallbacks: Option<bool>,
    /// Anthropic only: unbuffered tool-input streaming. Default as above.
    pub eager_input_streaming: Option<bool>,
    /// Anthropic only: automatic prompt caching. Default: on.
    pub prompt_cache: Option<bool>,
    /// Per-model overrides; listed models are offered by `/model`.
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub max_tokens: Option<u32>,
    pub extra_body: Option<Map<String, Value>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersFile {
    pub default_model: Option<String>,
    pub subagent_model: Option<String>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, Clone)]
pub struct Providers {
    providers: BTreeMap<String, ProviderConfig>,
    /// A resolved `provider/model` spec.
    pub default_model: String,
    /// Spec for subagents without their own model; `None` means the
    /// caller's model.
    pub subagent_model: Option<String>,
}

/// Everything needed to send one request: a provider plus a model on it.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    pub provider: &'a str,
    pub config: &'a ProviderConfig,
    pub model: &'a str,
}

/// The providers files, lowest precedence first: `$HARNESS_PROVIDERS`
/// alone, else `~/.harness/providers.json` then `./.harness/providers.json`.
pub fn config_paths() -> Vec<PathBuf> {
    match std::env::var_os("HARNESS_PROVIDERS") {
        Some(p) => vec![PathBuf::from(p)],
        None => {
            let mut v = Vec::new();
            if let Some(home) = std::env::var_os("HOME") {
                v.push(Path::new(&home).join(".harness/providers.json"));
            }
            v.push(PathBuf::from(".harness/providers.json"));
            v
        }
    }
}

/// The file the web UI edits: the highest-precedence one.
pub fn editable_path() -> PathBuf {
    config_paths().pop().expect("at least one providers path")
}

impl Providers {
    /// Loads the config files and environment described in the module docs.
    pub fn load() -> Result<Self> {
        Self::load_with(None)
    }

    /// Like [`Providers::load`], but with the file at `path` read as `text`
    /// instead, so an edit can be validated before it is saved.
    pub fn load_with(replace: Option<(&Path, &str)>) -> Result<Self> {
        let mut files = Vec::new();
        for path in config_paths() {
            let text = match replace {
                Some((p, text)) if p == path => text.to_string(),
                _ if !path.exists() => continue,
                _ => std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?,
            };
            if text.trim().is_empty() {
                continue;
            }
            let file: ProvidersFile =
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            files.push(file);
        }
        Self::from_parts(files, &|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
    }

    /// Builds the provider set from parsed files (lowest precedence first)
    /// and an environment lookup.
    pub fn from_parts(files: Vec<ProvidersFile>, env: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let mut providers = BTreeMap::new();
        let mut default_model = None;
        let mut subagent_model = None;
        for file in files {
            providers.extend(file.providers);
            default_model = file.default_model.or(default_model);
            subagent_model = file.subagent_model.or(subagent_model);
        }
        for p in providers.values_mut() {
            p.base_url = expand_env(&p.base_url).trim_end_matches('/').to_string();
            p.api_key = p.api_key.as_deref().map(expand_env).filter(|k| !k.is_empty());
            for v in p.headers.values_mut() {
                *v = expand_env(v);
            }
        }

        let anthropic_key = env("ANTHROPIC_API_KEY").map(|k| (k, AuthStyle::XApiKey));
        let anthropic_auth = anthropic_key.or(env("ANTHROPIC_AUTH_TOKEN").map(|t| (t, AuthStyle::Bearer)));
        if let Some((key, auth)) = anthropic_auth
            && !providers.contains_key("anthropic")
        {
            providers.insert(
                "anthropic".into(),
                ProviderConfig::implicit(
                    Protocol::Anthropic,
                    env("ANTHROPIC_BASE_URL").unwrap_or_else(|| format!("https://{OFFICIAL_ANTHROPIC_HOST}")),
                    key,
                    auth,
                ),
            );
        }
        if let Some(key) = env("OPENAI_API_KEY")
            && !providers.contains_key("openai")
        {
            providers.insert(
                "openai".into(),
                ProviderConfig::implicit(
                    Protocol::OpenAiResponses,
                    env("OPENAI_BASE_URL").unwrap_or_else(|| "https://api.openai.com/v1".into()),
                    key,
                    AuthStyle::Bearer,
                ),
            );
        }
        if providers.is_empty() {
            bail!(
                "no LLM provider configured: set ANTHROPIC_API_KEY (or OPENAI_API_KEY), or declare \
                 gateways in ~/.harness/providers.json or .harness/providers.json"
            );
        }

        let mut this = Self { providers, default_model: String::new(), subagent_model: None };
        let requested = env("HARNESS_MODEL").or(default_model).or_else(|| this.fallback_default());
        let requested = requested.ok_or_else(|| {
            anyhow!("no default model: set `default_model` in providers.json or HARNESS_MODEL=<provider>/<model>")
        })?;
        // A bare model name (e.g. HARNESS_MODEL=claude-opus-5) goes to the
        // sole provider, or to `anthropic` when there are several.
        let bare_provider = match this.names()[..] {
            [only] => Some(only.to_string()),
            _ => this.providers.contains_key("anthropic").then(|| "anthropic".to_string()),
        };
        this.default_model = this.resolve_spec(&requested, bare_provider.as_deref())?;
        if let Some(spec) = env("HARNESS_SUBAGENT_MODEL").or(subagent_model) {
            this.subagent_model = Some(this.resolve_spec(&spec, None)?);
        }
        Ok(this)
    }

    /// Without an explicit default: the implicit Anthropic provider's
    /// default model, or the first model listed on a sole provider.
    fn fallback_default(&self) -> Option<String> {
        if self.providers.get("anthropic").is_some_and(|p| p.protocol == Protocol::Anthropic) {
            return Some(format!("anthropic/{DEFAULT_ANTHROPIC_MODEL}"));
        }
        let (name, p) = self.providers.iter().next().filter(|_| self.providers.len() == 1)?;
        p.models.keys().next().map(|m| format!("{name}/{m}"))
    }

    /// Normalizes a spec to `provider/model`. The part before the first `/`
    /// names a provider only if one is configured under that name; otherwise
    /// the whole spec is a model on `default_provider` (or the default
    /// model's provider), so gateway model names containing `/` still work.
    pub fn resolve_spec(&self, spec: &str, default_provider: Option<&str>) -> Result<String> {
        let spec = spec.trim();
        if let Some((provider, model)) = spec.split_once('/')
            && self.providers.contains_key(provider)
        {
            if model.is_empty() {
                bail!("model spec `{spec}` names no model");
            }
            return Ok(spec.to_string());
        }
        let provider = default_provider
            .or_else(|| self.default_model.split_once('/').map(|(p, _)| p))
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "`{spec}` does not start with a configured provider ({}); use <provider>/<model>",
                    self.names().join(", ")
                )
            })?;
        Ok(format!("{provider}/{spec}"))
    }

    /// The provider and model for a resolved spec.
    pub fn target<'a>(&'a self, spec: &'a str) -> Result<Target<'a>> {
        let (provider, model) = spec.split_once('/').ok_or_else(|| anyhow!("unresolved model spec `{spec}`"))?;
        let config = self.providers.get(provider).ok_or_else(|| anyhow!("unknown provider `{provider}`"))?;
        Ok(Target { provider, config, model })
    }

    pub fn names(&self) -> Vec<&str> {
        self.providers.keys().map(String::as_str).collect()
    }

    /// Every `provider/model` listed in the config, for `/model`.
    pub fn listed_models(&self) -> Vec<String> {
        self.providers.iter().flat_map(|(p, c)| c.models.keys().map(move |m| format!("{p}/{m}"))).collect()
    }

    pub fn provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.get(name)
    }

    pub fn protocol_of(&self, provider: &str) -> Option<Protocol> {
        self.providers.get(provider).map(|p| p.protocol)
    }
}

impl ProviderConfig {
    fn implicit(protocol: Protocol, base_url: String, key: String, auth: AuthStyle) -> Self {
        Self {
            protocol,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: Some(key),
            auth: Some(auth),
            headers: BTreeMap::new(),
            max_tokens: None,
            extra_body: None,
            fallbacks: None,
            eager_input_streaming: None,
            prompt_cache: None,
            models: BTreeMap::new(),
        }
    }

    fn is_official_anthropic(&self) -> bool {
        let host = self.base_url.split("://").nth(1).unwrap_or(&self.base_url);
        host.split(['/', ':']).next() == Some(OFFICIAL_ANTHROPIC_HOST)
    }
}

impl Target<'_> {
    pub fn spec(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }

    fn model_config(&self) -> Option<&ModelConfig> {
        self.config.models.get(self.model)
    }

    pub fn max_tokens(&self) -> u32 {
        self.model_config()
            .and_then(|m| m.max_tokens)
            .or(self.config.max_tokens)
            .or_else(|| std::env::var("HARNESS_MAX_TOKENS").ok().and_then(|v| v.parse().ok()))
            .unwrap_or(DEFAULT_MAX_TOKENS)
    }

    /// Applies the provider's, then the model's, `extra_body` to `body`.
    pub fn apply_extra_body(&self, body: &mut Value) {
        for extra in [self.config.extra_body.as_ref(), self.model_config().and_then(|m| m.extra_body.as_ref())]
            .into_iter()
            .flatten()
        {
            merge_json(body, &Value::Object(extra.clone()));
        }
    }

    pub fn fallbacks(&self) -> bool {
        self.config.fallbacks.unwrap_or_else(|| self.config.is_official_anthropic())
    }

    pub fn eager_input_streaming(&self) -> bool {
        self.config.eager_input_streaming.unwrap_or_else(|| self.config.is_official_anthropic())
    }

    pub fn prompt_cache(&self) -> bool {
        self.config.prompt_cache.unwrap_or(true)
    }

    /// `{base_url}/v1/{path}`, not doubling a `/v1` already in `base_url`.
    pub fn url(&self, path: &str) -> String {
        let base = &self.config.base_url;
        if base.ends_with("/v1") { format!("{base}/{path}") } else { format!("{base}/v1/{path}") }
    }

    /// Authentication and custom headers.
    pub fn headers(&self) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        if let Some(key) = &self.config.api_key {
            let default = match self.config.protocol {
                Protocol::Anthropic => AuthStyle::XApiKey,
                Protocol::OpenAiResponses => AuthStyle::Bearer,
            };
            match self.config.auth.unwrap_or(default) {
                AuthStyle::XApiKey => headers.push(("x-api-key".into(), key.clone())),
                AuthStyle::Bearer => headers.push(("authorization".into(), format!("Bearer {key}"))),
            }
        }
        headers.extend(self.config.headers.iter().map(|(k, v)| (k.clone(), v.clone())));
        headers
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn no_env(_: &str) -> Option<String> {
        None
    }

    pub(crate) fn file(v: Value) -> ProvidersFile {
        serde_json::from_value(v).unwrap()
    }

    fn two_relays() -> ProvidersFile {
        file(json!({
            "default_model": "relay-a/claude-opus-5",
            "providers": {
                "relay-a": {"protocol": "anthropic", "base_url": "https://a.example.com/", "api_key": "ka"},
                "relay-b": {
                    "protocol": "openai-responses", "base_url": "https://b.example.com/v1", "api_key": "kb",
                    "headers": {"X-Team": "t"}, "max_tokens": 8000,
                    "extra_body": {"reasoning": {"effort": "medium"}},
                    "models": {"gpt-5": {"max_tokens": 32000, "extra_body": {"reasoning": {"effort": "high"}}}}
                }
            }
        }))
    }

    #[test]
    fn resolves_specs() {
        let p = Providers::from_parts(vec![two_relays()], &no_env).unwrap();
        assert_eq!(p.default_model, "relay-a/claude-opus-5");
        assert_eq!(p.resolve_spec("relay-b/gpt-5", None).unwrap(), "relay-b/gpt-5");
        // No known provider prefix: a model on the default provider, even
        // when the model name itself contains a slash.
        assert_eq!(p.resolve_spec("claude-sonnet-5", None).unwrap(), "relay-a/claude-sonnet-5");
        assert_eq!(p.resolve_spec("openai/gpt-5", Some("relay-b")).unwrap(), "relay-b/openai/gpt-5");
        assert!(p.resolve_spec("relay-b/", None).is_err());
        assert_eq!(p.listed_models(), vec!["relay-b/gpt-5"]);
    }

    #[test]
    fn target_settings() {
        let p = Providers::from_parts(vec![two_relays()], &no_env).unwrap();
        let a = p.target("relay-a/claude-opus-5").unwrap();
        assert_eq!(a.url("messages"), "https://a.example.com/v1/messages");
        assert_eq!(a.headers(), vec![("x-api-key".to_string(), "ka".to_string())]);
        assert!(!a.fallbacks() && !a.eager_input_streaming() && a.prompt_cache(), "gateways default conservative");

        let b = p.target("relay-b/gpt-5").unwrap();
        assert_eq!(b.url("responses"), "https://b.example.com/v1/responses");
        assert_eq!(
            b.headers(),
            vec![("authorization".to_string(), "Bearer kb".to_string()), ("X-Team".to_string(), "t".to_string())]
        );
        assert_eq!(b.max_tokens(), 32000);
        let mut body = json!({"model": "gpt-5"});
        b.apply_extra_body(&mut body);
        assert_eq!(body, json!({"model": "gpt-5", "reasoning": {"effort": "high"}}));
        assert_eq!(p.target("relay-b/other").unwrap().max_tokens(), 8000);
    }

    #[test]
    fn implicit_providers_from_env() {
        let env = |k: &str| match k {
            "ANTHROPIC_API_KEY" => Some("sk-ant".to_string()),
            "OPENAI_API_KEY" => Some("sk-oai".to_string()),
            _ => None,
        };
        let p = Providers::from_parts(vec![], &env).unwrap();
        assert_eq!(p.names(), vec!["anthropic", "openai"]);
        assert_eq!(p.default_model, "anthropic/claude-opus-5");
        let a = p.target(&p.default_model).unwrap();
        assert!(a.fallbacks() && a.eager_input_streaming(), "official API gets its features");
        assert_eq!(p.target("openai/gpt-5").unwrap().url("responses"), "https://api.openai.com/v1/responses");
    }

    #[test]
    fn bare_model_name_uses_sole_or_anthropic_provider() {
        let env = |k: &str| match k {
            "ANTHROPIC_API_KEY" => Some("sk".to_string()),
            "HARNESS_MODEL" => Some("claude-sonnet-5".to_string()),
            _ => None,
        };
        assert_eq!(Providers::from_parts(vec![], &env).unwrap().default_model, "anthropic/claude-sonnet-5");
    }

    #[test]
    fn env_overrides_and_errors() {
        let env = |k: &str| match k {
            "HARNESS_MODEL" => Some("relay-b/gpt-5".to_string()),
            "HARNESS_SUBAGENT_MODEL" => Some("claude-haiku-4-5".to_string()),
            _ => None,
        };
        let p = Providers::from_parts(vec![two_relays()], &env).unwrap();
        assert_eq!(p.default_model, "relay-b/gpt-5");
        assert_eq!(p.subagent_model.as_deref(), Some("relay-b/claude-haiku-4-5"));

        assert!(Providers::from_parts(vec![], &no_env).is_err(), "nothing configured");
        let bad = file(json!({"providers": {"x": {"protocol": "anthropic", "base_url": "u"}}}));
        assert!(Providers::from_parts(vec![bad], &no_env).is_err(), "no default model");
        let unknown: Result<ProvidersFile, _> =
            serde_json::from_value(json!({"providers": {"x": {"protocol": "grpc", "base_url": "u"}}}));
        assert!(unknown.is_err());
    }

    #[test]
    fn later_files_override_earlier() {
        let user = two_relays();
        let project = file(json!({
            "default_model": "relay-b/gpt-5",
            "providers": {"relay-a": {"protocol": "anthropic", "base_url": "https://a2.example.com"}}
        }));
        let p = Providers::from_parts(vec![user, project], &no_env).unwrap();
        assert_eq!(p.default_model, "relay-b/gpt-5");
        assert_eq!(p.target("relay-a/m").unwrap().url("messages"), "https://a2.example.com/v1/messages");
    }
}
