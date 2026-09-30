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
    self, api_key_id, credential_id, ApiKeyEntry, PoolPreferences, WorkBuddyAccount,
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
    WorkBuddyCredential {
        id: credential_id(token),
        label: label.to_string(),
        access_token: token.to_string(),
        refresh_token: Some(format!("{token}-refresh")),
        expires_at_ms: Some(4_000_000_000_000),
        domain: "copilot.tencent.com".to_string(),
        account: WorkBuddyAccount {
            uid: uid.to_string(),
            enterprise_id: "ent-1".to_string(),
            nickname: format!("{label}-nick"),
        },
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
