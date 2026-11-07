//! Inbound client access-key accounts.

use super::*;

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
