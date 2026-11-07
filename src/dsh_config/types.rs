//! The shapes this module speaks, and the read-only inspection that fills
//! them.
//!
//! [`DshModel`] is what the writer is asked to write, [`DshState`] is what the
//! UI is told about the current wiring, and [`read_state`] is the pure function
//! that derives the latter from the document text.

use super::*;

/// One model entry to write.
#[derive(Debug, Clone, PartialEq)]
pub struct DshModel {
    pub id: String,
    pub name: Option<String>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    /// `true` writes `[ text, image ]`; `false` writes `[ text ]`; `None` omits
    /// the key so DSH falls back to the route's `defaultInput`.
    pub supports_images: Option<bool>,
    /// `true` declares reasoning levels; `None` omits the key, which leaves the
    /// model without an Effort menu rather than inventing capability.
    pub supports_reasoning: Option<bool>,
    /// The effort tiers to advertise, in escalation order (`deepseek-*` →
    /// `low`/`high`/`max`). Empty/`None` keeps the historical single `high`
    /// level, so a model whose tiers the upstream does not enumerate does not
    /// silently gain an Effort menu it never had.
    pub reasoning_efforts: Vec<String>,
    /// Per-model 积分 consumption ratio (`×0.29`). `None` omits it; `Some(0.0)`
    /// is a free model.
    pub points_ratio: Option<f64>,
    /// Upstream free badge (e.g. `夜间免费`); `None` when not free.
    pub free_badge: Option<String>,
}

/// What the UI reports about the current wiring.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DshState {
    pub provider_exists: bool,
    pub base_url: Option<String>,
    pub model_ids: Vec<String>,
    /// True when the referenced credential is present in the credential store.
    pub credential_present: bool,
    /// True when the provider already sends the conversation session id, so the
    /// gateway's per-conversation session filter works.
    pub session_header: bool,
}

/// Map a provider model onto a DSH entry.
///
/// Field names differ across the boundary — the proxy speaks `snake_case`,
/// pi-ai's profile uses `camelCase` — so the mapping is explicit here rather
/// than delegated to a rename attribute on either side.
pub fn to_dsh_model(m: &crate::providers::GuiModel) -> DshModel {
    // DSH shows the model by its `name`; append the 积分 ratio and free badge so
    // the user sees the relative cost next to the model in the DSH picker.
    let name = match (&m.name, m.points_ratio, &m.free_badge) {
        (Some(n), Some(r), Some(b)) => Some(format!("{} (×{:.2} {})", n, r, b)),
        (Some(n), Some(r), None) => Some(format!("{} (×{:.2})", n, r)),
        (Some(n), None, Some(b)) => Some(format!("{} ({})", n, b)),
        (Some(n), None, None) => Some(n.clone()),
        (None, _, _) => None,
    };
    DshModel {
        id: m.id.clone(),
        name,
        context_window: m.context_window,
        max_tokens: m.max_output_tokens,
        supports_images: m.supports_images,
        supports_reasoning: m.supports_reasoning,
        reasoning_efforts: m.reasoning_efforts.clone().unwrap_or_default(),
        points_ratio: m.points_ratio,
        free_badge: m.free_badge.clone(),
    }
}

// ── Reading ─────────────────────────────────────────────────────────────────

/// Inspect a patch document without modifying it.
pub fn read_state(patch_text: &str, credentials_text: Option<&str>) -> DshState {
    let lines: Vec<String> = patch_text.lines().map(|s| s.to_string()).collect();
    let credential_present = credentials_text.map(credential_present).unwrap_or(false);
    let Some(span) = find_span(&lines) else {
        return DshState {
            provider_exists: false,
            base_url: None,
            model_ids: Vec::new(),
            credential_present,
            session_header: false,
        };
    };
    let body = &lines[span.body_start..span.body_end];
    DshState {
        provider_exists: true,
        base_url: scalar_at(body, "baseURL"),
        model_ids: model_ids_in(body),
        credential_present,
        session_header: body.iter().any(|l| {
            l.trim()
                .strip_prefix("sessionHeader:")
                .map(|rest| rest.trim() == SESSION_HEADER)
                .unwrap_or(false)
        }),
    }
}

/// Whether the credential store already defines the reference.
pub fn credential_present(credentials_text: &str) -> bool {
    credentials_text.lines().any(|l| {
        let t = l.trim();
        t.strip_prefix(CREDENTIAL_REF)
            .map(|rest| rest.trim_start().starts_with(':'))
            .unwrap_or(false)
    })
}
