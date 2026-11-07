//! See the module table in `workbuddy_auth/mod.rs` for what lives here.

use super::*;

/// Credentials file path: `~/.proxy-rs/workbuddy-credentials.json` (relocated by
/// `PROXY_DATA_DIR` like every other state file).
pub fn credentials_path() -> PathBuf {
    crate::settings::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("workbuddy-credentials.json")
}

/// Load every stored credential. A missing file is an empty store, not an error.
pub fn load_credentials() -> Vec<WorkBuddyCredential> {
    let Ok(text) = std::fs::read_to_string(credentials_path()) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Persist every credential. The file holds live tokens, so it is written
/// 0600 — the same protection workbuddy2api applies to its session file.
pub fn save_credentials(items: &[WorkBuddyCredential]) -> Result<()> {
    let path = credentials_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(items)?;
    std::fs::write(&path, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// True when two credentials describe the same account.
///
/// The id is the primary key, but it is not sufficient on its own: an entry
/// stored before the stable-identity change carries a token-hash id, and a
/// freshly parsed login state for that same account now derives an id from
/// `uid`. Matching on the account identity as well is what makes re-login an
/// update of the old row instead of a second row beside it.
///
/// `pub(crate)` because the import path needs the same notion of "same account"
/// when folding a bundle into the local store.
pub(crate) fn same_account(a: &WorkBuddyCredential, b: &WorkBuddyCredential) -> bool {
    if a.id == b.id {
        return true;
    }
    match (identity_key(&a.account), identity_key(&b.account)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Find the index of the entry for an account, by id then by account identity.
pub(crate) fn find_index(
    items: &[WorkBuddyCredential],
    item: &WorkBuddyCredential,
) -> Option<usize> {
    if let Some(i) = items.iter().position(|c| c.id == item.id) {
        return Some(i);
    }
    items.iter().position(|c| same_account(c, item))
}

/// Upsert one credential, matched by id and then by account identity.
///
/// Replacing an entry found by identity keeps its id: the id is what request
/// logs, override attribution and the configured default already refer to, so
/// re-pointing it at a new value would strand all three (see
/// [`reconcile_ids_after_import`], which exists for the same reason).
pub fn upsert_credential(item: WorkBuddyCredential) -> Result<WorkBuddyCredential> {
    let mut items = load_credentials();
    match find_index(&items, &item) {
        Some(i) => {
            let mut merged = item;
            // The stored id wins, so a stale pointer keeps resolving.
            merged.id = items[i].id.clone();
            items[i] = merged.clone();
            save_credentials(&items)?;
            Ok(merged)
        }
        None => {
            items.push(item.clone());
            save_credentials(&items)?;
            Ok(item)
        }
    }
}

/// Re-key every stored credential to the stable identity, once.
///
/// Ids created before the stable-identity change are token hashes. Any account
/// that has since been refreshed no longer matches its own id, so re-login would
/// duplicate it and an imported bundle would rewrite it (see
/// [`crate::pool_transfer`]). Re-keying to `uid`-based ids fixes that going
/// forward, and this pass migrates what is already on disk.
///
/// It also rewrites any reference to a changed id, so the configured default
/// identity keeps naming the same account instead of being cleared.
///
/// Returns the number of credentials re-keyed. Idempotent: a second call finds
/// nothing to change. Entries without a `uid` are left alone.
pub fn migrate_credential_ids() -> Result<usize> {
    let mut items = load_credentials();
    if items.is_empty() {
        return Ok(0);
    }

    // Collect the id renames first, so every reference can be rewritten from one
    // consistent map rather than from the partially-mutated vec.
    let mut renames: Vec<(String, String)> = Vec::new();
    for c in items.iter() {
        let stable = credential_id_for(&c.account, &c.access_token);
        if stable != c.id {
            renames.push((c.id.clone(), stable));
        }
    }
    if renames.is_empty() {
        return Ok(0);
    }

    for (old, new) in &renames {
        for c in items.iter_mut() {
            if c.id == *old {
                c.id = new.clone();
            }
        }
    }

    // Two entries can collapse onto one stable id when the same account was
    // imported twice under different tokens (exactly the duplicate this change
    // prevents). Keep the most recently used one.
    let mut seen: Vec<WorkBuddyCredential> = Vec::with_capacity(items.len());
    for c in items.into_iter() {
        match seen.iter().position(|e| e.id == c.id) {
            Some(i) => {
                if c.last_checkin_date > seen[i].last_checkin_date
                    || (c.last_checkin_date == seen[i].last_checkin_date
                        && c.points_fetched_at_ms > seen[i].points_fetched_at_ms)
                {
                    seen[i] = c;
                }
            }
            None => seen.push(c),
        }
    }

    save_credentials(&seen)?;

    let mut prefs = load_preferences();
    let mut prefs_changed = false;
    if let Some((_, new)) = renames
        .iter()
        .find(|(old, _)| *old == prefs.default_identity_id)
    {
        prefs.default_identity_id = new.clone();
        prefs_changed = true;
    }
    if let Some((_, new)) = renames
        .iter()
        .find(|(old, _)| *old == prefs.default_credential_id)
    {
        prefs.default_credential_id = new.clone();
        prefs_changed = true;
    }
    if prefs_changed {
        save_preferences(&prefs)?;
    }

    Ok(renames.len())
}
