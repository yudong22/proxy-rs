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
use std::sync::Arc;

use crate::claude_sessions;
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
    /// The port the gateway is listening on, read at call time.
    ///
    /// A live reference rather than a snapshot: the desktop binds its port
    /// *after* the admin state is built (the state is built first, so the
    /// console exists even when the bind fails), and a snapshot taken then
    /// would report the configured port instead of the one actually in use —
    /// which is how the console came to show "3456" while serving on 3457.
    ///
    /// The server build passes a fixed value (its port never changes after
    /// start), so the shape is the same for both.
    pub bound_port: std::sync::Arc<std::sync::atomic::AtomicU16>,
    pub started_at: std::time::Instant,
    pub bind_addr: String,
}

impl ProcessInfo {
    /// The port the gateway is listening on right now.
    pub fn port(&self) -> u16 {
        self.bound_port.load(std::sync::atomic::Ordering::SeqCst)
    }
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
            // Read by the shared front-end to label the process lifecycle. The
            // console cannot hide the start/stop button (that file is shared
            // with the desktop app), so clicking it surfaces the bridge's
            // explanation instead of a silent no-op.
            "managed_by": "systemd",
            "can_control_service": false,
            "uptime_secs": self.started_at.elapsed().as_secs(),
            "version": self.version,
            "provider": settings.provider_id,
            "upstream_url": settings.chat_url(&presets),
            "config_path": settings::settings_path().display().to_string(),
            "log_path": settings::log_file_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            // Desktop-only fields the shared front-end reads. They are answered
            // honestly rather than omitted: a missing key renders as the empty
            // string in a `<code>` element, which looks like a bug, while an
            // explicit "由 systemd 托管" reads as the truth.
            "launch_at_login": false,
            "launch_at_login_stale": false,
            "managed_by": "systemd",
            "port": self.port(),
            "configured_port": settings.port,
            "bind": self.bind_addr,
            "client_auth_enabled": client_accounts::enabled_from(settings.client_auth_enabled),
            "data_dir": settings::data_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            // Whether an upstream key is configured, so the settings form can say
            // "already set, leave blank to keep" instead of looking empty.
            "api_key_set": !settings.api_key.trim().is_empty(),
            "env_path": settings::dotenv_path().map(|p| p.display().to_string()),
            // **The overview's 剩余积分 / 当前账号 cards read this.**
            //
            // It was missing from the server payload, so on the web console those
            // two cards rendered empty forever while the desktop app showed them
            // — the same front-end, a hole in the data behind it. The shape must
            // match the desktop command exactly (`id`/`label`/`points`).
            "current_identity": current_identity_summary(),
            // `is_dev` gates a couple of desktop-only affordances; a server is
            // never a dev build.
            "is_dev": false,
            "configured_launch_at_login": false,
            "launch_at_login_plist_present": false,
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

// ── Overview cards ─────────────────────────────────────────────────────────

/// The identity currently in force, for the overview header's three cards.
///
/// Resolved through `effective_default_identity` rather than the stored
/// preference: with no explicit default the first usable entry wins, and the
/// card must describe what is *actually* serving traffic.
pub fn current_identity_summary() -> Value {
    let id = workbuddy_auth::effective_default_identity();
    let (label, points) = match id.as_str() {
        id if id.starts_with("wb-") => workbuddy_auth::load_credentials()
            .into_iter()
            .find(|c| c.id == id)
            .map(|c| (c.label, c.points))
            .unwrap_or_else(|| ("未知账号".to_string(), None)),
        id if id.starts_with("k-") => workbuddy_auth::load_api_keys()
            .into_iter()
            .find(|k| k.id == id)
            .map(|k| (k.label, k.points))
            .unwrap_or_else(|| ("未知密钥".to_string(), None)),
        _ => ("静态 API Key".to_string(), None),
    };
    json!({ "id": id, "label": label, "points": points })
}

/// Credentials list in the shape the desktop console's identity pool expects.
pub async fn credentials_list(pool: &CredentialPool) -> Result<Value> {
    let items = workbuddy_auth::load_credentials();
    let prefs = workbuddy_auth::load_preferences();
    let now = crate::util::unix_millis();
    let effective_default = items
        .iter()
        .find(|c| c.is_usable(now))
        .map(|c| c.id.clone())
        .unwrap_or_default();
    let default_id = if prefs.default_credential_id.is_empty() {
        effective_default
    } else {
        prefs.default_credential_id.clone()
    };
    let mut list: Vec<Value> = Vec::with_capacity(items.len());
    for c in &items {
        let sessions = pool.sticky_sessions_for(&c.id).await;
        list.push(credential_json(c, sessions, c.id == default_id));
    }
    Ok(json!({
        "credentials": list,
        "default_credential_id": prefs.default_credential_id,
        "default_identity_id": prefs.default_identity_id,
        "identity_order": prefs.identity_order,
    }))
}

/// Upstream API keys in the shape the identity pool expects.
pub fn api_keys_list() -> Result<Value> {
    let items = workbuddy_auth::load_api_keys();
    let prefs = workbuddy_auth::load_preferences();
    let effective = workbuddy_auth::active_api_key();
    let list: Vec<Value> = items
        .iter()
        .map(|k| {
            let is_default = if !prefs.default_key_id.is_empty() {
                k.id == prefs.default_key_id
            } else {
                effective.as_deref() == Some(k.key.as_str())
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
    Ok(json!({
        "keys": list,
        "default_key_id": prefs.default_key_id,
        "default_identity_id": prefs.default_identity_id,
    }))
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

/// Re-issue an account's key, returning the new plaintext once.
///
/// The only remedy for an account created before plaintext was kept: its hash
/// cannot produce the key back, so "write this gateway into my client config"
/// could never use it.
pub fn reissue_client_account_key(id: &str) -> Result<Value> {
    let (account, key) = client_accounts::reissue_key(id)?;
    Ok(json!({
        "account": client_account_json(&account, crate::util::unix_millis()),
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

// ── DSH / points ───────────────────────────────────────────────────────────

/// Report the DSH provider wiring so the console can show the current state.
pub fn read_dsh_config() -> Result<Value> {
    let Some(patch_path) = crate::dsh_config::patch_path() else {
        return Ok(json!({
            "supported": false,
            "reason": "DSH_HOME / HOME is not set",
        }));
    };
    let credentials_path = crate::dsh_config::credentials_path();
    if !patch_path.exists() {
        return Ok(json!({
            "supported": true,
            "profile": crate::dsh_config::active_profile(),
            "settings_path": patch_path.display().to_string(),
            "settings_exists": false,
            "provider_exists": false,
            "model_count": 0,
            "base_url": null,
            "credential_present": false,
            "session_header": false,
        }));
    }
    let settings_text =
        std::fs::read_to_string(&patch_path).map_err(|e| anyhow!("读取 DSH 配置失败: {e}"))?;
    let credentials_text = credentials_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok());
    let state = crate::dsh_config::read_state(&settings_text, credentials_text.as_deref());
    Ok(json!({
        "supported": true,
        "profile": crate::dsh_config::active_profile(),
        "settings_path": patch_path.display().to_string(),
        "settings_exists": true,
        "provider_exists": state.provider_exists,
        "model_count": state.model_ids.len(),
        "base_url": state.base_url,
        "credential_present": state.credential_present,
        "session_header": state.session_header,
    }))
}

/// Write this proxy into the active DSH profile's patch file.
pub async fn apply_dsh_config(
    state: &std::sync::Arc<crate::admin_routes::AdminState>,
) -> Result<Value> {
    let Some(patch_path) = crate::dsh_config::patch_path() else {
        bail!("DSH_HOME / HOME 未设置，无法定位 DSH 配置");
    };
    let Some(credentials_path) = crate::dsh_config::credentials_path() else {
        bail!("DSH_HOME / HOME 未设置，无法定位 DSH 凭据文件");
    };
    let settings = GuiSettings::load();
    let preset = settings.models_preset();
    if settings.api_key.is_empty() {
        bail!("尚未配置 API Key，无法拉取模型列表");
    }
    let client = reqwest::Client::new();
    let models = crate::providers::fetch_models(&client, &preset, &settings.api_key)
        .await
        .map_err(|e| anyhow!("从上游 {} 拉取模型失败，未改动 DSH 配置: {e}", preset.id))?;
    if models.is_empty() {
        bail!("上游 {} 未返回任何模型，未改动 DSH 配置", preset.id);
    }
    let base_url = format!("http://127.0.0.1:{}/v1", state.process.port());
    let dsh_models: Vec<crate::dsh_config::DshModel> =
        models.iter().map(crate::dsh_config::to_dsh_model).collect();
    let (patch, credentials, credential_added) =
        crate::dsh_config::apply(&patch_path, &credentials_path, &base_url, &dsh_models)
            .map_err(|e| anyhow!("写入 DSH 配置失败: {e}"))?;
    Ok(json!({
        "ok": true,
        "models": dsh_models.len(),
        "profile": crate::dsh_config::active_profile(),
        "settings_path": patch.display().to_string(),
        "credentials_path": credentials.display().to_string(),
        "credential_added": credential_added,
    }))
}

/// Refresh every identity's point balance.
pub async fn refresh_all_points(
    state: &std::sync::Arc<crate::admin_routes::AdminState>,
) -> Result<Value> {
    let report = workbuddy_auth::refresh_all_points(&state.client).await;
    let known = report.results.iter().filter(|r| r.points.is_some()).count();
    state
        .logs
        .push(
            "INFO",
            format!(
                "已刷新身份池积分: 共 {} 个（账号+密钥），{} 个读取成功",
                report.results.len(),
                known
            ),
        )
        .await;
    Ok(json!({
        "results": report.results,
        "current_identity": current_identity_summary(),
    }))
}

/// Refresh one identity's point balance.
pub async fn refresh_one_points(
    state: &std::sync::Arc<crate::admin_routes::AdminState>,
    id: &str,
) -> Result<Value> {
    let Some(result) = workbuddy_auth::refresh_one_points(&state.client, id).await else {
        bail!("身份不存在: {id}");
    };
    state
        .logs
        .push(
            "INFO",
            format!(
                "已刷新积分: {} ({}) → {}",
                result.label,
                result.id,
                match result.points {
                    Some(p) => format!("{p:.2}"),
                    None => "读取失败".to_string(),
                }
            ),
        )
        .await;
    Ok(json!({
        "result": result,
        "current_identity": current_identity_summary(),
    }))
}

// ── Local client sessions (Claude Code) ───────────────────────────────────

/// A local-session source descriptor the console renders as a source segment.
///
/// DSH is listed even though it is not readable yet: an absent control reads
/// as a missing feature, a disabled one with a reason reads as a known limit.
fn session_sources() -> Value {
    json!([
        { "kind": "claude_local", "label": "Claude Code", "available": true, "note": Value::Null },
        {
            "kind": "dsh_local",
            "label": "DSH",
            "available": false,
            "note": "DSH 会话是 zstd 压缩的本地存档，暂不支持浏览"
        }
    ])
}

fn sessions_unavailable(reason: String) -> Value {
    json!({
        "available": false,
        "reason": reason,
        "root": Value::Null,
        "sources": session_sources(),
        "stats_available": false,
        "total": 0,
        "offset": 0,
        "limit": 0,
        "sessions": [],
    })
}

/// One filesystem pass over the page's transcripts, on a blocking thread.
async fn read_session_page(
    args: claude_sessions::ListArgs,
) -> Result<Option<claude_sessions::ListPage>> {
    let Some(src) = claude_sessions::ClaudeSessionsSource::from_env() else {
        return Ok(None);
    };
    let page = tokio::task::spawn_blocking(move || claude_sessions::list_sessions(&src, &args))
        .await
        .map_err(|e| anyhow!("会话读取异常终止: {e}"))?
        .map_err(|e| anyhow!("读取会话失败: {e}"))?;
    Ok(Some(page))
}

/// Gateway totals for the page's ids, on their own blocking thread.
///
/// An error is reported per page (`gateway_error`) rather than failing the
/// whole request: the local transcript is still worth showing when SQLite is
/// locked or not yet open.
async fn gateway_totals(
    stats: Option<Arc<StatsDb>>,
    ids: Vec<String>,
) -> (bool, Option<String>, Vec<claude_sessions::GatewayUsage>) {
    let Some(stats) = stats else {
        return (false, None, Vec::new());
    };
    if ids.is_empty() {
        return (true, None, Vec::new());
    }
    match tokio::task::spawn_blocking(move || stats.session_usage_totals_for(&ids)).await {
        Ok(Ok(totals)) => (true, None, totals),
        Ok(Err(e)) => (true, Some(format!("网关用量查询失败: {e}")), Vec::new()),
        Err(e) => (true, Some(format!("网关用量查询异常终止: {e}")), Vec::new()),
    }
}

/// The 会话 page: local Claude Code conversations with gateway usage joined
/// by `claude:<uuid>`.
pub async fn claude_sessions_list(
    stats: Option<Arc<StatsDb>>,
    args: claude_sessions::ListArgs,
) -> Result<Value> {
    let Some(page) = read_session_page(args).await? else {
        return Ok(sessions_unavailable(
            "未找到 ~/.claude/projects：当前用户没有 Claude Code 本地会话记录".to_string(),
        ));
    };
    let ids = claude_sessions::gateway_ids(&page.sessions);
    let (stats_available, gateway_error, totals) = gateway_totals(stats, ids).await;
    let mut rows = page.sessions;
    claude_sessions::attach_gateway(&mut rows, &totals);
    Ok(json!({
        "available": true,
        "reason": Value::Null,
        "root": page.root,
        "sources": session_sources(),
        "stats_available": stats_available,
        "gateway_error": gateway_error,
        "total": page.total,
        "offset": page.offset,
        "limit": page.limit,
        "sessions": rows,
    }))
}

/// One session's full transcript (the newest page of entries) plus totals.
pub async fn claude_session_detail(
    stats: Option<Arc<StatsDb>>,
    id: String,
    args: claude_sessions::DetailArgs,
) -> Result<Value> {
    let src = claude_sessions::ClaudeSessionsSource::from_env()
        .ok_or_else(|| anyhow!("未找到 ~/.claude/projects"))?;
    let sid = id.clone();
    let mut detail =
        tokio::task::spawn_blocking(move || claude_sessions::read_session(&src, &sid, &args))
            .await
            .map_err(|e| anyhow!("会话读取异常终止: {e}"))?
            .map_err(|e| anyhow!("{e}"))?;
    let ids = vec![format!("claude:{}", detail.summary.id)];
    let (_, gateway_error, totals) = gateway_totals(stats, ids).await;
    detail.gateway = totals.into_iter().next();
    detail.gateway_error = gateway_error;
    Ok(serde_json::to_value(detail)?)
}

/// One subagent transcript under a session.
pub async fn claude_subagent_detail(
    id: String,
    agent: String,
    args: claude_sessions::DetailArgs,
) -> Result<Value> {
    let src = claude_sessions::ClaudeSessionsSource::from_env()
        .ok_or_else(|| anyhow!("未找到 ~/.claude/projects"))?;
    let detail = tokio::task::spawn_blocking(move || {
        claude_sessions::read_subagent(&src, &id, &agent, &args)
    })
    .await
    .map_err(|e| anyhow!("会话读取异常终止: {e}"))?
    .map_err(|e| anyhow!("{e}"))?;
    Ok(serde_json::to_value(detail)?)
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
