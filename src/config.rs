use crate::settings::{GuiSettings, DEFAULT_PORT};
use anyhow::{bail, Result};
use reqwest::Url;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Values parsed from the `.env` file, loaded once per process.
///
/// Deliberately *not* pushed into the process environment. `dotenvy::dotenv()`
/// exists to call `std::env::set_var`, which is unsound to run while other
/// threads read the environment — and `Config::from_settings` runs on a tokio
/// worker on every proxy (re)start, i.e. on every settings save, while request
/// handlers concurrently read env (`PROXY_TZ_OFFSET_HOURS`, the upstream keys).
/// Parsing the file into a map keeps the documented `.env` behaviour without
/// mutating shared process state.
fn dotenv_values() -> &'static std::collections::HashMap<String, String> {
    static VALUES: OnceLock<std::collections::HashMap<String, String>> = OnceLock::new();
    VALUES.get_or_init(load_dotenv_file)
}

/// Parse a `.env` file's contents into a map, first occurrence winning.
///
/// Split from [`load_dotenv_file`] so the parsing rules can be asserted without
/// depending on which `.env` happens to exist on the machine.
fn parse_dotenv(path: &std::path::Path) -> Option<std::collections::HashMap<String, String>> {
    let iter = dotenvy::from_path_iter(path).ok()?;
    Some(
        iter.flatten()
            .fold(std::collections::HashMap::new(), |mut acc, (k, v)| {
                acc.entry(k).or_insert(v);
                acc
            }),
    )
}

/// Read the first `.env` that exists, in the documented search order.
///
/// Uses `from_path_iter`, whose iterator only *parses* — the mutating
/// `set_var` calls live in the separate `load`/`load_override` methods, which
/// this never calls.
fn load_dotenv_file() -> std::collections::HashMap<String, String> {
    let mut candidates = vec![PathBuf::from(".env")];
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(&home).join(".proxy-rs").join(".env"));
        candidates.push(PathBuf::from(&home).join(".proxy-rs.env"));
        candidates.push(PathBuf::from(home).join(".anthropic-proxy.env"));
    }
    candidates.push(PathBuf::from("/etc/proxy-rs/.env"));
    candidates.push(PathBuf::from("/etc/anthropic-proxy/.env"));

    candidates
        .iter()
        .find_map(|path| path.exists().then(|| parse_dotenv(path)).flatten())
        .unwrap_or_default()
}

/// Look up a configuration value: the `.env` file first, then the real process
/// environment.
///
/// `.env` wins because it is the more specific, project-local setting, and
/// because it is the documented override for `gui-settings.json`. The process
/// environment is still consulted, so a variable exported by the shell or set
/// by launchd keeps working.
///
/// Scope: this covers the configuration keys `from_settings` reads. Readers
/// outside this module that go straight to `std::env` — `PROXY_TZ_OFFSET_HOURS`,
/// `CODEX_HOME` — now see only the real process environment. Previously a
/// `.env` value reached them as a side effect of `set_var`, but only after the
/// first proxy start had run `load_dotenv`, so it was never dependable; those
/// are system-level settings and belong in the environment or a settings file.
fn env_lookup(name: &str) -> Option<String> {
    dotenv_values()
        .get(name)
        .cloned()
        .or_else(|| std::env::var(name).ok())
}

/// Trimmed, non-empty configuration value, if set.
fn non_empty_env(name: &str) -> Option<String> {
    env_lookup(name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Truthy configuration flag (`1` or `true`, case-insensitive).
fn env_flag(name: &str) -> bool {
    env_lookup(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Environment variable parsed as a port number.
fn env_u16(name: &str) -> Option<u16> {
    non_empty_env(name).and_then(|v| v.parse().ok())
}

/// How a provider exposes its model catalog.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ModelsFlavor {
    /// Standard OpenAI `GET {base}/v1/models` returning `{"data":[{"id":…}]}`.
    #[default]
    OpenAI,
    /// Vendor-specific config endpoint (WorkBuddy `/v3/config`) whose models
    /// must be filtered by `agents[name=cli].models`.
    WorkBuddyConfig,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub bind: String,
    pub upstream_urls: Vec<String>,
    pub api_key: Option<String>,
    pub passthrough_api_key: bool,
    pub model_map: BTreeMap<String, String>,
    pub system_prompt_ignore_terms: Vec<String>,
    pub reasoning_model: Option<String>,
    pub completion_model: Option<String>,
    pub debug: bool,
    pub verbose: bool,
    /// Vendor models endpoint, e.g. `https://copilot.tencent.com/v3/config`.
    pub models_config_url: Option<String>,
    pub models_flavor: ModelsFlavor,
    /// When true (WorkBuddy/CodeBuddy flavor), neutralize upstream content-filter
    /// fingerprints across every outbound message field. See pipeline.rs.
    pub sanitize_fingerprints: bool,
    /// When true, a non-streaming client request is served by requesting a
    /// stream from the upstream and aggregating it back into one response.
    ///
    /// WorkBuddy rejects non-streaming bodies outright (`11101 Non-stream chat
    /// request is currently not supported`), so a `/v1/messages` call without
    /// `stream:true` would always 502 without this.
    pub force_stream_upstream: bool,
    /// Optional gateway wallet-balance endpoint for `GET /v1/credits`.
    /// When unset, the WorkBuddy flavor auto-uses its billing resource endpoint.
    pub credits_endpoint: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 3456,
            bind: "0.0.0.0".to_string(),
            upstream_urls: vec!["http://localhost:11434".to_string()],
            api_key: None,
            passthrough_api_key: false,
            model_map: BTreeMap::new(),
            system_prompt_ignore_terms: Vec::new(),
            reasoning_model: None,
            completion_model: None,
            debug: false,
            verbose: false,
            models_config_url: None,
            models_flavor: ModelsFlavor::OpenAI,
            credits_endpoint: None,
            sanitize_fingerprints: false,
            force_stream_upstream: false,
        }
    }
}

impl Config {
    /// Build the proxy configuration from persisted settings.
    ///
    /// `gui-settings.json` is the source of truth for the upstream endpoint,
    /// credentials, bind address and model overrides. Environment variables and
    /// a `.env` file may then override any of those fields, so an app launched
    /// from a configured shell (or an existing `.env` setup) keeps behaving as
    /// documented. This is the single construction path for the app.
    pub fn from_settings(settings: &GuiSettings) -> Result<Self> {
        let chat_url = settings.chat_url(&crate::providers::builtin_presets());
        let upstream_urls = Self::parse_upstream_urls(&chat_url)?;

        let preset = settings.models_preset();
        let models_flavor = if preset.models_config_url.is_some() {
            ModelsFlavor::WorkBuddyConfig
        } else {
            ModelsFlavor::OpenAI
        };
        let sanitize_fingerprints = models_flavor == ModelsFlavor::WorkBuddyConfig;
        // Providers that only serve streaming bodies declare it in their preset
        // (`force_stream`), or the user turns it on per-provider in the GUI.
        // `PROXY_FORCE_STREAM` / `ANTHROPIC_PROXY_FORCE_STREAM` still wins, so it remains overridable
        // for testing other providers without touching the settings file.
        let force_stream_upstream = match env_lookup("PROXY_FORCE_STREAM")
            .or_else(|| env_lookup("ANTHROPIC_PROXY_FORCE_STREAM"))
        {
            Some(v) => v == "1" || v.eq_ignore_ascii_case("true"),
            None => settings.force_stream(&crate::providers::builtin_presets()),
        };

        let mut model_map = if settings.model_map.trim().is_empty() {
            BTreeMap::new()
        } else {
            Self::parse_model_map(&settings.model_map)?
        };
        if let Some(raw_map) =
            non_empty_env("PROXY_MODEL_MAP").or_else(|| non_empty_env("ANTHROPIC_PROXY_MODEL_MAP"))
        {
            model_map.extend(Self::parse_model_map(&raw_map)?);
        }

        let mut config = Config {
            port: env_u16("PORT").unwrap_or(if settings.port == 0 {
                DEFAULT_PORT
            } else {
                settings.port
            }),
            bind: non_empty_env("PROXY_BIND")
                .or_else(|| non_empty_env("ANTHROPIC_PROXY_BIND"))
                .unwrap_or_else(|| {
                    if settings.bind.trim().is_empty() {
                        "127.0.0.1".to_string()
                    } else {
                        settings.bind.trim().to_string()
                    }
                }),
            upstream_urls,
            api_key: Some(settings.api_key.trim().to_string()).filter(|k| !k.is_empty()),
            reasoning_model: Some(settings.reasoning_model.trim().to_string())
                .filter(|m| !m.is_empty()),
            completion_model: Some(settings.completion_model.trim().to_string())
                .filter(|m| !m.is_empty()),
            model_map,
            models_config_url: preset.models_config_url,
            models_flavor,
            sanitize_fingerprints,
            force_stream_upstream,
            ..Default::default()
        };

        // GUI-exposed fingerprint word list (semicolon/newline separated). Mirrors
        // the ANTHROPIC_PROXY_SYSTEM_PROMPT_IGNORE_TERMS env var but editable from
        // the console without a restart. Both the system-prompt scrub and the
        // full-message sanitizer consume this list.
        let gui_terms = Self::parse_system_prompt_ignore_terms(&settings.sanitize_terms);
        if !gui_terms.is_empty() {
            config.system_prompt_ignore_terms = gui_terms;
        }

        config.apply_env_overrides()?;
        Ok(config)
    }

    /// Apply the documented environment variables on top of the settings.
    fn apply_env_overrides(&mut self) -> Result<()> {
        if let Some(raw_urls) = non_empty_env("UPSTREAM_BASE_URL")
            .or_else(|| non_empty_env("PROXY_BASE_URL"))
            .or_else(|| non_empty_env("ANTHROPIC_PROXY_BASE_URL"))
        {
            self.upstream_urls = Self::parse_upstream_urls(&raw_urls)?;
        }

        if let Some(key) =
            non_empty_env("UPSTREAM_API_KEY").or_else(|| non_empty_env("OPENROUTER_API_KEY"))
        {
            self.api_key = Some(key);
        }
        if let Some(model) = non_empty_env("REASONING_MODEL") {
            self.reasoning_model = Some(model);
        }
        if let Some(model) = non_empty_env("COMPLETION_MODEL") {
            self.completion_model = Some(model);
        }
        if let Some(endpoint) = non_empty_env("CREDITS_API_ENDPOINT") {
            self.credits_endpoint = Some(endpoint);
        }

        if let Some(terms) = non_empty_env("PROXY_SYSTEM_PROMPT_IGNORE_TERMS")
            .or_else(|| non_empty_env("ANTHROPIC_PROXY_SYSTEM_PROMPT_IGNORE_TERMS"))
        {
            self.system_prompt_ignore_terms = Self::parse_system_prompt_ignore_terms(&terms);
            Self::dedupe_ignore_terms(&mut self.system_prompt_ignore_terms);
        }

        self.passthrough_api_key = env_flag("UPSTREAM_API_KEY_PASSTHROUGH");
        self.debug = env_flag("DEBUG");
        self.verbose = env_flag("VERBOSE");

        // Passthrough extracts the key per request, so a static key contradicts it.
        if self.passthrough_api_key && self.api_key.is_some() {
            bail!(
                "UPSTREAM_API_KEY_PASSTHROUGH=true cannot be used together with UPSTREAM_API_KEY.\n\
                 When passthrough is enabled, the API key is extracted from each incoming request's x-api-key header.\n\
                 Unset UPSTREAM_API_KEY or set UPSTREAM_API_KEY_PASSTHROUGH=false."
            );
        }

        Ok(())
    }

    pub fn chat_completions_urls(&self) -> Vec<String> {
        self.upstream_urls
            .iter()
            .map(|url| {
                Self::resolve_chat_completions_url(url)
                    .expect("URLs should be validated during configuration loading")
            })
            .collect()
    }

    pub fn models_urls(&self) -> Vec<String> {
        self.upstream_urls
            .iter()
            .map(|url| {
                Self::resolve_models_url(url)
                    .expect("URLs should be validated during configuration loading")
            })
            .collect()
    }

    pub fn parse_upstream_urls(raw: &str) -> Result<Vec<String>> {
        let urls: Vec<String> = raw
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        if urls.is_empty() {
            bail!("UPSTREAM_BASE_URL must not be empty");
        }

        for url in &urls {
            Self::resolve_chat_completions_url(url)?;
        }

        Ok(urls)
    }

    fn resolve_chat_completions_url(base_url: &str) -> Result<String> {
        let (normalized, path_segments) = Self::parse_base_url(base_url)?;

        if Self::is_chat_completions_path(&path_segments) {
            return Ok(normalized.to_string());
        }

        let last_segment = path_segments.last().map(String::as_str);
        if matches!(last_segment, Some("chat") | Some("completions")) {
            bail!(
                "UPSTREAM_BASE_URL must be either a service base URL, a versioned base URL like https://gateway.example.com/v2, or the full .../chat/completions endpoint"
            );
        }

        if last_segment.is_some_and(Self::is_version_segment) {
            return Ok(format!("{}/chat/completions", normalized));
        }

        Ok(format!("{}/v1/chat/completions", normalized))
    }

    fn resolve_models_url(base_url: &str) -> Result<String> {
        let (normalized, path_segments) = Self::parse_base_url(base_url)?;

        if Self::is_chat_completions_path(&path_segments) {
            let base = normalized
                .trim_end_matches("/chat/completions")
                .trim_end_matches('/');
            return Ok(format!("{}/models", base));
        }

        let last_segment = path_segments.last().map(String::as_str);
        if matches!(last_segment, Some("chat") | Some("completions")) {
            bail!(
                "UPSTREAM_BASE_URL must be either a service base URL, a versioned base URL like https://gateway.example.com/v2, or the full .../chat/completions endpoint"
            );
        }

        if last_segment.is_some_and(Self::is_version_segment) {
            return Ok(format!("{}/models", normalized));
        }

        Ok(format!("{}/v1/models", normalized))
    }

    fn parse_base_url(base_url: &str) -> Result<(String, Vec<String>)> {
        let normalized = base_url.trim();

        if normalized.is_empty() {
            bail!("UPSTREAM_BASE_URL must not be empty");
        }

        let parsed = Url::parse(normalized).map_err(|err| {
            anyhow::anyhow!("UPSTREAM_BASE_URL must be a valid http(s) URL: {}", err)
        })?;

        if !matches!(parsed.scheme(), "http" | "https") {
            bail!("UPSTREAM_BASE_URL must use http or https");
        }

        if parsed.query().is_some() || parsed.fragment().is_some() {
            bail!("UPSTREAM_BASE_URL must not include query parameters or fragments");
        }

        let path_segments: Vec<_> = parsed
            .path_segments()
            .map(|segments| {
                segments
                    .filter(|segment| !segment.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        Ok((normalized.trim_end_matches('/').to_string(), path_segments))
    }

    fn is_chat_completions_path(segments: &[String]) -> bool {
        matches!(segments, [.., chat, completions] if chat == "chat" && completions == "completions")
    }

    fn is_version_segment(segment: &str) -> bool {
        let version = segment
            .strip_prefix('v')
            .or_else(|| segment.strip_prefix('V'));

        version
            .is_some_and(|value| !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit()))
    }

    pub fn parse_system_prompt_ignore_terms(value: &str) -> Vec<String> {
        value
            .split([';', '\n'])
            .map(str::trim)
            .filter(|term| !term.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }

    pub fn dedupe_ignore_terms(terms: &mut Vec<String>) {
        let mut deduped = Vec::new();
        let mut seen = Vec::new();
        for term in terms.drain(..) {
            let normalized = term
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_lowercase();
            if !seen.iter().any(|existing: &String| existing == &normalized) {
                seen.push(normalized);
                deduped.push(term);
            }
        }
        *terms = deduped;
    }

    pub fn parse_model_map(value: &str) -> Result<BTreeMap<String, String>> {
        let mut model_map = BTreeMap::new();

        for entry in value
            .split([';', '\n'])
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            let (source, target) = entry.split_once('=').ok_or_else(|| {
                anyhow::anyhow!(
                    "Invalid model map entry '{}'. Expected source=target",
                    entry
                )
            })?;

            let source = source.trim();
            let target = target.trim();

            if source.is_empty() || target.is_empty() {
                bail!(
                    "Invalid model map entry '{}'. Source and target models must be non-empty",
                    entry
                );
            }

            model_map.insert(source.to_string(), target.to_string());
        }

        Ok(model_map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `contents` to a uniquely-named temp file for a test.
    fn temp_env(name: &str, contents: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("proxy-rs-env-{}-{}", std::process::id(), name));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn dotenv_values_are_parsed_without_touching_the_process_environment() {
        // The point of the parser: `dotenvy::dotenv()` would `set_var` these,
        // which is unsound while request handlers read the environment.
        let path = temp_env(
            "parse",
            "UPSTREAM_BASE_URL=https://example.test\nVERBOSE=1\n",
        );
        let vars = parse_dotenv(&path).expect("parses");
        assert_eq!(vars["UPSTREAM_BASE_URL"], "https://example.test");
        assert_eq!(vars["VERBOSE"], "1");

        assert!(
            std::env::var("UPSTREAM_BASE_URL").is_err(),
            "parsing must not leak into the process environment"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_repeated_key_keeps_its_first_value() {
        // Matches dotenv semantics: the first occurrence wins.
        let path = temp_env("dupe", "K=first\nK=second\n");
        let vars = parse_dotenv(&path).expect("parses");
        assert_eq!(vars["K"], "first");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn comments_and_quotes_are_handled_by_the_parser() {
        let path = temp_env(
            "quotes",
            "# a comment\nQUOTED=\"has spaces\"\nPLAIN=bare\nexport EXPORTED=yes\n",
        );
        let vars = parse_dotenv(&path).expect("parses");
        assert_eq!(vars["QUOTED"], "has spaces");
        assert_eq!(vars["PLAIN"], "bare");
        assert_eq!(vars["EXPORTED"], "yes");
        assert!(!vars.contains_key("# a comment"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_missing_file_is_empty_rather_than_an_error() {
        let missing = std::env::temp_dir().join("proxy-rs-env-does-not-exist");
        assert!(parse_dotenv(&missing).is_none());
    }

    #[test]
    fn base_url_without_version_defaults_to_v1_endpoint() {
        let url = Config::resolve_chat_completions_url("https://api.openai.com").unwrap();
        assert_eq!(url, "https://api.openai.com/v1/chat/completions");
    }

    #[test]
    fn versioned_base_url_preserves_existing_version() {
        let url = Config::resolve_chat_completions_url("https://gateway.example.com/v2").unwrap();
        assert_eq!(url, "https://gateway.example.com/v2/chat/completions");
    }

    #[test]
    fn full_chat_completions_endpoint_is_used_as_is() {
        let url = Config::resolve_chat_completions_url(
            "https://gateway.example.com/v2/chat/completions/",
        )
        .unwrap();
        assert_eq!(url, "https://gateway.example.com/v2/chat/completions");
    }

    #[test]
    fn models_url_without_version_defaults_to_v1_endpoint() {
        let url = Config::resolve_models_url("https://api.openai.com").unwrap();
        assert_eq!(url, "https://api.openai.com/v1/models");
    }

    #[test]
    fn versioned_models_url_preserves_existing_version() {
        let url = Config::resolve_models_url("https://gateway.example.com/v2").unwrap();
        assert_eq!(url, "https://gateway.example.com/v2/models");
    }

    #[test]
    fn full_chat_completions_endpoint_resolves_models_url() {
        let url =
            Config::resolve_models_url("https://gateway.example.com/v2/chat/completions").unwrap();
        assert_eq!(url, "https://gateway.example.com/v2/models");
    }

    #[test]
    fn partial_chat_path_is_rejected() {
        let err = Config::resolve_chat_completions_url("https://gateway.example.com/v2/chat")
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("service base URL, a versioned base URL"));
    }

    #[test]
    fn query_strings_are_rejected() {
        let err = Config::resolve_chat_completions_url("https://gateway.example.com/v2?foo=bar")
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("must not include query parameters or fragments"));
    }

    #[test]
    fn fragments_are_rejected() {
        let err = Config::resolve_chat_completions_url("https://gateway.example.com/v2#section")
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("must not include query parameters or fragments"));
    }

    #[test]
    fn empty_url_is_rejected() {
        let err = Config::resolve_chat_completions_url("").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn non_http_scheme_is_rejected() {
        let err = Config::resolve_chat_completions_url("ftp://gateway.example.com").unwrap_err();
        assert!(err.to_string().contains("must use http or https"));
    }

    #[test]
    fn explicit_v1_is_preserved_not_doubled() {
        let url = Config::resolve_chat_completions_url("https://openrouter.ai/api/v1").unwrap();
        assert_eq!(url, "https://openrouter.ai/api/v1/chat/completions");
    }

    #[test]
    fn trailing_slash_on_base_url_is_normalized() {
        let url = Config::resolve_chat_completions_url("https://api.openai.com/").unwrap();
        assert_eq!(url, "https://api.openai.com/v1/chat/completions");
    }

    #[test]
    fn models_url_from_explicit_v1() {
        let url = Config::resolve_models_url("https://openrouter.ai/api/v1").unwrap();
        assert_eq!(url, "https://openrouter.ai/api/v1/models");
    }

    #[test]
    fn models_url_with_trailing_slash() {
        let url = Config::resolve_models_url("https://api.openai.com/").unwrap();
        assert_eq!(url, "https://api.openai.com/v1/models");
    }

    #[test]
    fn url_with_subpath_and_no_version_defaults_to_v1() {
        let url = Config::resolve_chat_completions_url("https://openrouter.ai/api").unwrap();
        assert_eq!(url, "https://openrouter.ai/api/v1/chat/completions");
    }

    #[test]
    fn only_completions_path_is_rejected() {
        let err =
            Config::resolve_chat_completions_url("https://gateway.example.com/v2/completions")
                .unwrap_err();
        assert!(err
            .to_string()
            .contains("service base URL, a versioned base URL"));
    }

    #[test]
    fn uppercase_version_prefix_is_accepted() {
        let url = Config::resolve_chat_completions_url("https://gateway.example.com/V2").unwrap();
        assert_eq!(url, "https://gateway.example.com/V2/chat/completions");
    }

    #[test]
    fn parse_system_prompt_ignore_terms_supports_semicolons_and_newlines() {
        let terms =
            Config::parse_system_prompt_ignore_terms("rm -rf;git reset --hard\nsudo rm -rf");

        assert_eq!(
            terms,
            vec![
                "rm -rf".to_string(),
                "git reset --hard".to_string(),
                "sudo rm -rf".to_string()
            ]
        );
    }

    #[test]
    fn dedupe_ignore_terms_normalizes_case_and_whitespace() {
        let mut terms = vec![
            "rm -rf".to_string(),
            " RM\t-rF ".to_string(),
            "git reset --hard".to_string(),
        ];

        Config::dedupe_ignore_terms(&mut terms);

        assert_eq!(
            terms,
            vec!["rm -rf".to_string(), "git reset --hard".to_string()]
        );
    }

    #[test]
    fn parse_model_map_supports_semicolons_and_newlines() {
        let model_map = Config::parse_model_map(
            "claude-3-5-sonnet=openai/gpt-5.2-chat\nclaude-haiku=openai/gpt-4.1-mini",
        )
        .unwrap();

        assert_eq!(
            model_map.get("claude-3-5-sonnet"),
            Some(&"openai/gpt-5.2-chat".to_string())
        );
        assert_eq!(
            model_map.get("claude-haiku"),
            Some(&"openai/gpt-4.1-mini".to_string())
        );
    }

    #[test]
    fn parse_model_map_rejects_invalid_entries() {
        let err = Config::parse_model_map("claude-3-5-sonnet").unwrap_err();

        assert!(err.to_string().contains("Expected source=target"));
    }

    #[test]
    fn parse_upstream_urls_splits_on_semicolons() {
        let urls = Config::parse_upstream_urls("https://openrouter.ai/api;https://api.openai.com")
            .unwrap();

        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0], "https://openrouter.ai/api");
        assert_eq!(urls[1], "https://api.openai.com");
    }

    #[test]
    fn parse_upstream_urls_single_url_still_works() {
        let urls = Config::parse_upstream_urls("https://api.openai.com").unwrap();
        assert_eq!(urls.len(), 1);
    }

    #[test]
    fn parse_upstream_urls_rejects_empty() {
        let err = Config::parse_upstream_urls("").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn parse_upstream_urls_validates_each_url() {
        let err = Config::parse_upstream_urls("https://api.openai.com;not-a-url").unwrap_err();
        assert!(err.to_string().contains("valid http"));
    }

    #[test]
    fn chat_completions_urls_resolves_all() {
        let config = Config {
            upstream_urls: vec![
                "https://openrouter.ai/api".to_string(),
                "https://api.openai.com".to_string(),
            ],
            ..Default::default()
        };

        let urls = config.chat_completions_urls();
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0], "https://openrouter.ai/api/v1/chat/completions");
        assert_eq!(urls[1], "https://api.openai.com/v1/chat/completions");
    }

    #[test]
    fn passthrough_api_key_defaults_to_false() {
        let config = Config::default();
        assert!(!config.passthrough_api_key);
    }

    #[test]
    fn passthrough_disabled_with_static_key_works() {
        let config = Config {
            api_key: Some("sk-test".to_string()),
            passthrough_api_key: false,
            ..Default::default()
        };
        assert!(!config.passthrough_api_key);
        assert_eq!(config.api_key, Some("sk-test".to_string()));
    }

    #[test]
    fn passthrough_enabled_with_no_static_key() {
        let config = Config {
            api_key: None,
            passthrough_api_key: true,
            ..Default::default()
        };
        assert!(config.passthrough_api_key);
        assert!(config.api_key.is_none());
    }

    #[test]
    fn bind_defaults_to_zero_zero_zero_zero() {
        let config = Config::default();
        assert_eq!(config.bind, "0.0.0.0");
    }

    #[test]
    fn bind_accepts_loopback() {
        let config = Config {
            bind: "127.0.0.1".to_string(),
            ..Default::default()
        };
        assert_eq!(config.bind, "127.0.0.1");
    }
}
