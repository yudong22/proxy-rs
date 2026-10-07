//! The point of the export bundle: *another machine's* pool is exactly what the
//! exporting one had.
//!
//! The unit tests in `src/pool_transfer.rs` check the format and the merge
//! rules. This test checks the thing the feature is actually for — that the
//! round trip through a file is enough to reconstitute what the proxy reads at
//! request time (`active_api_key`, `effective_default_identity`,
//! `ordered_credentials`), including on a machine that has never seen these
//! accounts.
//!
//! It therefore goes through the real files (`PROXY_DATA_DIR` → temp directory),
//! the real `export_text`/`apply_import`, and the same accessors the request path
//! uses — not through the in-memory payload structs.

use proxy_rs::pool_transfer::{apply_import, export_text, ExportOptions, ImportMode};
use proxy_rs::workbuddy_auth::{
    self, api_key_id, credential_id_for, ApiKeyEntry, PoolPreferences, WorkBuddyAccount,
    WorkBuddyCredential,
};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// The two env vars `settings::data_dir()` honours. Spelled out here rather
/// than imported: integration tests compile against the public surface only, and
/// these names are the contract (`task dev` sets the first one too).
const DATA_DIR_ENV: &str = "PROXY_DATA_DIR";
const LEGACY_DATA_DIR_ENV: &str = "ANTHROPIC_PROXY_DATA_DIR";

/// `PROXY_DATA_DIR` is process-global, so this file's tests run one at a time.
/// The lock is shared with the library's own data-dir tests through the same
/// env var and, within one test process, the same convention.
fn lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// A relocated data directory that cleans up after itself.
struct TempDataDir {
    _guard: MutexGuard<'static, ()>,
    previous: (Option<std::ffi::OsString>, Option<std::ffi::OsString>),
    dir: PathBuf,
}

impl TempDataDir {
    fn new(name: &str) -> Self {
        let guard = lock();
        let previous = (
            std::env::var_os(DATA_DIR_ENV),
            std::env::var_os(LEGACY_DATA_DIR_ENV),
        );
        let dir =
            std::env::temp_dir().join(format!("proxy-rs-pool-it-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp data dir");
        std::env::remove_var(LEGACY_DATA_DIR_ENV);
        std::env::set_var(DATA_DIR_ENV, &dir);
        Self {
            _guard: guard,
            previous,
            dir,
        }
    }
}

impl Drop for TempDataDir {
    fn drop(&mut self) {
        match &self.previous.0 {
            Some(v) => std::env::set_var(DATA_DIR_ENV, v),
            None => std::env::remove_var(DATA_DIR_ENV),
        }
        match &self.previous.1 {
            Some(v) => std::env::set_var(LEGACY_DATA_DIR_ENV, v),
            None => std::env::remove_var(LEGACY_DATA_DIR_ENV),
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn credential(token: &str, label: &str, uid: &str) -> WorkBuddyCredential {
    let account = WorkBuddyAccount {
        uid: uid.to_string(),
        enterprise_id: "ent-1".to_string(),
        nickname: format!("{label}-nick"),
    };
    WorkBuddyCredential {
        // The stable identity, exactly as `parse_login_state` derives it: the id
        // must follow the *account*, not the token, or a refresh would strand it.
        id: credential_id_for(&account, token),
        label: label.to_string(),
        access_token: token.to_string(),
        refresh_token: Some(format!("{token}-refresh")),
        expires_at_ms: Some(4_000_000_000_000),
        domain: "copilot.tencent.com".to_string(),
        account,
        machine_id: "machine-ported".to_string(),
        enabled: true,
        ..Default::default()
    }
}

fn key(material: &str, label: &str) -> ApiKeyEntry {
    ApiKeyEntry {
        id: api_key_id(material),
        label: label.to_string(),
        key: material.to_string(),
        enabled: true,
        ..Default::default()
    }
}

/// What the request path would see right now.
fn effective_state() -> (String, String, Vec<String>, Vec<String>) {
    (
        workbuddy_auth::active_api_key().unwrap_or_default(),
        workbuddy_auth::effective_default_identity(),
        workbuddy_auth::ordered_credentials()
            .iter()
            .map(|c| c.id.clone())
            .collect(),
        workbuddy_auth::load_api_keys()
            .iter()
            .map(|k| k.id.clone())
            .collect(),
    )
}

#[test]
fn a_plain_bundle_reconstitutes_the_pool_on_a_fresh_machine() {
    let source_dir = TempDataDir::new("source");

    let creds = vec![
        credential("access-token-one", "工作号", "uid-1"),
        credential("access-token-two", "备用号", "uid-2"),
    ];
    let keys = vec![key("sk-primary", "主密钥"), key("sk-backup", "备用密钥")];
    workbuddy_auth::save_credentials(&creds).unwrap();
    workbuddy_auth::save_api_keys(&keys).unwrap();
    // Pin the second credential: ordering *is* the default mechanism, so this
    // is the assertion that proves the choice survives the trip.
    workbuddy_auth::save_preferences(&PoolPreferences {
        default_identity_id: creds[1].id.clone(),
        daily_checkin_enabled: true,
        daily_checkin_time: "07:15".to_string(),
        ..Default::default()
    })
    .unwrap();

    let before = effective_state();
    assert_eq!(before.1, creds[1].id, "sanity: the pinned account wins");
    assert_eq!(
        before.2[0], creds[1].id,
        "sanity: ordered_credentials puts the default first"
    );

    let (text, outcome) = export_text(&ExportOptions::default()).unwrap();
    assert!(!outcome.encrypted);
    drop(source_dir);

    // A different, empty machine.
    let _target_dir = TempDataDir::new("target");
    assert!(workbuddy_auth::load_credentials().is_empty());

    let preview = apply_import(&text, None, ImportMode::Merge).unwrap();
    assert_eq!(preview.credentials_added, 2);
    assert_eq!(preview.keys_added, 2);

    assert_eq!(
        effective_state(),
        before,
        "the request path must see exactly what the exporting machine saw"
    );

    // And the pieces the header builder reads, not just the ids.
    let ported = workbuddy_auth::load_credentials()
        .into_iter()
        .find(|c| c.id == creds[0].id)
        .expect("first credential survives");
    assert_eq!(ported.access_token, "access-token-one");
    assert_eq!(
        ported.refresh_token.as_deref(),
        Some("access-token-one-refresh")
    );
    assert_eq!(ported.account.uid, "uid-1");
    assert_eq!(ported.account.enterprise_id, "ent-1");
    assert_eq!(ported.machine_id, "machine-ported");
    let headers = workbuddy_auth::upstream_headers(&ported);
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "X-User-Id" && v == "uid-1"),
        "an imported account must still present its session headers: {headers:?}"
    );

    let prefs = workbuddy_auth::load_preferences();
    assert!(prefs.daily_checkin_enabled);
    assert_eq!(prefs.daily_checkin_time, "07:15");
}

#[test]
fn an_encrypted_bundle_reconstitutes_the_pool_too() {
    let _source_dir = TempDataDir::new("enc-source");
    let creds = vec![credential("access-token-one", "工作号", "uid-1")];
    workbuddy_auth::save_credentials(&creds).unwrap();
    workbuddy_auth::save_api_keys(&[key("sk-primary", "主密钥")]).unwrap();

    let before = effective_state();
    let passphrase = "一个不太短的口令 with spaces";
    let (text, outcome) = export_text(&ExportOptions {
        passphrase: Some(passphrase.to_string()),
    })
    .unwrap();
    assert!(outcome.encrypted);
    drop(_source_dir);

    let _target_dir = TempDataDir::new("enc-target");

    // Wrong passphrase first: the pool must be untouched afterwards, otherwise
    // a typo would silently half-import.
    assert!(apply_import(&text, Some("not it"), ImportMode::Merge).is_err());
    assert!(workbuddy_auth::load_credentials().is_empty());
    assert!(workbuddy_auth::load_api_keys().is_empty());

    apply_import(&text, Some(passphrase), ImportMode::Merge).unwrap();
    assert_eq!(effective_state(), before);
}

/// The recovery path for an account whose token has been rotated, and the reason
/// the id had to change from a token hash to an account identity.
///
/// This models the real lifecycle: log in → the access token expires and the
/// proxy refreshes it → the stored id no longer equals a hash of the current
/// token. Before the stable-identity change, that state broke two things at once:
/// importing your own export rewrote the id (dropping the default), and logging
/// in again added a duplicate row instead of updating the account.
#[test]
fn a_refreshed_account_survives_export_import_and_relogin() {
    let _dir = TempDataDir::new("refreshed-lifecycle");

    // 1. Log in.
    let account = WorkBuddyAccount {
        uid: "uid-lifecycle".to_string(),
        enterprise_id: "ent-1".to_string(),
        nickname: "甲".to_string(),
    };
    let original = credential("original-token", "工作号", "uid-lifecycle");
    let stable_id = original.id.clone();
    assert_eq!(
        stable_id,
        credential_id_for(&account, "original-token"),
        "fixture sanity: id comes from the account"
    );
    workbuddy_auth::save_credentials(std::slice::from_ref(&original)).unwrap();
    workbuddy_auth::save_preferences(&PoolPreferences {
        default_identity_id: stable_id.clone(),
        ..Default::default()
    })
    .unwrap();

    // 2. A refresh rotates the token. The id must NOT move.
    let rotated = WorkBuddyCredential {
        access_token: "rotated-token".to_string(),
        refresh_token: Some("ref-2".to_string()),
        last_refresh_at_ms: Some(1_790_000_000_000),
        ..original.clone()
    };
    workbuddy_auth::upsert_credential(rotated.clone()).unwrap();

    let after_refresh = workbuddy_auth::load_credentials();
    assert_eq!(
        after_refresh.len(),
        1,
        "a refresh must update, never duplicate"
    );
    assert_eq!(
        after_refresh[0].id, stable_id,
        "the id must survive a token rotation"
    );
    assert_eq!(after_refresh[0].access_token, "rotated-token");

    // 3. Export → import on a fresh machine. The id and the default must hold.
    let (text, _) = export_text(&ExportOptions::default()).unwrap();

    workbuddy_auth::save_credentials(&[]).unwrap();
    workbuddy_auth::save_preferences(&PoolPreferences::default()).unwrap();

    let preview = apply_import(&text, None, ImportMode::Merge).unwrap();
    assert_eq!(preview.credentials_added, 1);
    assert_eq!(
        workbuddy_auth::effective_default_identity(),
        stable_id,
        "the account is still the default after the round trip"
    );

    // 4. Re-login (the 401-that-cannot-self-heal path): same account, brand-new
    //    login state. It must update the existing row, not add one.
    let relogged = credential("brand-new-token", "工作号", "uid-lifecycle");
    workbuddy_auth::upsert_credential(relogged).unwrap();

    let final_state = workbuddy_auth::load_credentials();
    assert_eq!(
        final_state.len(),
        1,
        "re-logging into the same account must not create a second row"
    );
    assert_eq!(final_state[0].id, stable_id);
    assert_eq!(final_state[0].access_token, "brand-new-token");
    assert_eq!(
        workbuddy_auth::effective_default_identity(),
        stable_id,
        "the default still names the same account after a re-login"
    );
}
