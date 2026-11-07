//! Upstream model selection: which model a request is sent to, and the free-tier fallbacks.

use super::*;

/// Resolve the upstream model for a client-facing model name.
///
/// The one place the precedence lives, shared by every route so a request
/// cannot reach a different upstream model depending on which endpoint it
/// arrived at:
///
/// 1. the policy's configured model for this kind of request (if any),
/// 2. otherwise the model the client asked for,
/// 3. then `model_map`, applied to whichever of those won,
/// 4. then the `[1M]`-style suffix stripped, when configured.
///
/// The configured model deliberately outranks the client's: it exists precisely
/// to override what the client sends. Note `model_map` is consulted *after*
/// that override, so a mapping keyed on the client's model name does not apply
/// once a configured model has taken its place.
/// The virtual model name that the gateway routes to a free upstream node.
///
/// Clients ask for `free`; the gateway resolves it to `hy3` during the day and
/// `hy4-preview` at night (local 23:00–08:00, when `hy4-preview` is free), and
/// lets the existing credential pool round-robin accounts when one is
/// daily-exhausted. This is the single name clients should pin to.
pub const FREE_MODEL_NAME: &str = "free";

/// Resolve the virtual `free` model to its concrete upstream node.
///
/// Daytime (local hour 08:00–22:59) → the day free node (`hy3`); night
/// (local 23:00–07:59) → the night free node (`hy4-preview`). The night window
/// and node names are overridable through the env vars below so the schedule or
/// nodes can be retuned without a rebuild:
///
/// * `PROXY_FREE_DAY_MODEL`   (default `hy3`)
/// * `PROXY_FREE_NIGHT_MODEL` (default `hy4-preview`)
/// * `PROXY_FREE_NIGHT_START` (default `23`, inclusive)
/// * `PROXY_FREE_NIGHT_END`   (default `8`, exclusive)
pub(crate) fn resolve_free_model() -> String {
    let day_model = std::env::var("PROXY_FREE_DAY_MODEL").unwrap_or_else(|_| "hy3".to_string());
    let night_model =
        std::env::var("PROXY_FREE_NIGHT_MODEL").unwrap_or_else(|_| "hy4-preview".to_string());
    let start: i32 = std::env::var("PROXY_FREE_NIGHT_START")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(23);
    let end: i32 = std::env::var("PROXY_FREE_NIGHT_END")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(8);

    if is_night(start, end) {
        night_model
    } else {
        day_model
    }
}

/// The daytime free node (`hy3` by default). When the night free node
/// (`hy4-preview`) is exhausted across every account and key, the virtual `free`
/// model degrades to this node instead of erroring — it is the daytime free
/// node, so it is still "free" to the client.
pub(crate) fn free_degraded_model() -> String {
    std::env::var("PROXY_FREE_DAY_MODEL").unwrap_or_else(|_| "hy3".to_string())
}

/// The night free node (`hy4-preview` by default). A `free` request is only
/// eligible for the night→day degradation when its resolved upstream model is
/// exactly this node; a daytime `free` request already resolves to the day node
/// and has nothing further to degrade to.
pub(crate) fn free_night_model() -> String {
    std::env::var("PROXY_FREE_NIGHT_MODEL").unwrap_or_else(|_| "hy4-preview".to_string())
}

/// Whether a given local `hour` (0–23) falls inside the night window
/// `[start, end)`, wrapping past midnight when `start > end`.
///
/// Kept pure (no clock access) so it can be unit-tested deterministically.
/// `resolve_free_model` feeds it the current local hour.
pub(crate) fn is_night_hour(hour: i32, start: i32, end: i32) -> bool {
    if start <= end {
        // Non-wrapping window, e.g. 0–8.
        hour >= start && hour < end
    } else {
        // Wrapping window, e.g. 23–8 → night if hour >= 23 OR hour < 8.
        hour >= start || hour < end
    }
}

/// Whether the local clock is currently inside the night window
/// `[start, end)` expressed in local hours (0–23), wrapping past midnight.
fn is_night(start: i32, end: i32) -> bool {
    let offset_secs = crate::util::local_utc_offset_secs();
    let local_ms = crate::util::unix_millis() + offset_secs * 1_000;
    let secs_of_day = ((local_ms / 1_000) % 86_400 + 86_400) % 86_400;
    let hour = (secs_of_day / 3_600) as i32;
    is_night_hour(hour, start, end)
}

pub(crate) fn resolve_upstream_model(
    client_model: &str,
    configured_model: Option<&String>,
    policy: &TranslationPolicy,
) -> String {
    // Virtual `free` model: routed by the local clock before any configured
    // override or `model_map`, so a request for `free` always lands on the
    // current free node regardless of the configured reasoning/completion model.
    if client_model == FREE_MODEL_NAME {
        return resolve_free_model();
    }

    let model = configured_model
        .cloned()
        .unwrap_or_else(|| client_model.to_string());

    let resolved = policy
        .model_map
        .get(&model)
        .cloned()
        .unwrap_or_else(|| model.clone());

    if policy.strip_model_suffix {
        strip_model_suffix(&resolved)
    } else {
        resolved
    }
}

pub(crate) fn select_model(
    req: &anthropic::AnthropicRequest,
    policy: &TranslationPolicy,
) -> String {
    let has_thinking = req
        .extra
        .get("thinking")
        .and_then(|v| v.as_object())
        .map(|o| o.get("type").and_then(|t| t.as_str()) == Some("enabled"))
        .unwrap_or(false);

    let configured = if has_thinking {
        policy.reasoning_model.as_ref()
    } else {
        policy.completion_model.as_ref()
    };

    resolve_upstream_model(&req.model, configured, policy)
}
