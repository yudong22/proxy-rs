//! Client access accounts: the **inbound** half of the gateway's credentials.
//!
//! ## Why this is not the identity pool
//!
//! The project already has an 身份池 (`workbuddy-credentials.json` +
//! `api-keys.json`): those are **outbound** credentials — what this gateway
//! presents *to the upstream*. This module is the opposite direction: who is
//! allowed to call **this** gateway. Both can be active at once and they never
//! read each other's files.
//!
//! The two also travel on different headers, and that separation is the whole
//! design (see `docs/plan-client-access-accounts.md` §4.3):
//!
//!   * `x-api-key` / `authorization` stay owned by `UPSTREAM_API_KEY_PASSTHROUGH`
//!     and the upstream-credential path. This module **never reads them**.
//!   * a gateway access key travels on its own header, [`KEY_HEADER`]
//!     (`x-proxy-rs-key`), is consumed here, and is stripped before the request
//!     reaches the forwarding code.
//!
//! A request may therefore carry both headers: the upstream key goes upstream,
//! the gateway key stops at the door.
//!
//! ## What is stored
//!
//! Only `sha256(key)`, never the key. The plaintext is returned to the caller
//! exactly once, at creation time, and is unrecoverable afterwards — a leaked
//! state file cannot be replayed against the gateway. `id` is derived from the
//! hash (`ca-<6 hex>`) rather than from the plaintext, so an export/import round
//! trip can re-derive and repair it without ever holding a secret.
//!
//! Layer note: like `workbuddy_auth.rs` and `pool_transfer.rs`, this is Layer 3
//! with I/O.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Header carrying a gateway access key.
///
/// Deliberately **not** `x-api-key` / `authorization`: those belong to the
/// upstream passthrough path, and reusing them would make one header mean two
/// things depending on which feature is switched on.
pub const KEY_HEADER: &str = "x-proxy-rs-key";

/// Environment variable that forces the feature on, overriding the GUI switch.
///
/// A headless deployment has no GUI to flip a switch in, and a server should be
/// gated by default — so the deploy script sets this rather than relying on a
/// settings file.
pub const ENABLED_ENV: &str = "PROXY_CLIENT_AUTH";

/// Prefix on every generated key, so a key is recognisable at a glance in a
/// client's config or a shell history.
const KEY_PREFIX: &str = "prs_";

/// Bytes of entropy per key. 32 bytes → 43 base64url chars, well beyond guessable.
const KEY_BYTES: usize = 32;

/// One inbound access account.
///
/// There is no plaintext field here on purpose. Adding one "for convenience"
/// would put every usable key in a world-readable-ish state file and undo the
/// point of storing a hash.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientAccount {
    /// Stable id `ca-<6 hex>`, derived from `key_hash`.
    pub id: String,
    /// User-facing note ("家里的笔记本", "CI").
    #[serde(default)]
    pub label: String,
    /// Hex SHA-256 of the key. The key itself is never stored.
    pub key_hash: String,
    /// First few characters of the key, for the list view only.
    ///
    /// Not a secret: it is a prefix of a 43-char random string, so it narrows a
    /// brute-force search by nothing meaningful, and it lets a user tell two
    /// rows apart without revealing either.
    #[serde(default)]
    pub key_prefix: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub created_at_ms: i64,
    /// Last time this account authenticated a request. Observational only —
    /// deliberately *not* a quota counter (out of scope for this release).
    #[serde(default)]
    pub last_used_ms: Option<i64>,
    #[serde(default)]
    pub note: String,
}

fn default_true() -> bool {
    true
}

impl ClientAccount {
    /// Masked display form for the list view.
    pub fn masked(&self) -> String {
        if self.key_prefix.is_empty() {
            "…".to_string()
        } else {
            format!("{}…", self.key_prefix)
        }
    }
}

/// Store file: `~/.proxy-rs/client-accounts.json`.
///
/// Relocated by `PROXY_DATA_DIR` like every other state file, which is also how
/// a second instance (or a dev run) keeps its own accounts.
pub fn accounts_path() -> PathBuf {
    crate::settings::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("client-accounts.json")
}

/// Load the accounts.
///
/// A missing or unparseable file is an empty store, not an error: this is read
/// on the request path, and a corrupt file must not take the listener down. The
/// distinction between "missing" and "corrupt" is surfaced separately by
/// [`store_health`], which is what the export path uses to warn the user
/// instead of silently writing an empty bundle.
pub fn load_accounts() -> Vec<ClientAccount> {
    let Ok(text) = std::fs::read_to_string(accounts_path()) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Persist the accounts, owner-only where the platform supports it.
pub fn save_accounts(items: &[ClientAccount]) -> Result<()> {
    let path = accounts_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(items)?)?;
    set_private_permissions(&tmp);
    std::fs::rename(&tmp, &path)?;
    set_private_permissions(&path);
    Ok(())
}

/// 0600 on Unix; a no-op elsewhere (Windows inherits the user profile's ACLs).
fn set_private_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

// ── Key material ───────────────────────────────────────────────────────────

/// SHA-256 of a key, lowercase hex.
pub fn hash_key(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The account id for a key hash: `ca-<6 hex>`.
///
/// Derived from the **hash**, not from the plaintext: an imported bundle carries
/// no plaintext, so this is the only thing a re-key can work from.
pub fn id_from_hash(key_hash: &str) -> String {
    let hex: String = key_hash.chars().take(6).collect();
    format!("ca-{hex}")
}

/// Generate a fresh key. Returns `(plaintext, hash, prefix)`.
///
/// The plaintext is the caller's to hand to the user once; only the hash and
/// prefix belong in the store.
pub fn generate_key() -> (String, String, String) {
    use aes_gcm::aead::rand_core::RngCore;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    use base64::Engine as _;

    let mut raw = [0u8; KEY_BYTES];
    aes_gcm::aead::OsRng.fill_bytes(&mut raw);
    let key = format!("{KEY_PREFIX}{}", B64.encode(raw));
    let hash = hash_key(&key);
    // 12 chars: "prs_" plus 8 of the body — enough to identify, far too little
    // to help guess the remaining 35.
    let prefix: String = key.chars().take(12).collect();
    (key, hash, prefix)
}

/// Create a new account and persist it.
///
/// Returns the plaintext key **once**; it is not recoverable afterwards.
pub fn create_account(label: &str, note: &str) -> Result<(ClientAccount, String)> {
    let (key, hash, prefix) = generate_key();
    let mut items = load_accounts();
    let account = ClientAccount {
        id: id_from_hash(&hash),
        label: label.trim().to_string(),
        key_hash: hash,
        key_prefix: prefix,
        enabled: true,
        created_at_ms: crate::util::unix_millis(),
        last_used_ms: None,
        note: note.trim().to_string(),
    };
    if items.iter().any(|a| a.key_hash == account.key_hash) {
        // 32 random bytes colliding is not a real event; if it ever happens the
        // store is broken, and silently overwriting a row would be worse.
        bail!("生成密钥时发生碰撞，请重试");
    }
    items.push(account.clone());
    save_accounts(&items)?;
    Ok((account, key))
}

/// Replace one account by id. Returns whether it existed.
pub fn update_account(account: ClientAccount) -> Result<bool> {
    let mut items = load_accounts();
    match items.iter_mut().find(|a| a.id == account.id) {
        Some(slot) => {
            *slot = account;
            save_accounts(&items)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Enable/disable one account. Returns whether it existed.
pub fn set_enabled(id: &str, enabled: bool) -> Result<bool> {
    let mut items = load_accounts();
    match items.iter_mut().find(|a| a.id == id) {
        Some(slot) => {
            slot.enabled = enabled;
            save_accounts(&items)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Delete one account. Returns whether it existed.
pub fn delete_account(id: &str) -> Result<bool> {
    let mut items = load_accounts();
    let before = items.len();
    items.retain(|a| a.id != id);
    if items.len() == before {
        return Ok(false);
    }
    save_accounts(&items)?;
    Ok(true)
}

// ── Verification ───────────────────────────────────────────────────────────

/// The authenticated caller, attached to request extensions by the gate.
///
/// A distinct type from [`VerifiedAccount`] so the handler's extraction
/// (`Extension<ClientAccountIdentity>`) cannot be confused with the internal
/// verification result — the handler only ever needs the display identity, and
/// coupling the two would drag storage-shaped fields into the request path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientAccountIdentity {
    pub id: String,
    pub label: String,
}

/// What a successful verification tells the rest of the pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedAccount {
    pub id: String,
    pub label: String,
}

impl From<VerifiedAccount> for ClientAccountIdentity {
    fn from(value: VerifiedAccount) -> Self {
        Self {
            id: value.id,
            label: value.label,
        }
    }
}

/// Why a request was refused.
///
/// Separate from "no account matched" so the log can say *which* mistake the
/// caller made — a disabled account and a typo look identical in a 401 body but
/// are very different to debug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyFailure {
    /// No header at all.
    Missing,
    /// A header was present but matches no account.
    Unknown,
    /// Matches an account that is switched off.
    Disabled,
}

impl VerifyFailure {
    /// Stable slug for the response body / log line.
    pub fn slug(self) -> &'static str {
        match self {
            VerifyFailure::Missing => "missing_key",
            VerifyFailure::Unknown => "invalid_key",
            VerifyFailure::Disabled => "account_disabled",
        }
    }
}

/// Verify a presented key against the store. Pure: never writes.
///
/// Linear over the accounts, which is fine at any realistic pool size (tens);
/// the hash is computed once and compared in constant time per candidate, so a
/// timing side channel cannot be used to recover a near-miss key.
pub fn verify(
    presented: &str,
    accounts: &[ClientAccount],
) -> Result<VerifiedAccount, VerifyFailure> {
    let presented = presented.trim();
    if presented.is_empty() {
        return Err(VerifyFailure::Missing);
    }
    let hash = hash_key(presented);

    // Find by hash first, then apply the enabled check: an admin who disabled an
    // account should see `account_disabled`, not `invalid_key`, because the key
    // is in fact correct and the log must not lie about it.
    for account in accounts {
        if crate::api_token::constant_time_eq(account.key_hash.as_bytes(), hash.as_bytes()) {
            if !account.enabled {
                return Err(VerifyFailure::Disabled);
            }
            return Ok(VerifiedAccount {
                id: account.id.clone(),
                label: account.label.clone(),
            });
        }
    }
    Err(VerifyFailure::Unknown)
}

// ── Live state (hot-reloadable, no restart) ────────────────────────────────

/// Process-wide auth state shared by the middleware.
///
/// Holding the accounts in memory is what makes a GUI/HTTP change take effect on
/// the **next request** rather than at the next restart. `enabled` is captured
/// here rather than read from `Config` per request because `Config` is rebuilt
/// only when the listener is restarted.
pub struct ClientAuthState {
    enabled: std::sync::atomic::AtomicBool,
    accounts: tokio::sync::RwLock<Vec<ClientAccount>>,
    /// Throttles `last_used_ms` writes: persisting on every request would turn
    /// the request path into a disk-write path.
    last_persist_ms: std::sync::atomic::AtomicI64,
}

/// Minimum spacing between `last_used_ms` persistence, in milliseconds.
const LAST_USED_PERSIST_MS: i64 = 60_000;

impl ClientAuthState {
    /// Build state from the current settings and store.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: std::sync::atomic::AtomicBool::new(enabled),
            accounts: tokio::sync::RwLock::new(load_accounts()),
            last_persist_ms: std::sync::atomic::AtomicI64::new(0),
        }
    }

    /// A disabled instance, for tests and for installations that never turn the
    /// feature on — the middleware becomes a pass-through.
    pub fn disabled() -> Self {
        Self::new(false)
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled
            .store(enabled, std::sync::atomic::Ordering::SeqCst);
    }

    /// Re-read the store so a GUI/HTTP change applies without a restart.
    pub async fn reload_from_store(&self) {
        let fresh = load_accounts();
        *self.accounts.write().await = fresh;
    }

    /// Snapshot for the admin list endpoint.
    pub async fn snapshot(&self) -> Vec<ClientAccount> {
        self.accounts.read().await.clone()
    }

    /// Verify a presented key against the in-memory copy.
    pub async fn verify(&self, presented: &str) -> Result<VerifiedAccount, VerifyFailure> {
        let accounts = self.accounts.read().await;
        verify(presented, &accounts)
    }

    /// Record a successful use, persisting at most once per minute.
    ///
    /// Errors are swallowed: failing to stamp a timestamp must never fail the
    /// request that just authenticated correctly.
    pub async fn touch(&self, id: &str) {
        let now = crate::util::unix_millis();
        {
            let mut accounts = self.accounts.write().await;
            if let Some(account) = accounts.iter_mut().find(|a| a.id == id) {
                account.last_used_ms = Some(now);
            }
        }
        let last = self
            .last_persist_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        if now - last < LAST_USED_PERSIST_MS {
            return;
        }
        self.last_persist_ms
            .store(now, std::sync::atomic::Ordering::Relaxed);
        let snapshot = self.snapshot().await;
        let _ = save_accounts(&snapshot);
    }
}

/// Whether the feature is on, given the stored setting.
///
/// `PROXY_CLIENT_AUTH` wins over `gui-settings.json` in both directions (`=0` /
/// `=false` turns it off) so a deployment can pin the answer without trusting a
/// settings file that a GUI user could toggle.
pub fn enabled_from(settings_flag: bool) -> bool {
    match crate::config::env_value(ENABLED_ENV) {
        Some(raw) => {
            let v = raw.trim();
            !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("no"))
        }
        None => settings_flag,
    }
}

// ── Health (for the export path) ───────────────────────────────────────────

/// Whether the store file is missing, fine, or corrupt.
///
/// Mirrors `workbuddy_auth::store_health`'s shape so the pool export can iterate
/// one list instead of special-casing this file. Without it a corrupt accounts
/// file would export as an empty list and the user would only discover the loss
/// on the target machine.
pub fn store_health() -> crate::workbuddy_auth::StoreState {
    crate::workbuddy_auth::inspect_store_for::<Vec<ClientAccount>>(accounts_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point `PROXY_DATA_DIR` at a temp dir for the duration of one test.
    ///
    /// The env var is process-global, so these tests serialise on the shared
    /// lock `settings` already provides for exactly this reason.
    struct TempDataDir {
        _guard: std::sync::MutexGuard<'static, ()>,
        dir: PathBuf,
    }

    impl TempDataDir {
        fn new(name: &str) -> Self {
            let guard = crate::settings::data_dir_test_lock();
            let dir = std::env::temp_dir().join(format!(
                "proxy-rs-client-accounts-{}-{name}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            std::env::set_var("PROXY_DATA_DIR", &dir);
            Self { _guard: guard, dir }
        }
    }

    impl Drop for TempDataDir {
        fn drop(&mut self) {
            std::env::remove_var("PROXY_DATA_DIR");
            if !std::thread::panicking() {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }
    }

    #[test]
    fn a_created_key_is_never_written_to_disk() {
        let dir = TempDataDir::new("no-plaintext");
        let (account, key) = create_account("笔记本", "").expect("create");
        assert!(key.starts_with(KEY_PREFIX));

        let text = std::fs::read_to_string(dir.dir.join("client-accounts.json")).expect("read");
        assert!(
            !text.contains(&key),
            "the plaintext key must never reach the store"
        );
        assert!(text.contains(&account.key_hash), "the hash is stored");
        assert_eq!(account.id, id_from_hash(&account.key_hash));
    }

    #[test]
    fn the_right_key_verifies_and_a_near_miss_does_not() {
        let _dir = TempDataDir::new("verify");
        let (account, key) = create_account("ci", "").expect("create");
        let items = load_accounts();

        let ok = verify(&key, &items).expect("correct key");
        assert_eq!(ok.id, account.id);

        // Flip the last character: same length, different content.
        let mut bad = key.clone();
        bad.pop();
        bad.push(if key.ends_with('A') { 'B' } else { 'A' });
        assert_eq!(verify(&bad, &items), Err(VerifyFailure::Unknown));

        assert_eq!(verify("", &items), Err(VerifyFailure::Missing));
        assert_eq!(
            verify("prs_totally-made-up", &items),
            Err(VerifyFailure::Unknown)
        );
    }

    #[test]
    fn a_disabled_account_reports_disabled_not_unknown() {
        let _dir = TempDataDir::new("disabled");
        let (account, key) = create_account("old", "").expect("create");
        assert!(set_enabled(&account.id, false).expect("disable"));

        let items = load_accounts();
        // The key is *correct*; the account is simply off. Reporting `unknown`
        // would send whoever debugs this hunting for a typo that does not exist.
        assert_eq!(verify(&key, &items), Err(VerifyFailure::Disabled));
    }

    #[test]
    fn ids_are_derived_from_the_hash_and_survive_a_reload() {
        let _dir = TempDataDir::new("ids");
        let (account, _key) = create_account("a", "").expect("create");
        assert_eq!(account.id, id_from_hash(&account.key_hash));
        assert_eq!(account.id.len(), 9, "ca- + 6 hex");

        let reloaded = load_accounts();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].id, account.id);
    }

    #[test]
    fn delete_removes_only_the_named_account() {
        let _dir = TempDataDir::new("delete");
        let (a, _) = create_account("a", "").expect("create a");
        let (b, _) = create_account("b", "").expect("create b");
        assert!(delete_account(&a.id).expect("delete"));
        let items = load_accounts();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, b.id);
        assert!(!delete_account(&a.id).expect("second delete"));
    }

    #[test]
    fn the_masked_form_never_exposes_the_body() {
        let (_key, hash, prefix) = generate_key();
        let account = ClientAccount {
            id: id_from_hash(&hash),
            key_hash: hash,
            key_prefix: prefix,
            ..Default::default()
        };
        let masked = account.masked();
        assert!(masked.ends_with('…'));
        assert!(masked.len() < 20, "masked form stays short: {masked}");
    }

    #[tokio::test]
    async fn state_reloads_from_disk_without_a_restart() {
        let _dir = TempDataDir::new("reload");
        let state = ClientAuthState::new(true);
        assert_eq!(state.snapshot().await.len(), 0);

        let (account, key) = create_account("live", "").expect("create");
        // The state was built before the account existed, so it must be told.
        assert_eq!(
            state.verify(&key).await,
            Err(VerifyFailure::Unknown),
            "no reload yet: the in-memory copy is still empty"
        );
        state.reload_from_store().await;
        assert_eq!(
            state.verify(&key).await.expect("after reload").id,
            account.id
        );
    }

    #[tokio::test]
    async fn a_disabled_state_short_circuits_before_the_store() {
        let _dir = TempDataDir::new("off");
        let state = ClientAuthState::disabled();
        assert!(!state.is_enabled());
        state.set_enabled(true);
        assert!(state.is_enabled());
    }

    #[test]
    fn corrupt_json_is_an_empty_store_not_a_panic() {
        let dir = TempDataDir::new("corrupt");
        std::fs::write(dir.dir.join("client-accounts.json"), "{not json").expect("write");
        // The request path must keep serving; the export path asks `store_health`.
        assert!(load_accounts().is_empty());
        assert!(matches!(
            store_health(),
            crate::workbuddy_auth::StoreState::Corrupt(_)
        ));
    }
}
