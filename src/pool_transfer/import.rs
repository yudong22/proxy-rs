//! Decoding a bundle, planning the merge against local state, and committing it.

use super::*;

/// How an import treats what is already on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    /// Upsert by id; everything else on this machine is left alone.
    Merge,
    /// The bundle becomes the whole pool; existing entries are dropped.
    Replace,
}

impl ImportMode {
    fn as_str(self) -> &'static str {
        match self {
            ImportMode::Merge => "merge",
            ImportMode::Replace => "replace",
        }
    }

    /// Parse the GUI's `mode` argument. Unknown values are rejected rather than
    /// silently treated as "merge": an import that does the wrong thing to a
    /// credential pool is not something to guess about.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim() {
            "merge" | "" => Ok(ImportMode::Merge),
            "replace" => Ok(ImportMode::Replace),
            other => bail!("未知的导入模式: {other}"),
        }
    }
}

/// Read-only summary of what applying a bundle would do. Deliberately contains
/// no token, no key and no label beyond what the user already sees in the pool
/// list — it is rendered in the GUI and, on a bad day, pasted into an issue.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportPreview {
    pub mode: String,
    pub encrypted: bool,
    pub credentials_total: usize,
    pub credentials_added: usize,
    pub credentials_updated: usize,
    pub credentials_unchanged: usize,
    /// Replace mode only: local entries the bundle does not mention.
    pub credentials_removed: usize,
    pub keys_total: usize,
    pub keys_added: usize,
    pub keys_updated: usize,
    pub keys_unchanged: usize,
    pub keys_removed: usize,
    /// Inbound gateway accounts in the bundle / affected by this import.
    #[serde(default)]
    pub client_accounts_total: usize,
    #[serde(default)]
    pub client_accounts_added: usize,
    #[serde(default)]
    pub client_accounts_updated: usize,
    #[serde(default)]
    pub client_accounts_unchanged: usize,
    #[serde(default)]
    pub client_accounts_removed: usize,
    /// The default identity that will be in force after the import, or empty
    /// for "no explicit default (first usable wins)".
    pub default_identity_applied: String,
    /// True when a bundle default was dropped because it names nothing.
    pub default_identity_dropped: bool,
    pub checkin_enabled: Option<bool>,
    pub checkin_time: Option<String>,
    /// Non-fatal findings: checksum mismatch, id recomputed, unknown fields.
    pub warnings: Vec<String>,
}

/// The three stores as they will be written. Built by `plan`, committed by
/// `commit`; `preview_import` stops after building it.
struct PlannedState {
    credentials: Vec<WorkBuddyCredential>,
    api_keys: Vec<ApiKeyEntry>,
    client_accounts: Vec<crate::client_accounts::ClientAccount>,
    prefs: PoolPreferences,
    /// Explicit legacy pointers (`default_credential_id` / `default_key_id`).
    /// Cleared on replace so a stale pointer cannot survive its target.
    clear_legacy_defaults: bool,
}

/// A decoded bundle, plus anything worth telling the user about the decoding.
struct Decoded {
    payload: BundlePayload,
    encrypted: bool,
    checksum: Option<String>,
    warnings: Vec<String>,
}

// ── Export ─────────────────────────────────────────────────────────────────

/// Normalize one credential for travel: drop the state that describes *this*
/// machine's recent trouble, keep everything that describes the identity.
///
/// A cooldown is a rate-limit memory from the exporting machine's network, and
/// `last_error` is the text of that failure — carrying either to a fresh machine
/// would make a healthy account look broken there for no reason.
pub(crate) fn normalize_credential(c: &WorkBuddyCredential) -> WorkBuddyCredential {
    let mut out = c.clone();
    out.cooldown_until_ms = None;
    out.last_error.clear();
    out
}

/// Parse (and if needed decrypt) a bundle, checking format-level invariants.
/// Does not touch the local stores.
fn decode(text: &str, passphrase: Option<&str>) -> Result<Decoded> {
    if text.len() > MAX_BUNDLE_BYTES {
        bail!(
            "文件过大（{} 字节，上限 {} 字节），这不是账号池导出文件",
            text.len(),
            MAX_BUNDLE_BYTES
        );
    }
    let bundle: BundleFile =
        serde_json::from_str(text).context("无法解析导出文件：不是有效的 JSON")?;

    if bundle.format != BUNDLE_FORMAT {
        bail!(
            "这不是 Proxy RS 账号池导出文件（format = \"{}\"）",
            bundle.format
        );
    }
    let mut warnings = Vec::new();
    if bundle.version > BUNDLE_VERSION {
        // A newer writer may have added fields this build ignores. Importing the
        // part it does understand is more useful than refusing outright, but the
        // user has to know the result may be incomplete.
        warnings.push(format!(
            "该文件由更新版本的 Proxy RS 导出 (格式版本 {})，本版本按格式版本 {} 读取，可能丢失部分内容",
            bundle.version, BUNDLE_VERSION
        ));
    }

    let (payload, encrypted) = match bundle.kind.as_str() {
        KIND_PLAIN => (
            BundlePayload {
                preferences: bundle.preferences.clone().unwrap_or_default(),
                credentials: bundle.credentials.clone(),
                api_keys: bundle.api_keys.clone(),
                client_accounts: bundle.client_accounts.clone(),
            },
            false,
        ),
        KIND_ENCRYPTED => {
            let Some(passphrase) = passphrase.map(str::trim).filter(|p| !p.is_empty()) else {
                bail!("该导出文件已加密，请输入口令");
            };
            let kdf = bundle.kdf.as_ref().context("加密文件缺少口令派生参数")?;
            let blob = bundle.cipher.as_ref().context("加密文件缺少密文")?;
            (decrypt_payload(passphrase, kdf, blob)?, true)
        }
        other => bail!("未知的导出文件类型: {other}"),
    };

    Ok(Decoded {
        payload,
        encrypted,
        checksum: bundle.checksum.clone(),
        warnings,
    })
}

/// Re-key imported credentials to the stable account identity, and repair every
/// reference to a re-keyed id.
///
/// History: this used to force `id == credential_id(access_token)`. That rule
/// is wrong for any account that has ever been refreshed — the token rotates,
/// the hash changes, but the account does not — so the function rewrote the ids
/// of perfectly good credentials *and* left `default_identity_id` pointing at
/// the old value, silently clearing the user's chosen default.
///
/// Now it re-keys to [`credential_id_for`] (uid-based when available) and:
///
/// * remaps `preferences.default_identity_id` (and the legacy per-type pointer)
///   through the same rename table, so the default survives the import;
/// * keeps the stored id when the account is already keyed correctly, so a
///   round trip is a no-op rather than a rewrite;
/// * warns only when it actually changed something.
fn reconcile_ids(payload: &mut BundlePayload, warnings: &mut Vec<String>) {
    let mut renames: Vec<(String, String)> = Vec::new();
    for c in payload.credentials.iter() {
        let stable = crate::workbuddy_auth::credential_id_for(&c.account, &c.access_token);
        if stable != c.id {
            renames.push((c.id.clone(), stable));
        }
    }

    for (old, new) in &renames {
        for c in payload.credentials.iter_mut() {
            if c.id == *old {
                c.id = new.clone();
            }
        }
    }
    if !renames.is_empty() {
        warnings.push(format!(
            "已按账号身份（uid）重新计算 {} 个账号的 id，并同步更新默认身份引用",
            renames.len()
        ));
    }

    // Rewrite the default pointer through the rename table instead of letting it
    // dangle. Without this the account keeps existing but stops being the
    // default — a silent, confusing side effect of importing your own export.
    let remap = |id: &str| -> Option<String> {
        renames
            .iter()
            .find(|(old, _)| old == id)
            .map(|(_, new)| new.clone())
    };
    if let Some(id) = payload.preferences.default_identity_id.clone() {
        if let Some(new) = remap(&id) {
            payload.preferences.default_identity_id = Some(new);
        }
    }

    for k in payload.api_keys.iter_mut() {
        let expected = api_key_id(&k.key);
        if k.id != expected {
            warnings.push(format!(
                "密钥 {} 的 id 与内容不一致，已重新计算为 {}",
                if k.id.is_empty() { "(空)" } else { &k.id },
                expected
            ));
            k.id = expected;
        }
    }
    // Gateway accounts: the id must be `ca-<first 6 hex of key_hash>`. This can
    // be checked without any plaintext — which is the whole reason the id is
    // derived from the hash rather than the key.
    for account in payload.client_accounts.iter_mut() {
        let expected = crate::client_accounts::id_from_hash(&account.key_hash);
        if account.id != expected {
            warnings.push(format!(
                "访问账户 {} 的 id 与密钥哈希不一致，已重新计算为 {}",
                if account.id.is_empty() {
                    "(空)"
                } else {
                    &account.id
                },
                expected
            ));
            account.id = expected;
        }
    }

    // A duplicate id inside one bundle collapses to the last occurrence, which
    // is exactly what a set of upserts would do anyway.
    let mut seen = std::collections::HashSet::new();
    if payload
        .credentials
        .iter()
        .any(|c| !seen.insert(c.id.clone()))
    {
        let mut deduped: Vec<WorkBuddyCredential> = Vec::new();
        for c in payload.credentials.drain(..) {
            match deduped.iter_mut().find(|e| e.id == c.id) {
                Some(slot) => *slot = c,
                None => deduped.push(c),
            }
        }
        warnings.push("导出文件内存在重复账号，已按 id 去重".to_string());
        payload.credentials = deduped;
    }
    let mut seen = std::collections::HashSet::new();
    if payload.api_keys.iter().any(|k| !seen.insert(k.id.clone())) {
        let mut deduped: Vec<ApiKeyEntry> = Vec::new();
        for k in payload.api_keys.drain(..) {
            match deduped.iter_mut().find(|e| e.id == k.id) {
                Some(slot) => *slot = k,
                None => deduped.push(k),
            }
        }
        warnings.push("导出文件内存在重复密钥，已按 id 去重".to_string());
        payload.api_keys = deduped;
    }
    let mut seen = std::collections::HashSet::new();
    if payload
        .client_accounts
        .iter()
        .any(|a| !seen.insert(a.id.clone()))
    {
        let mut deduped: Vec<crate::client_accounts::ClientAccount> = Vec::new();
        for a in payload.client_accounts.drain(..) {
            match deduped.iter_mut().find(|e| e.id == a.id) {
                Some(slot) => *slot = a,
                None => deduped.push(a),
            }
        }
        warnings.push("导出文件内存在重复访问账户，已按 id 去重".to_string());
        payload.client_accounts = deduped;
    }
}

/// Drop entries that cannot authenticate: a credential with an empty token or a
/// key with an empty key string would be written to the pool and then never
/// usable, and would keep showing up in the list as a mysterious failure.
fn drop_empty(payload: &mut BundlePayload, warnings: &mut Vec<String>) {
    let before = payload.credentials.len();
    payload
        .credentials
        .retain(|c| !c.access_token.trim().is_empty());
    if before != payload.credentials.len() {
        warnings.push(format!(
            "已跳过 {} 个缺少 access token 的账号",
            before - payload.credentials.len()
        ));
    }
    let before = payload.api_keys.len();
    payload.api_keys.retain(|k| !k.key.trim().is_empty());
    if before != payload.api_keys.len() {
        warnings.push(format!(
            "已跳过 {} 个内容为空的密钥",
            before - payload.api_keys.len()
        ));
    }
    // An account with no hash can never authenticate anything, so importing it
    // would only plant a row that looks usable and is not.
    let before = payload.client_accounts.len();
    payload
        .client_accounts
        .retain(|a| !a.key_hash.trim().is_empty());
    if before != payload.client_accounts.len() {
        warnings.push(format!(
            "已跳过 {} 个缺少密钥哈希的访问账户",
            before - payload.client_accounts.len()
        ));
    }
}

// ── Planning ───────────────────────────────────────────────────────────────

/// Fold the bundle into the local stores, purely (no writes).
fn plan(decoded: &Decoded, mode: ImportMode) -> Result<(ImportPreview, PlannedState)> {
    let mut payload = decoded.payload.clone();
    let mut warnings = decoded.warnings.clone();

    // Verify the checksum against the payload as it arrived, before any
    // normalization rewrites a field. A mismatch means the file was edited by
    // hand after export; that is not a reason to refuse a usable account, but it
    // is a reason to say so, because the edit is exactly what the user needs to
    // know about when the import result surprises them.
    if let Some(expected) = decoded.checksum.as_deref() {
        match payload_checksum(&payload) {
            Ok(actual) if actual != expected => warnings.push(
                "文件校验和与内容不一致：导出文件可能被手工修改过，请确认内容无误".to_string(),
            ),
            Err(e) => warnings.push(format!("无法校验文件完整性: {e}")),
            _ => {}
        }
    }

    reconcile_ids(&mut payload, &mut warnings);
    drop_empty(&mut payload, &mut warnings);

    let local_creds = workbuddy_auth::load_credentials();
    let local_keys = workbuddy_auth::load_api_keys();
    let local_accounts = crate::client_accounts::load_accounts();
    let local_prefs = workbuddy_auth::load_preferences();

    let mut credentials: Vec<WorkBuddyCredential> = match mode {
        ImportMode::Merge => local_creds.clone(),
        ImportMode::Replace => Vec::new(),
    };
    let mut api_keys: Vec<ApiKeyEntry> = match mode {
        ImportMode::Merge => local_keys.clone(),
        ImportMode::Replace => Vec::new(),
    };
    let mut client_accounts: Vec<crate::client_accounts::ClientAccount> = match mode {
        ImportMode::Merge => local_accounts.clone(),
        ImportMode::Replace => Vec::new(),
    };

    let mut credentials_added = 0;
    let mut credentials_updated = 0;
    let mut credentials_unchanged = 0;
    for incoming in &payload.credentials {
        // Match by account identity, not just id. A LOCAL entry written before
        // the stable-identity change still carries a token-hash id, while the
        // bundle's entry for the same account is keyed by uid; comparing ids
        // alone would import it as a second row. The stored id is kept on a
        // match (see `find_index`) so existing references keep resolving.
        let existing = workbuddy_auth::find_index(&credentials, incoming);
        // Existing entries keep their refresh material when the bundle does not
        // carry any: this machine may hold a fresher refresh token than the
        // bundle, and dropping it would force a re-login for no reason.
        let merged = match existing {
            Some(i) => {
                let mut m = preserve_local_secrets(incoming, &credentials[i]);
                m.id = credentials[i].id.clone();
                m
            }
            None => incoming.clone(),
        };
        match existing {
            Some(i) => {
                if records_equal(&credentials[i], &merged) {
                    credentials_unchanged += 1;
                } else {
                    credentials_updated += 1;
                }
                credentials[i] = merged;
            }
            None => {
                credentials.push(merged);
                credentials_added += 1;
            }
        }
    }

    let mut keys_added = 0;
    let mut keys_updated = 0;
    let mut keys_unchanged = 0;
    for incoming in &payload.api_keys {
        match api_keys.iter_mut().find(|k| k.id == incoming.id) {
            Some(slot) => {
                if records_equal(slot, incoming) {
                    keys_unchanged += 1;
                } else {
                    keys_updated += 1;
                }
                *slot = incoming.clone();
            }
            None => {
                api_keys.push(incoming.clone());
                keys_added += 1;
            }
        }
    }

    let mut client_accounts_added = 0;
    let mut client_accounts_updated = 0;
    let mut client_accounts_unchanged = 0;
    for incoming in &payload.client_accounts {
        // Matched by id, like API keys: the id is a hash prefix, so it is both
        // stable across machines and unique per key material.
        match client_accounts.iter_mut().find(|a| a.id == incoming.id) {
            Some(slot) => {
                if records_equal(slot, incoming) {
                    client_accounts_unchanged += 1;
                } else {
                    client_accounts_updated += 1;
                }
                *slot = incoming.clone();
            }
            None => {
                client_accounts.push(incoming.clone());
                client_accounts_added += 1;
            }
        }
    }

    let credentials_removed = match mode {
        ImportMode::Replace => local_creds.len(),
        ImportMode::Merge => 0,
    };
    let keys_removed = match mode {
        ImportMode::Replace => local_keys.len(),
        ImportMode::Merge => 0,
    };
    let client_accounts_removed = match mode {
        ImportMode::Replace => local_accounts.len(),
        ImportMode::Merge => 0,
    };

    // ── Preferences ──
    let mut prefs = local_prefs.clone();
    prefs.daily_checkin_enabled = payload
        .preferences
        .daily_checkin_enabled
        .unwrap_or(local_prefs.daily_checkin_enabled);
    prefs.daily_checkin_time = payload
        .preferences
        .daily_checkin_time
        .as_ref()
        .map(|t| workbuddy_auth::normalize_checkin_time(t))
        .unwrap_or_else(|| local_prefs.daily_checkin_time.clone());

    let known = |id: &str, creds: &[WorkBuddyCredential], keys: &[ApiKeyEntry]| {
        id.is_empty()
            || id == workbuddy_auth::NO_ACCOUNT_POOL_SENTINEL
            || creds.iter().any(|c| c.id == id)
            || keys.iter().any(|k| k.id == id)
    };

    let mut default_identity_dropped = false;
    // `None` means the bundle had no opinion, so the local default stands —
    // unless replace mode just deleted its target, in which case a dangling
    // pointer would silently fall through to the first usable entry anyway.
    // Clearing it keeps the GUI honest about what is in force.
    let candidate = payload
        .preferences
        .default_identity_id
        .as_deref()
        .map(str::to_string);
    match candidate {
        Some(id) if known(&id, &credentials, &api_keys) => {
            prefs.default_identity_id = id;
        }
        Some(id) => {
            warnings.push(format!(
                "导出文件中的默认身份 {id} 不在导入内容中，已清空默认身份"
            ));
            prefs.default_identity_id.clear();
            default_identity_dropped = true;
        }
        None => {
            if !prefs.default_identity_id.is_empty()
                && !known(&prefs.default_identity_id, &credentials, &api_keys)
            {
                warnings.push(format!(
                    "本机默认身份 {} 不在导入后的身份池中，已清空默认身份",
                    prefs.default_identity_id
                ));
                prefs.default_identity_id.clear();
                default_identity_dropped = true;
            }
        }
    }
    if mode == ImportMode::Replace
        && (!prefs.default_credential_id.is_empty() || !prefs.default_key_id.is_empty())
    {
        warnings.push("替换导入已清除旧版默认账号/密钥指针".to_string());
    }

    let preview = ImportPreview {
        mode: mode.as_str().to_string(),
        encrypted: decoded.encrypted,
        credentials_total: payload.credentials.len(),
        credentials_added,
        credentials_updated,
        credentials_unchanged,
        credentials_removed,
        keys_total: payload.api_keys.len(),
        keys_added,
        keys_updated,
        keys_unchanged,
        keys_removed,
        client_accounts_total: payload.client_accounts.len(),
        client_accounts_added,
        client_accounts_updated,
        client_accounts_unchanged,
        client_accounts_removed,
        default_identity_applied: prefs.default_identity_id.clone(),
        default_identity_dropped,
        checkin_enabled: payload.preferences.daily_checkin_enabled,
        checkin_time: payload.preferences.daily_checkin_time.clone(),
        warnings,
    };

    let state = PlannedState {
        credentials,
        api_keys,
        client_accounts,
        prefs,
        clear_legacy_defaults: mode == ImportMode::Replace,
    };
    Ok((preview, state))
}

/// Keep this machine's fresher login material when the bundle has none.
fn preserve_local_secrets(
    incoming: &WorkBuddyCredential,
    local: &WorkBuddyCredential,
) -> WorkBuddyCredential {
    let mut merged = incoming.clone();
    if merged.refresh_token.is_none() {
        merged.refresh_token = local.refresh_token.clone();
    }
    if merged.expires_at_ms.is_none() {
        merged.expires_at_ms = local.expires_at_ms;
    }
    if merged.machine_id.trim().is_empty() {
        merged.machine_id = local.machine_id.clone();
    }
    merged
}

/// Structural equality, used only to distinguish "updated" from "unchanged" in
/// the preview. Serializing is both simpler and stricter than a field-by-field
/// comparison that a future field would silently escape.
fn records_equal<T: Serialize>(a: &T, b: &T) -> bool {
    match (serde_json::to_value(a), serde_json::to_value(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

// ── Public entry points ────────────────────────────────────────────────────

/// Read-only: decode the bundle and report what importing it would change.
pub fn preview_import(
    text: &str,
    passphrase: Option<&str>,
    mode: ImportMode,
) -> Result<ImportPreview> {
    let decoded = decode(text, passphrase)?;
    let (preview, _) = plan(&decoded, mode)?;
    Ok(preview)
}

/// Decode, plan, then write. Re-decodes instead of accepting a previous
/// preview's result: the file could have been re-exported (or edited) between
/// the two calls, and the write must reflect what is on screen *now*.
pub fn apply_import(
    text: &str,
    passphrase: Option<&str>,
    mode: ImportMode,
) -> Result<ImportPreview> {
    let decoded = decode(text, passphrase)?;
    let (preview, state) = plan(&decoded, mode)?;
    commit(&state)?;
    Ok(preview)
}

/// Write the three stores, backing each one up first.
fn commit(state: &PlannedState) -> Result<()> {
    let mut prefs = state.prefs.clone();
    if state.clear_legacy_defaults {
        prefs.default_credential_id.clear();
        prefs.default_key_id.clear();
    }

    // Credentials first: if a later write fails, the pool is at least consistent
    // about which accounts exist, and the failure is reported rather than
    // swallowed.
    write_json_atomic(
        &workbuddy_auth::credentials_path(),
        &state.credentials,
        true,
    )?;
    write_json_atomic(&workbuddy_auth::api_keys_path(), &state.api_keys, true)?;
    // Client accounts are written last: they are the newest store, so an
    // interrupted import leaves the two long-standing pools consistent first.
    write_json_atomic(
        &crate::client_accounts::accounts_path(),
        &state.client_accounts,
        true,
    )?;
    write_json_atomic(&workbuddy_auth::preferences_path(), &prefs, false)?;
    Ok(())
}
