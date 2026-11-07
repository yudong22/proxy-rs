//! The credential / api-key pool: add, remove, toggle, set default, reorder.

use super::*;

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
