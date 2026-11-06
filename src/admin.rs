//! Transport-agnostic admin surface for the gateway.
//!
//! ## Why this module exists
//!
//! The desktop console reaches the gateway through Tauri IPC: 54
//! `#[tauri::command]` functions in `src-tauri/src/main.rs`, each holding the
//! real logic. A headless server has no webview and no IPC, so on Linux none of
//! that is reachable — and the capabilities it gates are not decoration. Without
//! them an operator cannot add a WorkBuddy login, rotate an upstream key, or
//! issue a client access key without hand-editing JSON in `/opt/proxy-rs/data`.
//!
//! This module is the server's equivalent. It deliberately **does not** try to be
//! shared with the desktop app:
//!
//! * The desktop commands mutate live `AppContext` state (settings behind an
//!   `RwLock`, a `ServiceController` whose listener is torn down and rebound, a
//!   Tauri `AppHandle` for tray updates). None of that exists here.
//! * Several are desktop-only by nature — start/stop the service, open the log
//!   directory, native "save as" panels, OAuth with a `localhost` callback. A
//!   server has no browser to redirect to and no file manager to open.
//!
//! What *is* shared, and must stay shared, is the storage layer: every function
//! below goes through the same `proxy_rs::workbuddy_auth` / `client_accounts` /
//! `pool_transfer` calls the desktop app uses. Two front-ends writing the same
//! files through different code would eventually disagree about a field.
//!
//! ## What this module is not
//!
//! It does not authenticate. Every handler here assumes the caller was already
//! authorised by [`crate::admin_auth`]; keeping the two apart is what lets the
//! auth layer be tested without a live store and the handlers be tested without
//! forging cookies.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::client_accounts;
use crate::pool_transfer;
use crate::session_pool::CredentialPool;
use crate::settings::{self, GuiSettings};
use crate::stats::StatsDb;
use crate::workbuddy_auth::{self, ApiKeyEntry, WorkBuddyCredential};

/// Read-only view of the running process, for the console's status strip.
///
/// A struct rather than reading the environment at render time: the values must
/// all describe *the same moment*, and a payload assembled from three separate
/// reads can straddle a restart.
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub version: &'static str,
    pub bound_port: u16,
    pub started_at: std::time::Instant,
    pub bind_addr: String,
}

impl ProcessInfo {
    /// The status payload the console's header renders.
    pub fn status_json(&self) -> Value {
        let settings = GuiSettings::load();
        let presets = crate::providers::builtin_presets();
        json!({
            // A headless gateway has no start/stop control — it is always
            // "running" if this code is answering at all. Reporting `true`
            // rather than omitting the field keeps the console's shape aligned
            // with the desktop payload.
            "running": true,
            "service_running": true,
            "managed_by": "systemd",
            "uptime_secs": self.started_at.elapsed().as_secs(),
            "version": self.version,
            "provider": settings.provider_id,
            "upstream": settings.chat_url(&presets),
            "port": self.bound_port,
            "configured_port": settings.port,
            "bind": self.bind_addr,
            "client_auth_enabled": client_accounts::enabled_from(settings.client_auth_enabled),
            "data_dir": settings::data_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        })
    }
}

// ── Settings ───────────────────────────────────────────────────────────────

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
        let v = v.trim();
        if !v.is_empty() && !v.contains('•') {
            s.api_key = v.to_string();
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

    s.save().map_err(|e| anyhow!("保存设置失败: {e}"))?;
    Ok(settings_json())
}

// ── Identity pool ──────────────────────────────────────────────────────────

/// Masked view of one credential, shaped exactly like the desktop console's row.
///
/// Kept identical on purpose: the same front-end code renders both, and a field
/// that exists on one transport but not the other shows up as a blank cell with
/// no explanation.
pub fn credential_json(c: &WorkBuddyCredential, sticky_sessions: usize, is_default: bool) -> Value {
    let now = crate::util::unix_millis();
    let state = if !c.enabled {
        "disabled"
    } else if c
        .cooldown_until_ms
        .map(|until| now < until)
        .unwrap_or(false)
    {
        "cooldown"
    } else if !c.is_usable(now) {
        "expired"
    } else {
        "ok"
    };
    // Whether a fresh login is the only way back. Two cases, both un-self-healing:
    // a credential with no refresh token at all, and one whose refresh was
    // *rejected*. The second cannot be detected statically, so it comes from the
    // durable marker a failed refresh writes.
    let needs_relogin =
        c.enabled && (c.refresh_token.is_none() || c.relogin_required_since_ms.is_some());
    json!({
        "id": c.id,
        "label": c.label,
        "masked_token": c.masked_token(),
        "expires_at_ms": c.expires_at_ms,
        "uid": c.account.uid,
        "nickname": c.account.nickname,
        "enabled": c.enabled,
        "state": state,
        "last_error": c.last_error,
        "sticky_sessions": sticky_sessions,
        "is_default": is_default,
        "points": c.points,
        "points_fetched_at_ms": c.points_fetched_at_ms,
        "last_refresh_at_ms": c.last_refresh_at_ms,
        "has_refresh_token": c.refresh_token.is_some(),
        "needs_relogin": needs_relogin,
        "relogin_required_since_ms": c.relogin_required_since_ms,
        "checked_in_today": workbuddy_auth::checked_in_today(
            c,
            &workbuddy_auth::local_day(now),
        ),
    })
}

/// The interleaved identity list: credentials *and* upstream keys, in the
/// user's own order.
pub async fn identities_json(pool: &CredentialPool) -> Value {
    let credentials = workbuddy_auth::load_credentials();
    let prefs = workbuddy_auth::load_preferences();
    let now = crate::util::unix_millis();

    let effective_default = credentials
        .iter()
        .find(|c| c.is_usable(now))
        .map(|c| c.id.clone())
        .unwrap_or_default();
    let default_id = if prefs.default_credential_id.is_empty() {
        effective_default
    } else {
        prefs.default_credential_id.clone()
    };

    let mut cred_rows: Vec<Value> = Vec::with_capacity(credentials.len());
    for c in &credentials {
        let sessions = pool.sticky_sessions_for(&c.id).await;
        cred_rows.push(credential_json(c, sessions, c.id == default_id));
    }

    let keys = workbuddy_auth::load_api_keys();
    let active_key = workbuddy_auth::active_api_key();
    let key_rows: Vec<Value> = keys
        .iter()
        .map(|k| {
            let is_default = if !prefs.default_key_id.is_empty() {
                k.id == prefs.default_key_id
            } else {
                active_key.as_deref() == Some(k.key.as_str())
            };
            json!({
                "id": k.id,
                "label": k.label,
                "masked": k.masked(),
                "enabled": k.enabled,
                "points": k.points,
                "is_default": is_default,
                "kind": "key",
            })
        })
        .collect();

    json!({
        "credentials": cred_rows,
        "keys": key_rows,
        "default_credential_id": prefs.default_credential_id,
        "default_key_id": prefs.default_key_id,
        "default_identity_id": prefs.default_identity_id,
        "identity_order": prefs.identity_order,
        "enabled_accounts": credentials.iter().filter(|c| c.enabled).count(),
    })
}

/// Add or replace a WorkBuddy login from pasted login-state JSON.
pub fn add_credential(login_state: &str, label: &str) -> Result<Value> {
    let mut credential = workbuddy_auth::parse_login_state(login_state)
        .map_err(|e| anyhow!("登录态解析失败: {e}"))?;
    if !label.trim().is_empty() {
        credential.label = label.trim().to_string();
    }
    workbuddy_auth::upsert_credential(credential.clone())
        .map_err(|e| anyhow!("保存登录态失败: {e}"))?;
    Ok(json!({ "ok": true, "id": credential.id, "label": credential.label }))
}

pub fn delete_credential(id: &str) -> Result<Value> {
    let mut items = workbuddy_auth::load_credentials();
    let before = items.len();
    items.retain(|c| c.id != id);
    if items.len() == before {
        bail!("账号不存在: {id}");
    }
    workbuddy_auth::save_credentials(&items).map_err(|e| anyhow!("保存失败: {e}"))?;
    Ok(json!({ "ok": true }))
}

pub fn set_credential_enabled(id: &str, enabled: bool) -> Result<Value> {
    let mut items = workbuddy_auth::load_credentials();
    let Some(slot) = items.iter_mut().find(|c| c.id == id) else {
        bail!("账号不存在: {id}");
    };
    slot.enabled = enabled;
    workbuddy_auth::save_credentials(&items).map_err(|e| anyhow!("保存失败: {e}"))?;
    Ok(json!({ "ok": true, "enabled": enabled }))
}

pub fn add_key(key: &str, label: &str) -> Result<Value> {
    let key = key.trim();
    if key.is_empty() {
        bail!("API Key 不能为空");
    }
    let id = workbuddy_auth::api_key_id(key);
    let mut items = workbuddy_auth::load_api_keys();
    if items.iter().any(|k| k.id == id) {
        // Idempotent rather than an error: pasting the same key twice is a
        // plausible mis-click, and the outcome the user wants (it is in the
        // pool) has already happened.
        return Ok(json!({ "ok": true, "id": id, "already_present": true }));
    }
    items.push(ApiKeyEntry {
        id: id.clone(),
        label: if label.trim().is_empty() {
            "API Key".to_string()
        } else {
            label.trim().to_string()
        },
        key: key.to_string(),
        enabled: true,
        points: None,
        points_fetched_at_ms: None,
    });
    workbuddy_auth::save_api_keys(&items).map_err(|e| anyhow!("保存失败: {e}"))?;
    Ok(json!({ "ok": true, "id": id }))
}

pub fn delete_key(id: &str) -> Result<Value> {
    let mut items = workbuddy_auth::load_api_keys();
    let before = items.len();
    items.retain(|k| k.id != id);
    if items.len() == before {
        bail!("密钥不存在: {id}");
    }
    workbuddy_auth::save_api_keys(&items).map_err(|e| anyhow!("保存失败: {e}"))?;
    Ok(json!({ "ok": true }))
}

pub fn set_key_enabled(id: &str, enabled: bool) -> Result<Value> {
    let mut items = workbuddy_auth::load_api_keys();
    let Some(slot) = items.iter_mut().find(|k| k.id == id) else {
        bail!("密钥不存在: {id}");
    };
    slot.enabled = enabled;
    workbuddy_auth::save_api_keys(&items).map_err(|e| anyhow!("保存失败: {e}"))?;
    Ok(json!({ "ok": true, "enabled": enabled }))
}

/// Replace an existing credential's login state, keeping its identity.
///
/// The id is preserved so anything that referred to the account — the request
/// log, the default-identity pointer, the sticky sessions — keeps resolving. A
/// re-login that minted a new id would silently orphan all of those.
pub fn relogin_credential(id: &str, login_state: &str) -> Result<Value> {
    let mut fresh = workbuddy_auth::parse_login_state(login_state)
        .map_err(|e| anyhow!("登录态解析失败: {e}"))?;

    let existing = workbuddy_auth::load_credentials()
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| anyhow!("账号不存在: {id}"))?;

    // Carry the user's own settings across the re-login. Points are deliberately
    // *not* carried: they belong to the account and will be re-read.
    if !existing.label.trim().is_empty() && existing.label != existing.account.nickname {
        fresh.label = existing.label.clone();
    }
    fresh.enabled = existing.enabled;
    fresh.last_checkin_date = existing.last_checkin_date.clone();
    fresh.cooldown_until_ms = None;
    fresh.last_error.clear();
    fresh.id = existing.id.clone();

    let saved =
        workbuddy_auth::upsert_credential(fresh).map_err(|e| anyhow!("保存登录态失败: {e}"))?;
    Ok(json!({ "ok": true, "id": saved.id, "label": saved.label }))
}

pub fn set_default_identity(id: &str) -> Result<Value> {
    let prefs =
        workbuddy_auth::set_default_identity(id).map_err(|e| anyhow!("设置默认身份失败: {e}"))?;
    Ok(json!({ "ok": true, "default_identity_id": prefs.default_identity_id }))
}

/// Persist a new identity order.
///
/// Validated against the store rather than trusted: the list is a full
/// permutation of the pool, so a stale browser tab submitting an old order would
/// otherwise silently drop whichever identity was added since. The desktop
/// command applies the same check; the message it produces ("pool has N,
/// submitted M") is what makes that failure self-explanatory.
pub fn reorder_identities(order: &[String]) -> Result<Value> {
    let mut known: Vec<String> = workbuddy_auth::load_credentials()
        .iter()
        .map(|c| c.id.clone())
        .collect();
    known.extend(workbuddy_auth::load_api_keys().iter().map(|k| k.id.clone()));

    let known_set: std::collections::HashSet<&str> = known.iter().map(|s| s.as_str()).collect();
    let requested_set: std::collections::HashSet<&str> = order.iter().map(|s| s.as_str()).collect();
    if requested_set.len() != order.len() {
        bail!("排序列表含重复的身份 id");
    }
    if requested_set != known_set || order.len() != known.len() {
        bail!(
            "排序列表与身份池不一致（池内 {} 个，提交 {} 个）",
            known.len(),
            order.len()
        );
    }

    let mut prefs = workbuddy_auth::load_preferences();
    prefs.identity_order = order.to_vec();
    // Pin the default to the new top so a reload cannot revert the drag.
    let default_moved = match workbuddy_auth::reorder_pin_target(&prefs, order) {
        Some(top) => {
            prefs.default_identity_id = top;
            true
        }
        None => false,
    };
    workbuddy_auth::save_preferences(&prefs).map_err(|e| anyhow!("保存顺序失败: {e}"))?;
    Ok(
        json!({ "ok": true, "identity_order": prefs.identity_order, "default_moved": default_moved }),
    )
}

pub fn preferences_json() -> Value {
    let p = workbuddy_auth::load_preferences();
    json!({
        "default_credential_id": p.default_credential_id,
        "default_key_id": p.default_key_id,
        "default_identity_id": p.default_identity_id,
        "daily_checkin_enabled": p.daily_checkin_enabled,
        "daily_checkin_time": p.daily_checkin_time,
        "last_checkin_run_date": p.last_checkin_run_date,
        "growth_enabled": p.growth_enabled,
        "last_growth_run_date": p.last_growth_run_date,
        "identity_order": p.identity_order,
    })
}

pub fn save_schedule(enabled: bool, time: &str, growth: Option<bool>) -> Result<Value> {
    workbuddy_auth::save_schedule(enabled, time).map_err(|e| anyhow!("保存打卡设置失败: {e}"))?;
    if let Some(g) = growth {
        workbuddy_auth::save_growth_enabled(g).map_err(|e| anyhow!("保存成长任务设置失败: {e}"))?;
    }
    Ok(preferences_json())
}

// ── Inbound access accounts ────────────────────────────────────────────────

pub fn client_account_json(a: &client_accounts::ClientAccount, now_ms: i64) -> Value {
    json!({
        "id": a.id,
        "label": a.label,
        "key_prefix": a.key_prefix,
        "enabled": a.enabled,
        "created_at_ms": a.created_at_ms,
        "created_days_ago": (now_ms - a.created_at_ms).max(0) / 86_400_000,
        "last_used_ms": a.last_used_ms,
        "note": a.note,
    })
}

pub async fn client_accounts_json(state: &client_accounts::ClientAuthState) -> Value {
    let now = crate::util::unix_millis();
    let accounts = state.snapshot().await;
    let list: Vec<Value> = accounts
        .iter()
        .map(|a| client_account_json(a, now))
        .collect();
    json!({
        "accounts": list,
        "total": list.len(),
        // The switch's *effective* value: `PROXY_CLIENT_AUTH` may have forced
        // it, in which case the console checkbox must show the truth, not the
        // stored preference it cannot override.
        "enabled": state.is_enabled(),
        "env_forced": crate::config::env_value(client_accounts::ENABLED_ENV).is_some(),
    })
}

/// Create an inbound access account. The plaintext key appears **once**, here.
pub fn create_client_account(label: &str, note: &str) -> Result<Value> {
    let (account, key) = client_accounts::create_account(label, note)
        .map_err(|e| anyhow!("创建访问密钥失败: {e}"))?;
    Ok(json!({
        "account": client_account_json(&account, crate::util::unix_millis()),
        // The one and only time this value exists. The store keeps a hash, so it
        // cannot be fetched again — only revoked and reissued.
        "api_key": key,
        "header": client_accounts::KEY_HEADER,
    }))
}

pub fn set_client_account_enabled(id: &str, enabled: bool) -> Result<Value> {
    let changed =
        client_accounts::set_enabled(id, enabled).map_err(|e| anyhow!("保存失败: {e}"))?;
    if !changed {
        bail!("访问账户不存在: {id}");
    }
    Ok(json!({ "ok": true, "enabled": enabled }))
}

pub fn delete_client_account(id: &str) -> Result<Value> {
    let changed = client_accounts::delete_account(id).map_err(|e| anyhow!("删除失败: {e}"))?;
    if !changed {
        bail!("访问账户不存在: {id}");
    }
    Ok(json!({ "ok": true }))
}

// ── Stats / logs ───────────────────────────────────────────────────────────

/// Today's statistics, off the async runtime.
pub async fn stats_today(stats: std::sync::Arc<StatsDb>) -> Result<Value> {
    let today = tokio::task::spawn_blocking(move || stats.query_today())
        .await
        .map_err(|e| anyhow!("统计查询异常终止: {e}"))?
        .map_err(|e| anyhow!("统计查询失败: {e}"))?;
    Ok(crate::api::stats_today_json(&today))
}

// ── Pool import / export ───────────────────────────────────────────────────

/// Export the pool as text, for the browser to save.
///
/// The desktop app opens a native "save as" panel; a server has nobody at the
/// console, so the same bytes go out as an HTTP download. That is also *less*
/// privileged: the server never learns a destination path, so it needs no
/// ability to write outside its own data directory.
pub fn export_pool(password: Option<&str>) -> Result<String> {
    let opts = pool_transfer::ExportOptions {
        passphrase: password
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string),
    };
    let (text, _outcome) =
        pool_transfer::export_text(&opts).map_err(|e| anyhow!("导出失败: {e}"))?;
    Ok(text)
}

/// Preview an import without writing anything.
pub fn preview_pool(text: &str, password: Option<&str>, mode: &str) -> Result<Value> {
    let mode = parse_import_mode(mode)?;
    let preview = pool_transfer::preview_import(
        text,
        password.map(str::trim).filter(|p| !p.is_empty()),
        mode,
    )
    .map_err(|e| anyhow!("导入包解析失败: {e}"))?;
    Ok(serde_json::to_value(preview).unwrap_or_else(|_| json!({})))
}

/// Apply an import. Re-decodes rather than trusting a previous preview: the
/// bundle could have been edited between the two calls, and the write must
/// reflect what is on screen now.
pub fn apply_pool_import(text: &str, password: Option<&str>, mode: &str) -> Result<Value> {
    let mode = parse_import_mode(mode)?;
    let preview = pool_transfer::apply_import(
        text,
        password.map(str::trim).filter(|p| !p.is_empty()),
        mode,
    )
    .map_err(|e| anyhow!("导入失败: {e}"))?;
    Ok(serde_json::to_value(preview).unwrap_or_else(|_| json!({})))
}

fn parse_import_mode(mode: &str) -> Result<pool_transfer::ImportMode> {
    match mode.trim().to_ascii_lowercase().as_str() {
        "" | "merge" => Ok(pool_transfer::ImportMode::Merge),
        "replace" => Ok(pool_transfer::ImportMode::Replace),
        other => bail!("未知的导入模式: {other}（可选 merge / replace）"),
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Show a key's first and last four characters, never more.
///
/// A key of 8 characters or fewer is masked entirely: revealing "first 4 + last
/// 4" of an 8-character secret reveals the whole secret.
pub fn mask_key(key: &str) -> String {
    if key.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "•".repeat(chars.len());
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}••••{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_key_is_masked_entirely() {
        // 8 chars: "first four + last four" would BE the key.
        assert_eq!(mask_key("abcdefgh"), "••••••••");
        assert_eq!(mask_key("short"), "•••••");
        assert_eq!(mask_key(""), "");
    }

    #[test]
    fn a_long_key_shows_only_the_ends() {
        assert_eq!(mask_key("sk-abcdefghijklmnop"), "sk-a••••mnop");
        // No middle survives.
        assert!(!mask_key("sk-abcdefghijklmnop").contains("efgh"));
    }
}
