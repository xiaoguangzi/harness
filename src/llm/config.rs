//! Provider (gateway) configuration.
//!
//! The settings file (`settings.rs`) declares the gateways to talk to and
//! which protocol each speaks. Models are addressed as `<provider>/<model>`:
//!
//! ```json
//! {
//!   "model": "relay-a/claude-opus-5",
//!   "subagent_model": "relay-b/gpt-5-mini",
//!   "effort": "xhigh",
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
//! `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` and `OPENAI_API_KEY` add
//! implicit `anthropic` and `openai` providers when no provider of that name
//! is configured.
//!
//! `effort` (also per model, or `$HARNESS_EFFORT`; default `high`) sets how
//! hard the model thinks: Anthropic `output_config.effort`, OpenAI
//! `reasoning.effort`.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::settings::Settings;
use crate::util::{expand_env, merge_json};

const DEFAULT_ANTHROPIC_MODEL: &str = "claude-opus-5";
const DEFAULT_MAX_TOKENS: u32 = 64_000;
const DEFAULT_CONTEXT_WINDOW: u32 = 200_000;
const DEFAULT_EFFORT: Effort = Effort::High;
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

/// Reasoning effort, in Anthropic's levels (Claude Code's `/effort`).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
    Max,
}

impl Effort {
    pub const ALL: [Effort; 5] = [Effort::Low, Effort::Medium, Effort::High, Effort::XHigh, Effort::Max];

    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Self::ALL.into_iter().find(|e| e.as_str() == s.trim().to_ascii_lowercase()).ok_or_else(|| {
            let levels: Vec<&str> = Self::ALL.iter().map(|e| e.as_str()).collect();
            anyhow!("unknown effort `{s}` (one of {})", levels.join(", "))
        })
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
    /// The models' context window, in tokens; the conversation is compacted
    /// before it fills up.
    pub context_window: Option<u32>,
    /// Merged into every request body; a `null` value removes a key.
    pub extra_body: Option<Map<String, Value>>,
    /// Anthropic only: server-side refusal fallbacks. Default: on for the
    /// official API, off for gateways.
    pub fallbacks: Option<bool>,
    /// Anthropic only: unbuffered tool-input streaming. Default as above.
    pub eager_input_streaming: Option<bool>,
    /// Anthropic only: images and PDFs a tool returns stay inside its
    /// `tool_result`, rather than follow the results as blocks of their
    /// own, which gateways that drop them from tool results still pass.
    /// Default as above.
    pub tool_result_media: Option<bool>,
    /// Anthropic only: automatic prompt caching. Default: on.
    pub prompt_cache: Option<bool>,
    /// Anthropic only: how long cached prompts are kept. Default: `5m`.
    pub prompt_cache_ttl: Option<CacheTtl>,
    /// Requests are gzip-compressed. Default: on for the official
    /// Anthropic API, off for gateways, many of which don't accept it.
    pub gzip: Option<bool>,
    /// Anthropic only: many MCP tools are found with the tool search tool
    /// rather than all sent in full. Default: on for the official API, off
    /// for gateways, whose models may not have it.
    pub tool_search: Option<bool>,
    /// Per-model overrides; listed models are offered by `/model`.
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
}

/// How long the Anthropic API keeps a cached prompt prefix after it was
/// last used: writing it costs 1.25 times the input price for 5 minutes,
/// twice that for an hour.
#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize)]
pub enum CacheTtl {
    #[default]
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

impl CacheTtl {
    pub fn duration(self) -> std::time::Duration {
        std::time::Duration::from_secs(match self {
            CacheTtl::FiveMinutes => 5 * 60,
            CacheTtl::OneHour => 60 * 60,
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub max_tokens: Option<u32>,
    pub context_window: Option<u32>,
    pub effort: Option<Effort>,
    pub extra_body: Option<Map<String, Value>>,
    /// What its tokens cost, for `/usage`'s estimate.
    pub price: Option<Price>,
}

/// A model's prices in dollars per million tokens. Without their own,
/// cache reads cost a tenth of the input price and cache writes 1.25 times
/// it (twice it for the one-hour cache), as on the Anthropic API.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

impl Price {
    const fn new(input: f64, output: f64, cache_read: f64) -> Self {
        Self { input, output, cache_read: Some(cache_read), cache_write: None }
    }

    /// What `usage` costs, in dollars, with cache writes for `ttl`.
    pub fn cost(&self, usage: &super::Usage, ttl: Option<CacheTtl>) -> f64 {
        let write_factor = if ttl == Some(CacheTtl::OneHour) { 2.0 } else { 1.25 };
        let (read, write) = (self.cache_read.unwrap_or(self.input / 10.0), self.cache_write.unwrap_or(self.input * write_factor));
        let uncached = usage.input_tokens.saturating_sub(usage.cache_read_tokens + usage.cache_write_tokens);
        let tokens = |n: u64, price: f64| n as f64 * price / 1_000_000.0;
        tokens(uncached, self.input) + tokens(usage.cache_read_tokens, read) + tokens(usage.cache_write_tokens, write)
            + tokens(usage.output_tokens, self.output)
    }
}

#[derive(Debug, Clone)]
pub struct Providers {
    providers: BTreeMap<String, ProviderConfig>,
    /// A resolved `provider/model` spec.
    pub default_model: String,
    /// Spec for subagents without their own model; `None` means the
    /// caller's model.
    pub subagent_model: Option<String>,
    /// Effort for models without their own `effort`.
    pub effort: Effort,
    /// `max_tokens` for providers and models without their own.
    max_tokens: Option<u32>,
    /// `context_window` for providers and models without their own.
    context_window: Option<u32>,
}

/// Everything needed to send one request: a provider plus a model on it.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    pub provider: &'a str,
    pub config: &'a ProviderConfig,
    pub model: &'a str,
    /// The configured effort; a session's `/effort` replaces it.
    pub effort: Effort,
    /// The top-level `max_tokens`, for providers and models without one.
    pub(super) default_max_tokens: Option<u32>,
    /// The top-level `context_window`, likewise.
    pub(super) default_context_window: Option<u32>,
}

impl Providers {
    /// The providers in `settings`, with the environment described in the
    /// module docs.
    pub fn from_settings(settings: &Settings) -> Result<Self> {
        Self::from_parts(settings, &|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
    }

    /// Builds the provider set from settings and an environment lookup.
    pub fn from_parts(settings: &Settings, env: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let mut providers = settings.providers.clone();
        let mut effort = settings.effort;
        let mut max_tokens = settings.max_tokens;
        if let Some(level) = env("HARNESS_EFFORT") {
            effort = Some(Effort::parse(&level).context("HARNESS_EFFORT")?);
        }
        if let Some(n) = env("HARNESS_MAX_TOKENS") {
            max_tokens = Some(n.trim().parse().context("HARNESS_MAX_TOKENS")?);
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
                "no LLM provider configured: set ANTHROPIC_API_KEY (or OPENAI_API_KEY), or add a gateway \
                 with `harness provider add <name>`"
            );
        }

        let mut this = Self {
            providers,
            default_model: String::new(),
            subagent_model: None,
            effort: effort.unwrap_or(DEFAULT_EFFORT),
            max_tokens,
            context_window: settings.context_window,
        };
        let requested = env("HARNESS_MODEL").or(settings.model.clone()).or_else(|| this.fallback_default());
        let requested = requested.ok_or_else(|| {
            anyhow!("no default model: set one with `harness provider default <provider>/<model>`, or HARNESS_MODEL")
        })?;
        // A bare model name (e.g. HARNESS_MODEL=claude-opus-5) goes to the
        // sole provider, or to `anthropic` when there are several.
        let bare_provider = match this.names()[..] {
            [only] => Some(only.to_string()),
            _ => this.providers.contains_key("anthropic").then(|| "anthropic".to_string()),
        };
        this.default_model = this.resolve_spec(&requested, bare_provider.as_deref())?;
        if let Some(spec) = env("HARNESS_SUBAGENT_MODEL").or(settings.subagent_model.clone()) {
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
        let effort = config.models.get(model).and_then(|m| m.effort).unwrap_or(self.effort);
        Ok(Target {
            provider,
            config,
            model,
            effort,
            default_max_tokens: self.max_tokens,
            default_context_window: self.context_window,
        })
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

    /// What tokens of the model `spec` cost, and how long its cache keeps
    /// them, if the price is configured or known.
    pub fn price(&self, spec: &str) -> Option<(Price, Option<CacheTtl>)> {
        let target = self.target(spec).ok()?;
        let price = target.model_config().and_then(|m| m.price).or_else(|| known_price(target.model))?;
        Some((price, target.cache_ttl()))
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
            context_window: None,
            extra_body: None,
            fallbacks: None,
            eager_input_streaming: None,
            tool_result_media: None,
            prompt_cache: None,
            prompt_cache_ttl: None,
            gzip: None,
            tool_search: None,
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
            .or(self.default_max_tokens)
            .unwrap_or(DEFAULT_MAX_TOKENS)
    }

    pub fn context_window(&self) -> u32 {
        self.model_config()
            .and_then(|m| m.context_window)
            .or(self.config.context_window)
            .or(self.default_context_window)
            .or_else(|| known_context_window(self.model))
            .unwrap_or(DEFAULT_CONTEXT_WINDOW)
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

    pub fn tool_result_media(&self) -> bool {
        self.config.tool_result_media.unwrap_or_else(|| self.config.is_official_anthropic())
    }

    pub fn prompt_cache(&self) -> bool {
        self.config.prompt_cache.unwrap_or(true)
    }

    /// How long the prompt cache keeps the conversation, when it is on.
    pub fn cache_ttl(&self) -> Option<CacheTtl> {
        (self.config.protocol == Protocol::Anthropic && self.prompt_cache())
            .then(|| self.config.prompt_cache_ttl.unwrap_or_default())
    }

    pub fn gzip(&self) -> bool {
        self.config.gzip.unwrap_or_else(|| self.config.is_official_anthropic())
    }

    pub fn tool_search(&self) -> bool {
        self.config.protocol == Protocol::Anthropic
            && self.config.tool_search.unwrap_or_else(|| self.config.is_official_anthropic())
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

/// The list price of a well-known model, named as for
/// [`known_context_window`].
fn known_price(model: &str) -> Option<Price> {
    let name = model.rsplit('/').next().unwrap_or(model).to_ascii_lowercase();
    let claude = name.find("claude-").map(|at| &name[at + "claude-".len()..])?;
    const PRICES: [(&str, Price); 11] = [
        ("fable-5-1", Price::new(10.0, 50.0, 0.25)),
        ("mythos-5-1", Price::new(10.0, 50.0, 0.25)),
        ("fable-", Price::new(10.0, 50.0, 1.0)),
        ("opus-5-5", Price::new(4.0, 20.0, 0.2)),
        ("opus-5", Price::new(5.0, 25.0, 0.5)),
        ("opus-4-", Price::new(5.0, 25.0, 0.5)),
        ("sonnet-5", Price::new(2.0, 10.0, 0.2)),
        ("sonnet-4-6", Price::new(3.0, 15.0, 0.3)),
        ("sonnet-4", Price::new(3.0, 15.0, 0.3)),
        ("haiku-5", Price::new(0.1, 0.5, 0.01)),
        ("haiku-4-5", Price::new(1.0, 5.0, 0.1)),
    ];
    PRICES.iter().find(|(prefix, _)| claude.starts_with(prefix)).map(|(_, price)| *price)
}

/// The context window of a well-known model, by its name as the official
/// APIs give it, also behind a gateway's prefix (`anthropic/claude-…`,
/// Bedrock's `us.anthropic.claude-…`) or with a date or tag after it.
fn known_context_window(model: &str) -> Option<u32> {
    let name = model.rsplit('/').next().unwrap_or(model).to_ascii_lowercase();
    let name = name.find("claude-").map_or(name.as_str(), |at| &name[at..]);
    const MILLION: u32 = 1_000_000;
    let window = if let Some(claude) = name.strip_prefix("claude-") {
        let one_million = ["opus-4-6", "opus-4-7", "opus-4-8", "opus-5", "sonnet-4-6", "sonnet-5", "haiku-5", "fable-", "mythos-"];
        if one_million.iter().any(|p| claude.starts_with(p)) { MILLION } else { 200_000 }
    } else if name.starts_with("gpt-5") {
        400_000
    } else if name.starts_with("gpt-4.1") {
        1_047_576
    } else if name.starts_with("o3") || name.starts_with("o4-mini") {
        200_000
    } else {
        return None;
    };
    Some(window)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn no_env(_: &str) -> Option<String> {
        None
    }

    pub(crate) fn file(v: Value) -> Settings {
        serde_json::from_value(v).unwrap()
    }

    fn two_relays() -> Settings {
        file(json!({
            "model": "relay-a/claude-opus-5",
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
        let p = Providers::from_parts(&two_relays(), &no_env).unwrap();
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
        let p = Providers::from_parts(&two_relays(), &no_env).unwrap();
        let a = p.target("relay-a/claude-opus-5").unwrap();
        assert_eq!(a.url("messages"), "https://a.example.com/v1/messages");
        assert_eq!(a.headers(), vec![("x-api-key".to_string(), "ka".to_string())]);
        assert!(!a.fallbacks() && !a.eager_input_streaming() && a.prompt_cache(), "gateways default conservative");
        assert!(!a.gzip() && a.cache_ttl() == Some(CacheTtl::FiveMinutes));

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
        assert_eq!(p.target("relay-a/m").unwrap().max_tokens(), DEFAULT_MAX_TOKENS);
        assert_eq!(p.target("relay-a/m").unwrap().context_window(), DEFAULT_CONTEXT_WINDOW);
        assert_eq!(p.target("relay-a/claude-opus-5").unwrap().context_window(), 1_000_000, "a known model");
        assert_eq!(p.target("relay-b/gpt-5").unwrap().context_window(), 400_000);

        let mut s = two_relays();
        s.max_tokens = Some(2000);
        let p = Providers::from_parts(&s, &no_env).unwrap();
        assert_eq!(p.target("relay-a/m").unwrap().max_tokens(), 2000, "the top-level default");
        assert_eq!(p.target("relay-b/other").unwrap().max_tokens(), 8000, "the provider's own wins");
        s.context_window = Some(100_000);
        s.providers.get_mut("relay-b").unwrap().models.get_mut("gpt-5").unwrap().context_window = Some(400_000);
        let p = Providers::from_parts(&s, &no_env).unwrap();
        assert_eq!(p.target("relay-a/m").unwrap().context_window(), 100_000);
        assert_eq!(p.target("relay-b/gpt-5").unwrap().context_window(), 400_000, "the model's own wins");
        let env = |k: &str| (k == "HARNESS_MAX_TOKENS").then(|| "3000".to_string());
        assert_eq!(Providers::from_parts(&s, &env).unwrap().target("relay-a/m").unwrap().max_tokens(), 3000);
    }

    #[test]
    fn implicit_providers_from_env() {
        let env = |k: &str| match k {
            "ANTHROPIC_API_KEY" => Some("sk-ant".to_string()),
            "OPENAI_API_KEY" => Some("sk-oai".to_string()),
            _ => None,
        };
        let p = Providers::from_parts(&Settings::default(), &env).unwrap();
        assert_eq!(p.names(), vec!["anthropic", "openai"]);
        assert_eq!(p.default_model, "anthropic/claude-opus-5");
        let a = p.target(&p.default_model).unwrap();
        assert!(a.fallbacks() && a.eager_input_streaming() && a.gzip(), "official API gets its features");
        assert_eq!(p.target("openai/gpt-5").unwrap().cache_ttl(), None);
        assert_eq!(p.target("openai/gpt-5").unwrap().url("responses"), "https://api.openai.com/v1/responses");
    }

    #[test]
    fn knows_the_context_windows_of_well_known_models() {
        for (model, window) in [
            ("claude-opus-5-5", Some(1_000_000)),
            ("claude-sonnet-4-6", Some(1_000_000)),
            ("us.anthropic.claude-fable-5-1", Some(1_000_000)),
            ("anthropic/claude-haiku-5-5", Some(1_000_000)),
            ("claude-haiku-4-5-20251001", Some(200_000)),
            ("claude-sonnet-4-5", Some(200_000)),
            ("Claude-Opus-4-1", Some(200_000)),
            ("openai/gpt-5-mini", Some(400_000)),
            ("gpt-4.1", Some(1_047_576)),
            ("qwen3-coder", None),
        ] {
            assert_eq!(known_context_window(model), window, "{model}");
        }
    }

    #[test]
    fn prices_known_and_configured_models() {
        let mut s = two_relays();
        s.providers.get_mut("relay-b").unwrap().models.get_mut("gpt-5").unwrap().price =
            Some(Price { input: 1.25, output: 10.0, cache_read: Some(0.125), cache_write: None });
        let p = Providers::from_parts(&s, &no_env).unwrap();
        let usage = crate::llm::Usage { input_tokens: 2_000_000, cache_read_tokens: 1_000_000, cache_write_tokens: 400_000, output_tokens: 100_000 };
        let (opus, ttl) = p.price("relay-a/claude-opus-5").unwrap();
        // 600k uncached at $5, 1M read at $0.50, 400k written at $6.25, 100k out at $25.
        assert!((opus.cost(&usage, ttl) - (3.0 + 0.5 + 2.5 + 2.5)).abs() < 1e-9);
        assert!((opus.cost(&usage, Some(CacheTtl::OneHour)) - (3.0 + 0.5 + 4.0 + 2.5)).abs() < 1e-9);
        assert_eq!(p.price("relay-b/gpt-5").unwrap().0.input, 1.25, "the configured price");
        assert!(p.price("relay-b/gpt-5-mini").is_none() && p.price("relay-a/qwen3").is_none());
        assert_eq!(known_price("claude-opus-5-5").unwrap().input, 4.0, "a newer model before its family");
        assert_eq!(known_price("claude-haiku-4-5-20251001").unwrap().input, 1.0);
    }

    #[test]
    fn bare_model_name_uses_sole_or_anthropic_provider() {
        let env = |k: &str| match k {
            "ANTHROPIC_API_KEY" => Some("sk".to_string()),
            "HARNESS_MODEL" => Some("claude-sonnet-5".to_string()),
            _ => None,
        };
        assert_eq!(Providers::from_parts(&Settings::default(), &env).unwrap().default_model, "anthropic/claude-sonnet-5");
    }

    #[test]
    fn env_overrides_and_errors() {
        let env = |k: &str| match k {
            "HARNESS_MODEL" => Some("relay-b/gpt-5".to_string()),
            "HARNESS_SUBAGENT_MODEL" => Some("claude-haiku-4-5".to_string()),
            _ => None,
        };
        let p = Providers::from_parts(&two_relays(), &env).unwrap();
        assert_eq!(p.default_model, "relay-b/gpt-5");
        assert_eq!(p.subagent_model.as_deref(), Some("relay-b/claude-haiku-4-5"));

        assert!(Providers::from_parts(&Settings::default(), &no_env).is_err(), "nothing configured");
        let bad = file(json!({"providers": {"x": {"protocol": "anthropic", "base_url": "u"}}}));
        assert!(Providers::from_parts(&bad, &no_env).is_err(), "no default model");
        let unknown: Result<Settings, _> =
            serde_json::from_value(json!({"providers": {"x": {"protocol": "grpc", "base_url": "u"}}}));
        assert!(unknown.is_err());
    }

    #[test]
    fn effort_precedence() {
        let mut f = two_relays();
        f.effort = Some(Effort::Medium);
        f.providers.get_mut("relay-b").unwrap().models.get_mut("gpt-5").unwrap().effort = Some(Effort::Low);
        let p = Providers::from_parts(&f, &no_env).unwrap();
        assert_eq!(p.target("relay-a/claude-opus-5").unwrap().effort, Effort::Medium);
        assert_eq!(p.target("relay-b/gpt-5").unwrap().effort, Effort::Low, "the model's own wins");

        let env = |k: &str| (k == "HARNESS_EFFORT").then(|| "max".to_string());
        let p = Providers::from_parts(&two_relays(), &env).unwrap();
        assert_eq!(p.target("relay-a/m").unwrap().effort, Effort::Max);
        let bad = |k: &str| (k == "HARNESS_EFFORT").then(|| "huge".to_string());
        assert!(Providers::from_parts(&two_relays(), &bad).is_err());
        let p = Providers::from_parts(&two_relays(), &no_env).unwrap();
        assert_eq!(p.target("relay-a/m").unwrap().effort, Effort::High, "the default");
    }
}
