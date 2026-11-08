//! Reading and saving GUI settings, and hot-applying the inbound gate.

use super::*;

/// Current settings with the upstream key masked.
///
/// The masking is not cosmetic: this payload is rendered in a browser, and a
/// browser's memory (and any screen share) is a worse place for a live upstream
/// key than a local webview's. The unmasked value is never needed by the form —
/// an empty field means "leave it alone", which [`save_settings`] honours.
pub fn settings_json() -> Value {
    let s = GuiSettings::load();
    let preset = s.models_preset();
    let mut payload = serde_json::to_value(&s).unwrap_or_else(|_| json!({}));
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("api_key".into(), json!(mask_key(&s.api_key)));
        obj.insert("api_key_set".into(), json!(!s.api_key.trim().is_empty()));
        // What the provider itself declares, so the console can label an unset
        // switch as "following the preset" rather than "off".
        obj.insert(
            "force_stream_preset".into(),
            json!(s.force_stream(&crate::providers::builtin_presets())),
        );
        obj.insert(
            "chat_url".into(),
            json!(s.chat_url(&crate::providers::builtin_presets())),
        );
        obj.insert("preset_name".into(), json!(preset.name));
    }
    payload
}

/// Decide what to store when the settings form posts an `api_key`.
///
/// Three cases, and the rule lives here so the desktop and the server console
/// cannot disagree about them (they used to; see the `*` case):
///
/// * blank / omitted → keep what is stored. The form has no key field, so an
///   empty string is "leave it alone", never "clear it".
/// * a masked echo (`•` or `*` runs) → keep what is stored. The form renders
///   the key masked, and a browser replaying that value must not overwrite the
///   real one.
/// * anything else → the new key, trimmed.
pub(crate) fn resolve_api_key(posted: &str, stored: &str) -> String {
    let v = posted.trim();
    if v.is_empty() || v.contains('•') || v.contains('*') {
        return stored.to_string();
    }
    v.to_string()
}

/// The fields a settings save may change.
///
/// A separate type from [`GuiSettings`] because `api_key` has different
/// semantics on write: an empty string means "keep the stored key", not "clear
/// it". Modelling that as a distinct input type is what keeps the rule in one
/// visible place instead of scattered through the handler.
#[derive(Debug, Default, serde::Deserialize)]
pub struct SettingsUpdate {
    pub provider_id: Option<String>,
    pub custom_url: Option<String>,
    /// `None` or empty → keep the existing key. A real value replaces it.
    pub api_key: Option<String>,
    pub port: Option<u16>,
    pub bind: Option<String>,
    pub reasoning_model: Option<String>,
    pub completion_model: Option<String>,
    pub model_map: Option<String>,
    pub sanitize_terms: Option<String>,
    pub force_stream: Option<bool>,
    pub client_auth_enabled: Option<bool>,
    /// Short-circuit Claude Code's auto-mode safety classifier with an `allow`
    /// verdict instead of forwarding it upstream. See
    /// [`settings::GuiSettings::intercept_safety_classifier`] for why this is
    /// opt-in rather than the default.
    pub intercept_safety_classifier: Option<bool>,
}

/// Apply a partial update and persist.
///
/// Returns the saved settings (masked) so the form can re-render from the truth
/// rather than from what it optimistically displayed.
pub fn save_settings(update: SettingsUpdate) -> Result<Value> {
    let mut s = GuiSettings::load();

    if let Some(v) = update.provider_id {
        s.provider_id = v.trim().to_string();
    }
    if let Some(v) = update.custom_url {
        s.custom_url = v.trim().to_string();
    }
    // Empty means "unchanged" — the form renders the key masked, so it cannot
    // echo a real value back and an empty submit must not wipe the key.
    if let Some(v) = update.api_key {
        let v = resolve_api_key(&v, &s.api_key);
        if !v.is_empty() {
            s.api_key = v;
        }
    }
    if let Some(v) = update.port {
        s.port = if v == 0 { settings::DEFAULT_PORT } else { v };
    }
    if let Some(v) = update.bind {
        let v = v.trim();
        s.bind = if v.is_empty() {
            "127.0.0.1".to_string()
        } else {
            v.to_string()
        };
    }
    if let Some(v) = update.reasoning_model {
        s.reasoning_model = v.trim().to_string();
    }
    if let Some(v) = update.completion_model {
        s.completion_model = v.trim().to_string();
    }
    if let Some(v) = update.model_map {
        s.model_map = v.trim().to_string();
    }
    if let Some(v) = update.sanitize_terms {
        s.sanitize_terms = v.trim().to_string();
    }
    if let Some(v) = update.force_stream {
        s.force_stream = Some(v);
    }
    if let Some(v) = update.client_auth_enabled {
        s.client_auth_enabled = v;
    }
    if let Some(v) = update.intercept_safety_classifier {
        s.intercept_safety_classifier = v;
    }

    s.save().map_err(|e| anyhow!("保存设置失败: {e}"))?;
    Ok(settings_json())
}

/// Hot-apply the inbound access-key gate after a settings save.
///
/// `ClientAuthState` is built once at process start and deliberately not rebuilt
/// on a proxy restart (it holds the throttled `last_used_ms` bookkeeping), so
/// persisting the switch alone is not enough: without this call the GUI would
/// report success while the data plane kept accepting keyless requests until a
/// full restart. Both the desktop and the server console call this, so the
/// "save means live" rule exists in exactly one place.
///
/// The flag is read back from the saved settings file rather than passed in:
/// `SettingsUpdate.client_auth_enabled` is optional, so an unrelated save must
/// not silently turn the feature off, and neither caller should have to
/// remember to fold in the `PROXY_CLIENT_AUTH` env override. That folding
/// happens here via [`client_accounts::enabled_from`].
///
/// `set_enabled` changes the live flag; `reload_from_store` re-reads the account
/// list (but *not* the flag, which is held in an `AtomicBool`). Both matter.
pub async fn hot_apply_client_auth(state: &Arc<crate::client_accounts::ClientAuthState>) {
    let stored_flag = GuiSettings::load().client_auth_enabled;
    let enabled = client_accounts::enabled_from(stored_flag);
    state.set_enabled(enabled);
    state.reload_from_store().await;
}

// ── Identity pool ──────────────────────────────────────────────────────────
