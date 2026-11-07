//! See the module table in `workbuddy_auth/mod.rs` for what lives here.

use super::*;

/// One upstream API key in the key pool.
///
/// Mirrors [`WorkBuddyCredential`] in shape (id / label / enabled / points) so
/// the GUI renders both pools with the same code, but it carries a raw key
/// instead of a login state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ApiKeyEntry {
    /// Stable short id (`k-<6 hex>`), derived from the key material so the same
    /// key imported twice collapses to one entry.
    pub id: String,
    #[serde(default)]
    pub label: String,
    pub key: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub points: Option<f64>,
    #[serde(default)]
    pub points_fetched_at_ms: Option<i64>,
}

impl ApiKeyEntry {
    /// Masked display form: the key never goes back to the UI whole.
    pub fn masked(&self) -> String {
        mask_secret(&self.key)
    }
}

/// Stable short id for an API key, derived like [`credential_id`].
pub fn api_key_id(key: &str) -> String {
    use std::fmt::Write;
    let digest = md5_hex(key.as_bytes());
    let mut out = String::from("k-");
    for byte in &digest[..3] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// API key pool file path: `~/.proxy-rs/api-keys.json`.
///
/// Live keys, so it is written 0600 like the credential store.
pub fn api_keys_path() -> PathBuf {
    crate::settings::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("api-keys.json")
}

/// Load the API key pool.
pub fn load_api_keys() -> Vec<ApiKeyEntry> {
    let Ok(text) = std::fs::read_to_string(api_keys_path()) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Persist the API key pool (mode 0600).
pub fn save_api_keys(items: &[ApiKeyEntry]) -> Result<()> {
    let path = api_keys_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(items)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Upsert one API key (matched by [`api_key_id`]).
pub fn upsert_api_key(item: ApiKeyEntry) -> Result<ApiKeyEntry> {
    let mut items = load_api_keys();
    let id = item.id.clone();
    if let Some(slot) = items.iter_mut().find(|k| k.id == id) {
        *slot = item.clone();
    } else {
        items.push(item.clone());
    }
    save_api_keys(&items)?;
    Ok(item)
}

/// The upstream key the proxy should use right now.
///
/// Resolution: a key pinned by the unified default identity wins, else the
/// pool's `default_key_id`, else the first enabled key, else `None` — which
/// leaves [`crate::config::Config::api_key`] (the single GUI field / env var) in
/// charge, so a 1.7.x setup is untouched.
pub fn active_api_key() -> Option<String> {
    let items = load_api_keys();
    if items.is_empty() {
        return None;
    }
    let prefs = load_preferences();
    let enabled = |k: &ApiKeyEntry| k.enabled && !k.key.trim().is_empty();
    // The unified 身份池 selector can pin a key directly (`k-…`). Honour it
    // first, but fall through (not disable) if that pinned key is gone/disabled.
    if prefs.default_identity_id.starts_with("k-") {
        if let Some(k) = items
            .iter()
            .find(|k| k.id == prefs.default_identity_id && enabled(k))
        {
            return Some(k.key.trim().to_string());
        }
    }
    if !prefs.default_key_id.is_empty() {
        if let Some(k) = items
            .iter()
            .find(|k| k.id == prefs.default_key_id && enabled(k))
        {
            return Some(k.key.trim().to_string());
        }
    }
    // Falls through to the first enabled key when the remembered default was
    // deleted or disabled: a stale pointer must not disable the proxy.
    items
        .iter()
        .find(|k| enabled(k))
        .map(|k| k.key.trim().to_string())
}

/// The active API key entry (including its id, label, and key string).
pub fn active_api_key_entry() -> Option<ApiKeyEntry> {
    let items = load_api_keys();
    if items.is_empty() {
        return None;
    }
    let prefs = load_preferences();
    let enabled = |k: &ApiKeyEntry| k.enabled && !k.key.trim().is_empty();
    if prefs.default_identity_id.starts_with("k-") {
        if let Some(k) = items
            .iter()
            .find(|k| k.id == prefs.default_identity_id && enabled(k))
        {
            return Some(k.clone());
        }
    }
    if !prefs.default_key_id.is_empty() {
        if let Some(k) = items
            .iter()
            .find(|k| k.id == prefs.default_key_id && enabled(k))
        {
            return Some(k.clone());
        }
    }
    // No pin: the first enabled key in the GUI's order, so dragging a key to the
    // top of the identity list makes it the one used.
    let ordered = apply_identity_order(items, &prefs.identity_order, |k| k.id.as_str());
    ordered.into_iter().find(|k| enabled(k))
}

/// All currently enabled API keys in the key pool with non-empty keys.
///
/// Returned in the GUI's identity order, so a key dragged above another key (or
/// into the middle of the accounts) is tried in that position: the failover path
/// walks this list, and `enabled_api_keys()` is its only entry point.
pub fn enabled_api_keys() -> Vec<ApiKeyEntry> {
    let prefs = load_preferences();
    let items = apply_identity_order(
        load_api_keys()
            .into_iter()
            .filter(|k| k.enabled && !k.key.trim().is_empty())
            .collect(),
        &prefs.identity_order,
        |k| k.id.as_str(),
    );
    items
}

/// Store the queried points for one API key.
pub fn set_api_key_points(id: &str, points: Option<f64>) -> Result<()> {
    let mut items = load_api_keys();
    let Some(k) = items.iter_mut().find(|k| k.id == id) else {
        return Ok(());
    };
    k.points = points;
    k.points_fetched_at_ms = Some(crate::util::unix_millis());
    save_api_keys(&items)
}

/// Query the remaining points for one API key.
///
/// The billing endpoint accepts the raw key material exactly as it
/// authenticates a chat request (`Authorization: Bearer <key>` plus
/// `X-API-Key`), and `credits.rs` already uses that pairing against this same
/// URL — so a key's balance is readable without any account login state.
pub async fn fetch_points_for_key(client: &reqwest::Client, key: &ApiKeyEntry) -> Option<f64> {
    if key.key.trim().is_empty() {
        return None;
    }
    let url = format!(
        "{}{}",
        WORKBUDDY_BILLING_HOST,
        crate::credits::WORKBUDDY_RESOURCE_PATH
    );
    let resp = client
        .post(&url)
        .timeout(std::time::Duration::from_secs(20))
        .header("Authorization", format!("Bearer {}", key.key))
        .header("X-API-Key", &key.key)
        .header("Content-Type", "application/json")
        .header("X-Client-Platform", "web")
        .header("Origin", WORKBUDDY_BILLING_HOST)
        .header(
            "Referer",
            format!("{}/profile/plans-usage", WORKBUDDY_BILLING_HOST),
        )
        .header(
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36",
        )
        .json(&serde_json::json!({
            "PageNumber": 1,
            "PageSize": 100,
            "ProductCode": crate::credits::WORKBUDDY_RESOURCE_PRODUCT_CODE,
            "Status": [0, 3],
        }))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;

    // A business error is not a balance; reporting 0 would read as exhausted.
    if let Some(code) = value.get("code").and_then(|c| c.as_i64()) {
        if code != 0 {
            return None;
        }
    }
    Some(crate::credits::remaining_from_response(&value))
}

/// Query and persist one API key's balance. Same rule as above.
pub(crate) async fn refresh_one_key_points(
    client: &reqwest::Client,
    key: &ApiKeyEntry,
) -> PointsResult {
    let points = fetch_points_for_key(client, key).await;
    let _ = set_api_key_points(&key.id, points);
    PointsResult {
        id: key.id.clone(),
        label: key_display_label(key),
        points,
    }
}

/// The name to show for an API key: its label, or the id when unlabelled.
fn key_display_label(key: &ApiKeyEntry) -> String {
    if key.label.is_empty() {
        key.id.clone()
    } else {
        key.label.clone()
    }
}
