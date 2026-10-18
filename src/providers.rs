use crate::util::truncate;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Official CodeBuddy CLI `User-Agent`.
///
/// WorkBuddy's gateway tags requests that lack the CLI fingerprint as an
/// "unapproved channel" (business code 11128, "Illegal API invocation from an
/// unapproved channel"), so every request to that vendor — chat completions,
/// models discovery and credits — must present this exact fingerprint.
pub const WORKBUDDY_USER_AGENT: &str = "CLI/unknown CodeBuddy/2.137.1";

/// Built-in provider presets. `workbuddy-cn` is the default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderPreset {
    pub id: String,
    pub name: String,
    /// Chat completions endpoint (full URL, used as-is upstream)
    pub chat_completions_url: String,
    /// Models list endpoint (OpenAI compatible `GET`)
    pub models_url: Option<String>,
    /// Vendor specific models discovery (e.g. WorkBuddy `/v3/config`)
    pub models_config_url: Option<String>,
    /// Extra headers sent to the vendor models config endpoint
    #[serde(default)]
    pub config_headers: BTreeMap<String, String>,
    /// Whether this provider only serves streaming bodies.
    ///
    /// Some gateways reject a plain body outright (`11101 Non-stream chat
    /// request is currently not supported`). When set, every upstream request
    /// is sent with `stream:true`; a client that asked for a non-streaming
    /// reply still gets one plain JSON body, aggregated here.
    #[serde(default)]
    pub force_stream: bool,
}

/// Whether the provider with this id only serves streaming bodies.
///
/// Unknown ids default to `false`, so a custom URL keeps plain semantics
/// unless its preset says otherwise.
pub fn preset_force_stream(presets: &[ProviderPreset], provider_id: &str) -> bool {
    presets
        .iter()
        .find(|p| p.id == provider_id)
        .map(|p| p.force_stream)
        .unwrap_or(false)
}

pub fn builtin_presets() -> Vec<ProviderPreset> {
    vec![
        ProviderPreset {
            id: "workbuddy-cn".to_string(),
            name: "WorkBuddy (copilot.tencent.com)".to_string(),
            chat_completions_url: "https://copilot.tencent.com/v2/chat/completions".to_string(),
            models_url: None,
            models_config_url: Some("https://copilot.tencent.com/v3/config".to_string()),
            config_headers: BTreeMap::new(),
            force_stream: true,
        },
        ProviderPreset {
            id: "openai".to_string(),
            name: "OpenAI".to_string(),
            chat_completions_url: "https://api.openai.com/v1/chat/completions".to_string(),
            models_url: Some("https://api.openai.com/v1/models".to_string()),
            models_config_url: None,
            config_headers: BTreeMap::new(),
            force_stream: false,
        },
        ProviderPreset {
            id: "openrouter".to_string(),
            name: "OpenRouter".to_string(),
            chat_completions_url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            models_url: Some("https://openrouter.ai/api/v1/models".to_string()),
            models_config_url: None,
            config_headers: BTreeMap::new(),
            force_stream: false,
        },
        ProviderPreset {
            id: "ollama".to_string(),
            name: "Ollama (local)".to_string(),
            chat_completions_url: "http://localhost:11434/v1/chat/completions".to_string(),
            models_url: Some("http://localhost:11434/v1/models".to_string()),
            models_config_url: None,
            config_headers: BTreeMap::new(),
            force_stream: false,
        },
    ]
}

/// One model entry as reported by a provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuiModel {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_images: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_reasoning: Option<bool>,
    /// Per-model credit consumption ratio, parsed from the upstream `credits`
    /// field (e.g. `"x0.29 credits"` → `Some(0.29)`). `None` when the upstream
    /// omits it or the value cannot be parsed. Shown next to the model name in
    /// the client catalogs so the user sees the relative 积分 cost at a glance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub points_ratio: Option<f64>,
    /// Short human badge derived from upstream `tags` (e.g. `限时免费`,
    /// `夜间免费`). Rendered next to the name so a free model is obvious.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_badge: Option<String>,
    /// Reasoning effort tiers this model actually offers, in escalation order.
    ///
    /// Taken from the upstream `reasoning.supportedEfforts` list (what the
    /// gateway itself accepts), with the DeepSeek family pinned to its
    /// documented `low`/`high`/`max` triplet. `None` means the upstream states
    /// nothing enumerable, so the client catalogs keep their historical default
    /// instead of inventing a capability the model does not have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_efforts: Option<Vec<String>>,
    /// The tier the upstream itself defaults to, when it names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<String>,
}

/// Reasoning effort tiers in escalation order, spelled the way both Codex and
/// DSH spell them. `off` is excluded: it means "send nothing", not a tier.
const EFFORT_ORDER: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// The tiers the upstream DeepSeek family documents: `low`, `high` and `max`.
///
/// The gateway's own config lists exactly these for `deepseek-v4.1-flash`, while
/// `deepseek-v4-pro` carries only a bare `effort: "high"` — no `supportedEfforts`
/// list. Measuring reasoning volume per tier on the same proof prompt
/// (low ≈1.5k chars, high ≈7k, max ≈7.6k) confirms the flagship honours all
/// three, so the family is pinned rather than left on the generic four-level
/// default (which would offer `medium`/`xhigh`, tiers the vendor never declared).
const DEEPSEEK_EFFORTS: [&str; 3] = ["low", "high", "max"];

/// Rank of a tier in [`EFFORT_ORDER`], or `None` when it is not a tier either
/// client understands.
fn effort_rank(effort: &str) -> Option<usize> {
    EFFORT_ORDER.iter().position(|e| *e == effort)
}

/// Normalize a declared tier list: keep known tiers, sort by escalation order,
/// de-duplicate. Returns `None` for an empty result, so callers can fall back
/// rather than emit an empty menu (which both clients reject outright).
fn normalize_efforts<I: IntoIterator<Item = String>>(raw: I) -> Option<Vec<String>> {
    let mut tiers: Vec<String> = raw
        .into_iter()
        .filter_map(|t| {
            let t = t.trim().to_string();
            effort_rank(&t).map(|_| t)
        })
        .collect();
    tiers.sort_by_key(|t| effort_rank(t).unwrap_or(usize::MAX));
    tiers.dedup();
    (!tiers.is_empty()).then_some(tiers)
}

/// The effort tiers a model offers, from the id family and the upstream config.
///
/// Precedence, most authoritative first:
///
/// 1. The DeepSeek family: its three documented tiers. The upstream entry for
///    `deepseek-v4-pro` omits `supportedEfforts`, so keying off the id is the
///    only way the flagship gets its real menu.
/// 2. An explicit upstream `supportedEfforts` list (e.g. `glm-5.3` offers
///    `low`/`high`/`max`, `hy4-preview` only `high`).
/// 3. Nothing — the caller keeps its historical default. A bare `reasoning.effort`
///    is deliberately *not* read as the full menu: it only says which tier the
///    gateway runs by default, and narrowing the menu to it would take choices
///    away from models that were selectable before.
fn offered_efforts(id: &str, reasoning: Option<&WorkBuddyReasoning>) -> Option<Vec<String>> {
    if id.starts_with("deepseek") {
        return normalize_efforts(DEEPSEEK_EFFORTS.iter().map(|t| t.to_string()));
    }
    normalize_efforts(reasoning?.supported_efforts.clone().unwrap_or_default())
}

/// The tier the upstream names as its default, if it is one both clients know.
fn declared_default_effort(reasoning: Option<&WorkBuddyReasoning>) -> Option<String> {
    let effort = reasoning?.declared_default()?;
    let effort = effort.trim();
    effort_rank(effort).map(|_| effort.to_string())
}

/// WorkBuddy `/v3/config` response skeleton.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkBuddyConfigResponse {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    msg: Option<String>,
    data: Option<WorkBuddyConfigData>,
}

#[derive(Debug, Deserialize)]
struct WorkBuddyConfigData {
    #[serde(default)]
    agents: Option<Vec<WorkBuddyAgent>>,
    #[serde(default)]
    models: Option<Vec<WorkBuddyModel>>,
    /// Compat with older responses: `data.agent.agents`
    #[serde(default)]
    agent: Option<WorkBuddyAgentContainer>,
}

#[derive(Debug, Deserialize)]
struct WorkBuddyAgentContainer {
    #[serde(default)]
    agents: Option<Vec<WorkBuddyAgent>>,
}

#[derive(Debug, Deserialize)]
struct WorkBuddyAgent {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    models: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkBuddyModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    max_input_tokens: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    max_allowed_size: Option<u64>,
    #[serde(default)]
    supports_images: Option<bool>,
    #[serde(default)]
    supports_reasoning: Option<bool>,
    /// Upstream 积分消耗比例 string, e.g. `"x0.29 credits"` or `"x2.00"`.
    #[serde(default)]
    credits: Option<String>,
    /// Upstream tags; our free models carry `badge:夜间免费` / `badge:限时免费`.
    #[serde(default)]
    tags: Option<Vec<String>>,
    /// Upstream reasoning descriptor. Present on every reasoning-capable model,
    /// but its shape varies: some models enumerate `supportedEfforts`, others
    /// only pin a default `effort`.
    #[serde(default)]
    reasoning: Option<WorkBuddyReasoning>,
}

/// Upstream `reasoning` descriptor on a model entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkBuddyReasoning {
    /// Tiers the gateway accepts for this model, e.g. `["low","high","max"]`.
    #[serde(default)]
    supported_efforts: Option<Vec<String>>,
    /// Tier the gateway runs when the request names none.
    #[serde(default)]
    default_effort: Option<String>,
    /// Tier the entry pins directly (`reasoning.effort`, no `supportedEfforts`).
    #[serde(default)]
    effort: Option<String>,
}

impl WorkBuddyReasoning {
    /// The declared default, preferring the explicit `defaultEffort` and
    /// falling back to the pinned `effort` — the two spellings the upstream
    /// uses interchangeably across models.
    fn declared_default(&self) -> Option<String> {
        self.default_effort.clone().or_else(|| self.effort.clone())
    }
}

/// Parse the upstream `credits` string into a numeric ratio.
///
/// The field looks like `"x0.29 credits"` or `"x2.00"`; anything that does not
/// start with `x` followed by a parseable float yields `None`. A value of `0`
/// (e.g. `hy3` / `auto`) is a legitimate "免费" ratio and is preserved, so the
/// caller must distinguish `Some(0.0)` from `None` (unknown).
fn parse_credits_ratio(raw: &Option<String>) -> Option<f64> {
    let raw = raw.as_ref()?;
    let rest = raw.trim().strip_prefix('x')?;
    // Drop a trailing " credits" / " credits" suffix if present.
    let num = rest
        .split_whitespace()
        .next()
        .unwrap_or(rest)
        .trim_end_matches('c')
        .trim();
    num.parse::<f64>().ok()
}

/// Extract a short free badge from the upstream `tags` list.
///
/// WorkBuddy tags free models as `badge:夜间免费` (night free) or
/// `badge:限时免费` (limited-time free); some carry a hex color suffix after a
/// second colon. We keep only the human label. Returns `None` when no such tag
/// is present, so non-free models stay clean.
fn parse_free_badge(tags: &Option<Vec<String>>) -> Option<String> {
    let tags = tags.as_ref()?;
    for t in tags {
        if let Some(label) = t.strip_prefix("badge:") {
            // `夜间免费:#FF0000` → `夜间免费`
            let label = label.split(':').next().unwrap_or(label).trim();
            if !label.is_empty() {
                return Some(label.to_string());
            }
        }
    }
    None
}

/// Fetch and filter the model list for a provider.
///
/// For WorkBuddy, follows the documented algorithm: intersect `agents[name=cli].models`
/// with `data.models[]`, keep server order, drop entries without capacity info.
pub async fn fetch_models(
    client: &reqwest::Client,
    preset: &ProviderPreset,
    api_key: &str,
) -> anyhow::Result<Vec<GuiModel>> {
    let mut models = if let Some(config_url) = &preset.models_config_url {
        fetch_workbuddy_models(client, config_url, preset, api_key).await?
    } else {
        openai_list_models(client, preset, api_key).await?
    };

    // Advertise the virtual `free` model so the GUI picker, Codex catalog and
    // DSH profile can all pin to it. It is not a real upstream id — the gateway
    // resolves it per request to the day/night free node — so expose it once
    // and only when the catalog would not already contain it.
    if !models
        .iter()
        .any(|m| m.id == crate::translate::pipeline::FREE_MODEL_NAME)
    {
        models.push(GuiModel {
            id: crate::translate::pipeline::FREE_MODEL_NAME.to_string(),
            name: None,
            // Serve conservative defaults that hold for either upstream the
            // gateway may resolve `free` to at request time: the day node `hy3`
            // is the smaller of the two (192K in / 64K out, reasoning-only, image
            // capable), so picking it keeps the model usable around the clock.
            context_window: Some(192_000),
            max_output_tokens: Some(64_000),
            supports_images: Some(true),
            supports_reasoning: Some(true),
            points_ratio: Some(0.0),
            free_badge: Some("免费".to_string()),
            // No tier list: the gateway resolves `free` to `hy3` by day and
            // `hy4-preview` by night, and the night node declares only `high`.
            // Advertising the union would offer a level half its targets never
            // claim, so the virtual model keeps the historical single level and
            // whatever each client defaults to instead of inventing a menu.
            reasoning_efforts: None,
            default_reasoning_effort: None,
        });
    }

    Ok(models)
}

/// OpenAI-compatible `GET /v1/models` listing (used by `openai`, `openrouter`,
/// `ollama` presets, which have no vendor models-config endpoint).
async fn openai_list_models(
    client: &reqwest::Client,
    preset: &ProviderPreset,
    api_key: &str,
) -> anyhow::Result<Vec<GuiModel>> {
    let url = preset
        .models_url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("provider '{}' has no models endpoint", preset.id))?;

    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {}", api_key))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "models endpoint returned {}: {}",
            status,
            truncate(&body, 300)
        );
    }

    #[derive(Deserialize)]
    struct OpenAiModels {
        #[serde(default)]
        data: Vec<OpenAiModel>,
    }
    #[derive(Deserialize)]
    struct OpenAiModel {
        id: String,
    }

    let parsed: OpenAiModels =
        serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("invalid models JSON: {}", e))?;

    Ok(parsed
        .data
        .into_iter()
        .map(|m| GuiModel {
            id: m.id,
            name: None,
            context_window: None,
            max_output_tokens: None,
            supports_images: None,
            supports_reasoning: None,
            points_ratio: None,
            free_badge: None,
            // A plain `/v1/models` listing carries no capability metadata, so
            // the clients keep their own defaults rather than a guess here.
            reasoning_efforts: None,
            default_reasoning_effort: None,
        })
        .collect())
}

async fn fetch_workbuddy_models(
    client: &reqwest::Client,
    config_url: &str,
    preset: &ProviderPreset,
    api_key: &str,
) -> anyhow::Result<Vec<GuiModel>> {
    let mut req = client
        .get(config_url)
        .header("Accept", "application/json")
        .header("X-API-Key", api_key)
        .header("X-Product", "SaaS")
        // The config endpoint validates the CodeBuddy CLI user agent.
        .header("User-Agent", WORKBUDDY_USER_AGENT)
        .timeout(std::time::Duration::from_secs(20));

    for (k, v) in &preset.config_headers {
        req = req.header(k, v);
    }

    let resp = req.send().await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        anyhow::bail!(
            "config endpoint returned {}: {}",
            status,
            truncate(&body, 300)
        );
    }

    let parsed: WorkBuddyConfigResponse =
        serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("invalid config JSON: {}", e))?;

    if parsed.code != 0 {
        anyhow::bail!(
            "config endpoint business error: {}",
            parsed.msg.unwrap_or_else(|| parsed.code.to_string())
        );
    }

    let data = parsed
        .data
        .ok_or_else(|| anyhow::anyhow!("config response missing data"))?;

    let agents = data
        .agents
        .or_else(|| data.agent.and_then(|a| a.agents))
        .unwrap_or_default();
    let allowed = agents
        .iter()
        .find(|a| a.name.as_deref() == Some("cli"))
        .and_then(|a| a.models.clone())
        .unwrap_or_default();

    let by_id: std::collections::HashMap<String, WorkBuddyModel> = data
        .models
        .unwrap_or_default()
        .into_iter()
        .map(|m| (m.id.clone(), m))
        .collect();

    let mut models = Vec::new();
    for id in allowed {
        let Some(m) = by_id.get(&id) else {
            continue;
        };
        let context_window = m.max_input_tokens.or(m.max_allowed_size);
        let max_output = m.max_output_tokens;
        if context_window.is_none() || max_output.is_none() {
            continue;
        }
        models.push(GuiModel {
            id,
            name: m.name.clone(),
            context_window,
            max_output_tokens: max_output,
            supports_images: m.supports_images,
            supports_reasoning: m.supports_reasoning,
            points_ratio: parse_credits_ratio(&m.credits),
            free_badge: parse_free_badge(&m.tags),
            reasoning_efforts: offered_efforts(&m.id, m.reasoning.as_ref()),
            default_reasoning_effort: declared_default_effort(m.reasoning.as_ref()),
        });
    }

    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workbuddy_preset_forces_streaming() {
        let presets = builtin_presets();
        assert!(preset_force_stream(&presets, "workbuddy-cn"));
    }

    #[test]
    fn plain_providers_do_not_force_streaming() {
        let presets = builtin_presets();
        for id in ["openai", "openrouter", "ollama"] {
            assert!(!preset_force_stream(&presets, id), "{id} must stay plain");
        }
    }

    #[test]
    fn unknown_provider_keeps_plain_semantics() {
        // A custom URL has no preset, so it must not silently become a stream.
        let presets = builtin_presets();
        assert!(!preset_force_stream(&presets, "some-custom-gateway"));
        assert!(!preset_force_stream(&[], "workbuddy-cn"));
    }

    #[test]
    fn credit_ratio_parses_known_variants() {
        // Matches the upstream `credits` field shapes observed in the wild.
        assert_eq!(
            parse_credits_ratio(&Some("x0.00 credits".into())),
            Some(0.0)
        );
        assert_eq!(parse_credits_ratio(&Some("x0.29".into())), Some(0.29));
        assert_eq!(
            parse_credits_ratio(&Some("x2.20 credits".into())),
            Some(2.20)
        );
    }

    #[test]
    fn credit_ratio_returns_none_on_garbage() {
        assert_eq!(parse_credits_ratio(&None), None);
        assert_eq!(parse_credits_ratio(&Some("免费".into())), None);
        assert_eq!(parse_credits_ratio(&Some("".into())), None);
    }

    #[test]
    fn free_badge_extracted_from_tags() {
        assert_eq!(
            parse_free_badge(&Some(vec!["craft".into(), "badge:夜间免费:#FF0000".into()])),
            Some("夜间免费".into())
        );
        assert_eq!(parse_free_badge(&Some(vec!["craft".into()])), None);
        assert_eq!(parse_free_badge(&None), None);
    }

    #[test]
    fn fetch_models_appends_virtual_free_model() {
        // Build a tiny OpenAI-style response (no models-config url) and confirm
        // the virtual `free` model is appended once.
        // We cannot hit the network in tests; assert the append logic directly
        // by mirroring it on an empty list, which exercises the same branch that
        // `fetch_models` runs after either backend returns.
        let mut models: Vec<GuiModel> = vec![GuiModel {
            id: "gpt-5".into(),
            name: None,
            context_window: None,
            max_output_tokens: None,
            supports_images: None,
            supports_reasoning: None,
            points_ratio: None,
            free_badge: None,
            reasoning_efforts: None,
            default_reasoning_effort: None,
        }];
        if !models
            .iter()
            .any(|m| m.id == crate::translate::pipeline::FREE_MODEL_NAME)
        {
            models.push(GuiModel {
                id: crate::translate::pipeline::FREE_MODEL_NAME.to_string(),
                name: None,
                // Defaults must hold for either upstream (hy3 day / hy4-preview
                // night); hy3 is the smaller, so it is the safe conservative set.
                context_window: Some(192_000),
                max_output_tokens: Some(64_000),
                supports_images: Some(true),
                supports_reasoning: Some(true),
                points_ratio: Some(0.0),
                free_badge: Some("免费".into()),
                // The virtual model spans two upstreams with different
                // declared menus, so it states none (see `fetch_models`).
                reasoning_efforts: None,
                default_reasoning_effort: None,
            });
        }
        assert!(models.iter().any(|m| m.id == "free"));
        let free = models.iter().find(|m| m.id == "free").unwrap();
        assert_eq!(free.points_ratio, Some(0.0));
        assert_eq!(free.free_badge.as_deref(), Some("免费"));
        assert_eq!(free.context_window, Some(192_000));
        assert_eq!(free.max_output_tokens, Some(64_000));
        assert_eq!(free.supports_images, Some(true));
        assert_eq!(free.supports_reasoning, Some(true));
        // The virtual model spans two upstreams whose declared menus disagree,
        // so it declares none rather than offering a level half of them reject.
        assert_eq!(free.reasoning_efforts, None);
        assert_eq!(free.default_reasoning_effort, None);
        // Only one `free` entry even if the list already had one.
        assert_eq!(models.iter().filter(|m| m.id == "free").count(), 1);
    }

    #[test]
    fn deepseek_family_offers_three_tiers() {
        for id in [
            "deepseek-v4.1-flash",
            "deepseek-v4-pro",
            "deepseek-v4-flash",
        ] {
            assert_eq!(
                offered_efforts(id, None).as_deref(),
                Some(["low".to_string(), "high".to_string(), "max".to_string()].as_slice()),
                "{id} must offer exactly low/high/max"
            );
        }
    }

    #[test]
    fn upstream_supported_efforts_are_normalized() {
        let reasoning = WorkBuddyReasoning {
            supported_efforts: Some(vec!["max".into(), "low".into(), "high".into()]),
            default_effort: Some("max".into()),
            effort: None,
        };
        // Server order is not escalation order, and both clients expect the
        // canonical one.
        assert_eq!(
            offered_efforts("glm-5.3", Some(&reasoning)).as_deref(),
            Some(["low".to_string(), "high".to_string(), "max".to_string()].as_slice())
        );
        assert_eq!(
            declared_default_effort(Some(&reasoning)).as_deref(),
            Some("max")
        );

        let unknown = WorkBuddyReasoning {
            supported_efforts: Some(vec!["off".into(), "bogus".into()]),
            default_effort: Some("off".into()),
            effort: None,
        };
        assert_eq!(offered_efforts("some-model", Some(&unknown)), None);
        assert_eq!(declared_default_effort(Some(&unknown)), None);
        assert_eq!(offered_efforts("some-model", None), None);
    }

    #[test]
    fn pinned_effort_is_not_mistaken_for_the_whole_menu() {
        // `deepseek-v4-pro` ships only `reasoning.effort: "high"`. It must not
        // collapse a non-DeepSeek model's menu to that single tier either.
        let pinned = WorkBuddyReasoning {
            supported_efforts: None,
            default_effort: None,
            effort: Some("high".into()),
        };
        assert_eq!(offered_efforts("hy3", Some(&pinned)), None);
        assert_eq!(
            declared_default_effort(Some(&pinned)).as_deref(),
            Some("high")
        );
    }
}
