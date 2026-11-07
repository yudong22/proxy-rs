//! Account-pool transfer: a self-contained bundle that carries the identity
//! pool to another machine.
//!
//! What travels
//! ------------
//! Exactly the three files that make up the 身份池 — `workbuddy-credentials.json`
//! (login states), `api-keys.json` (upstream keys) and the selection/schedule
//! half of `workbuddy-pool.json` — and nothing else. `gui-settings.json` and
//! `.env` are deliberately excluded: the port, bind address, upstream URL and
//! model mapping are *machine* facts, and importing them into a second machine
//! is as likely to break it (port already taken, upstream pointed at a host that
//! does not resolve there) as to help. `stats.db` is excluded too: it is
//! history, not identity, and can be large.
//!
//! The three stores are not rewritten in a new shape — the bundle embeds
//! `WorkBuddyCredential` / `ApiKeyEntry` verbatim, so a field added later with
//! `#[serde(default)]` is automatically carried by an older build's bundle and
//! tolerated by a newer one.
//!
//! Plain or encrypted
//! ------------------
//! The default is a plain JSON document, written 0600. A bundle is a password
//! book — the login states in it are immediately usable — so encryption is
//! offered, but it is opt-in: a passphrase the user forgets turns the file into
//! rubbish, which is a worse failure than a file only they can read. When asked
//! for, the whole payload (tokens, keys, preferences) goes into one AES-256-GCM
//! ciphertext under an Argon2id-derived key, and the envelope keeps only counts
//! and KDF parameters — never a token.
//!
//! Import is non-destructive by construction
//! ----------------------------------------
//! [`preview_import`] is read-only and can be called repeatedly; [`apply_import`]
//! re-decodes the text rather than trusting a previous preview, backs up every
//! file it is about to replace, and writes through a temp file + rename so a
//! failure cannot leave half a pool behind.
//!
//! Layer note: like `credits.rs` and `workbuddy_auth.rs`, this is Layer 3 with
//! I/O — not part of the pure translation core.
//!
//! ## File layout
//!
//! Split from one 2,265-line file (1,255 production + 1,010 tests). The split
//! moves items only; not a line of logic changed, and every historical
//! `crate::pool_transfer::X` path resolves through the re-exports below, so
//! `tests/` and `src/admin/ops_pool.rs` needed no edits.
//!
//! | file | owns |
//! |---|---|
//! | [`bundle`] | the format constants and the on-disk envelope: `BundleFile`, `BundlePayload`, `KdfParams`, `CipherBlob` |
//! | [`crypto`] | Argon2id key derivation and the AES-256-GCM seal/open |
//! | [`export`] | building the payload from the local stores and writing the file (incl. the atomic write and 0600 permissions) |
//! | [`import`] | decoding, reconciling ids, planning against local state, committing, and the public preview/apply entry points |
//!
//! `use super::*;` in each submodule is deliberate: the four pieces share the
//! imports and call each other freely (export encrypts through `crypto`, import
//! verifies through export's checksum), so re-declaring imports per file would
//! be churn with no isolation benefit. The test module stays whole at the bottom
//! of this file, exactly as it was.

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use anyhow::{anyhow, bail, Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::workbuddy_auth::{
    self, api_key_id, md5_hex, ApiKeyEntry, PoolPreferences, WorkBuddyCredential,
};

mod bundle;
mod crypto;
mod export;
mod import;

pub use bundle::*;
pub(crate) use crypto::*;
pub use export::*;
pub use import::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workbuddy_auth::credential_id;
    use std::ffi::OsString;

    /// Every test here relocates `PROXY_DATA_DIR`, which is process-global. The
    /// lock is shared with the `settings` tests for exactly that reason: three
    /// test modules writing the same env var to different roots would otherwise
    /// interleave and fail each other.
    struct TempDataDir {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: (Option<OsString>, Option<OsString>),
        dir: PathBuf,
    }

    impl TempDataDir {
        fn new(name: &str) -> Self {
            let guard = crate::settings::data_dir_test_lock();
            let previous = (
                std::env::var_os(crate::settings::DATA_DIR_ENV),
                std::env::var_os(crate::settings::LEGACY_DATA_DIR_ENV),
            );
            let dir = std::env::temp_dir().join(format!(
                "proxy-rs-pool-transfer-{}-{}",
                std::process::id(),
                name
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::env::remove_var(crate::settings::LEGACY_DATA_DIR_ENV);
            std::env::set_var(crate::settings::DATA_DIR_ENV, &dir);
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
                Some(v) => std::env::set_var(crate::settings::DATA_DIR_ENV, v),
                None => std::env::remove_var(crate::settings::DATA_DIR_ENV),
            }
            match &self.previous.1 {
                Some(v) => std::env::set_var(crate::settings::LEGACY_DATA_DIR_ENV, v),
                None => std::env::remove_var(crate::settings::LEGACY_DATA_DIR_ENV),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn cred(id_token: &str, label: &str) -> WorkBuddyCredential {
        WorkBuddyCredential {
            id: credential_id(id_token),
            label: label.to_string(),
            access_token: id_token.to_string(),
            refresh_token: Some(format!("{id_token}-refresh")),
            expires_at_ms: Some(4_000_000_000_000),
            domain: "copilot.tencent.com".to_string(),
            machine_id: "machine-abc".to_string(),
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

    /// Seed the (relocated) stores the way the GUI would.
    fn seed(creds: &[WorkBuddyCredential], keys: &[ApiKeyEntry]) {
        workbuddy_auth::save_credentials(creds).unwrap();
        workbuddy_auth::save_api_keys(keys).unwrap();
    }

    #[test]
    fn plain_round_trip_restores_every_pool_member() {
        let _dir = TempDataDir::new("roundtrip");
        let creds = vec![cred("token-a", "工作号"), cred("token-b", "备用号")];
        let keys = vec![key("sk-aaa", "主密钥")];
        seed(&creds, &keys);
        workbuddy_auth::save_preferences(&PoolPreferences {
            default_identity_id: creds[0].id.clone(),
            daily_checkin_enabled: true,
            daily_checkin_time: "08:30".to_string(),
            ..Default::default()
        })
        .unwrap();

        let (text, outcome) = export_text(&ExportOptions::default()).unwrap();
        assert!(!outcome.encrypted);
        assert_eq!(outcome.counts.credentials, 2);
        assert_eq!(outcome.counts.api_keys, 1);

        // Wipe the machine, as if this were the new one.
        seed(&[], &[]);
        workbuddy_auth::save_preferences(&PoolPreferences::default()).unwrap();

        let preview = apply_import(&text, None, ImportMode::Merge).unwrap();
        assert_eq!(preview.credentials_added, 2);
        assert_eq!(preview.keys_added, 1);
        assert_eq!(
            preview.default_identity_applied,
            cred("token-a", "").id,
            "the exported default must come back"
        );

        let restored = workbuddy_auth::load_credentials();
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].label, "工作号");
        assert_eq!(restored[0].machine_id, "machine-abc");
        assert_eq!(workbuddy_auth::load_api_keys().len(), 1);
        assert_eq!(
            workbuddy_auth::effective_default_identity(),
            cred("token-a", "").id,
            "the account the bundle pinned is the identity in force"
        );
        let prefs = workbuddy_auth::load_preferences();
        assert!(prefs.daily_checkin_enabled);
        assert_eq!(prefs.daily_checkin_time, "08:30");
    }

    #[test]
    fn export_normalizes_machine_local_state() {
        let _dir = TempDataDir::new("normalize");
        let mut c = cred("token-a", "工作号");
        c.cooldown_until_ms = Some(crate::util::unix_millis() + 600_000);
        c.last_error = "429 too many requests".to_string();
        c.last_checkin_date = Some("2026-01-01".to_string());
        seed(&[c], &[]);

        let (text, _) = export_text(&ExportOptions::default()).unwrap();
        let bundle: BundleFile = serde_json::from_str(&text).unwrap();
        let exported = &bundle.credentials[0];
        assert_eq!(
            exported.cooldown_until_ms, None,
            "cooldown is machine-local"
        );
        assert!(
            exported.last_error.is_empty(),
            "stale error text must not travel"
        );
        assert_eq!(
            exported.last_checkin_date.as_deref(),
            Some("2026-01-01"),
            "the check-in day is the account's, not the machine's"
        );
        assert_eq!(exported.machine_id, "machine-abc");
    }

    #[test]
    fn encrypted_round_trip_matches_the_plain_one() {
        let _dir = TempDataDir::new("encrypted");
        let creds = vec![cred("token-a", "工作号")];
        seed(&creds, &[key("sk-aaa", "主密钥")]);
        let pass = "correct horse battery staple";

        let (plain, _) = export_text(&ExportOptions::default()).unwrap();
        let (sealed, outcome) = export_text(&ExportOptions {
            passphrase: Some(pass.to_string()),
        })
        .unwrap();
        assert!(outcome.encrypted);

        apply_import(&plain, None, ImportMode::Replace).unwrap();
        let from_plain = (
            workbuddy_auth::load_credentials(),
            workbuddy_auth::load_api_keys(),
        );

        seed(&[], &[]);
        apply_import(&sealed, Some(pass), ImportMode::Replace).unwrap();
        let from_sealed = (
            workbuddy_auth::load_credentials(),
            workbuddy_auth::load_api_keys(),
        );

        assert_eq!(from_plain.0, from_sealed.0);
        assert_eq!(from_plain.1, from_sealed.1);
    }

    #[test]
    fn an_empty_pool_exports_a_bundle_but_says_so() {
        let _dir = TempDataDir::new("empty-pool");
        seed(&[], &[]);

        let (text, outcome) = export_text(&ExportOptions::default()).unwrap();

        // The file is still written: it is a valid bundle, and it may legitimately
        // carry preferences alone. What must not happen is reporting success.
        assert_eq!(outcome.counts.credentials, 0);
        assert_eq!(outcome.counts.api_keys, 0);
        assert!(outcome.is_empty());
        assert!(
            outcome.warnings.iter().any(|w| w.contains("身份池为空")),
            "an empty export must explain itself: {:?}",
            outcome.warnings
        );
        // And it must still round-trip, so the user who exports an empty pool on
        // purpose (to move only the schedule) is not broken by the warning.
        let preview = preview_import(&text, None, ImportMode::Merge).unwrap();
        assert_eq!(preview.credentials_added, 0);
    }

    /// The failure that produced a 356-byte "backup" containing nothing: a
    /// corrupt store reads as empty through `load_*`, so the export cannot tell
    /// it apart from a genuinely empty pool unless it checks the files first.
    #[test]
    fn a_corrupt_store_is_reported_instead_of_exporting_silence() {
        let _dir = TempDataDir::new("corrupt-store");
        seed(&[cred("token-a", "工作号")], &[key("sk-aaa", "主密钥")]);

        // Truncate the credential file the way an interrupted write would.
        let path = workbuddy_auth::credentials_path();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.truncate(text.len() / 2);
        std::fs::write(&path, &text).unwrap();

        let (_, outcome) = export_text(&ExportOptions::default()).unwrap();

        assert_eq!(
            outcome.counts.credentials, 0,
            "a corrupt store does read as empty — that is the underlying design"
        );
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.contains("账号池文件已损坏")),
            "the corruption must be named, not silently exported as an empty pool: {:?}",
            outcome.warnings
        );
        // The healthy key pool is still exported: one broken file must not wipe
        // the rest of the bundle.
        assert_eq!(outcome.counts.api_keys, 1);
    }

    #[test]
    fn store_health_separates_missing_from_corrupt() {
        let _dir = TempDataDir::new("store-health");

        // Nothing on disk yet: every store is legitimately absent.
        for (_, state) in workbuddy_auth::store_health() {
            assert_eq!(state, workbuddy_auth::StoreState::Missing);
            assert!(!state.is_corrupt());
        }

        seed(&[cred("token-a", "工作号")], &[key("sk-aaa", "主密钥")]);
        workbuddy_auth::save_preferences(&PoolPreferences {
            daily_checkin_time: "09:00".to_string(),
            ..Default::default()
        })
        .unwrap();
        for (_, state) in workbuddy_auth::store_health() {
            assert_eq!(state, workbuddy_auth::StoreState::Ok);
        }

        std::fs::write(workbuddy_auth::api_keys_path(), "{ not json").unwrap();
        let health = workbuddy_auth::store_health();
        let keys = health.iter().find(|(n, _)| *n == "密钥池").unwrap();
        assert!(keys.1.is_corrupt(), "malformed JSON must be Corrupt");
    }

    #[test]
    fn encrypted_envelope_leaks_no_token_material() {
        let _dir = TempDataDir::new("no-leak");
        seed(
            &[cred("super-secret-access-token", "工作号")],
            &[key("sk-secret-material", "k")],
        );
        let (sealed, _) = export_text(&ExportOptions {
            passphrase: Some("pw".to_string()),
        })
        .unwrap();

        // Neither the full secret nor a distinguishing slice of it may appear in
        // the envelope — base64 of the ciphertext is the only thing that does.
        for needle in [
            "super-secret-access-token",
            "secret-access-token",
            "sk-secret-material",
            "copilot.tencent.com",
        ] {
            assert!(
                !sealed.contains(needle),
                "encrypted bundle must not contain {needle:?}"
            );
        }
        assert!(sealed.contains("argon2id"));
        assert!(sealed.contains("aes-256-gcm"));
    }

    #[test]
    fn wrong_passphrase_fails_and_writes_nothing() {
        let _dir = TempDataDir::new("wrong-pass");
        seed(&[cred("token-a", "工作号")], &[]);
        let (sealed, _) = export_text(&ExportOptions {
            passphrase: Some("right".to_string()),
        })
        .unwrap();
        seed(&[], &[]);

        let err = apply_import(&sealed, Some("wrong"), ImportMode::Merge).unwrap_err();
        assert!(
            err.to_string().contains("口令错误或文件已损坏"),
            "unexpected error: {err}"
        );
        assert!(
            workbuddy_auth::load_credentials().is_empty(),
            "a failed import must leave the pool untouched"
        );
    }

    #[test]
    fn encrypted_bundle_without_a_passphrase_says_so() {
        let _dir = TempDataDir::new("needs-pass");
        seed(&[cred("token-a", "工作号")], &[]);
        let (sealed, _) = export_text(&ExportOptions {
            passphrase: Some("pw".to_string()),
        })
        .unwrap();
        let err = preview_import(&sealed, None, ImportMode::Merge).unwrap_err();
        assert!(err.to_string().contains("请输入口令"), "unexpected: {err}");
    }

    /// Regression: importing your own export used to silently clear the default
    /// identity.
    ///
    /// `reconcile_ids` forced `id == hash(access_token)` and rewrote any entry
    /// that disagreed. An account that had been *refreshed* genuinely disagrees
    /// (the token rotated, the account did not), so its id was rewritten and the
    /// `default_identity_id` pointing at the old value stopped resolving — the
    /// account survived but quietly stopped being the default.
    #[test]
    fn importing_an_export_keeps_a_refreshed_account_as_the_default() {
        let _dir = TempDataDir::new("refreshed-default");

        // An account whose token has already been rotated once: the stored id
        // predates the current access token, exactly like a live install.
        let account = crate::workbuddy_auth::WorkBuddyAccount {
            uid: "uid-keep".to_string(),
            enterprise_id: "ent-1".to_string(),
            nickname: "甲".to_string(),
        };
        let stable_id = crate::workbuddy_auth::credential_id_for(&account, "original-token");
        let cred = WorkBuddyCredential {
            id: stable_id.clone(),
            label: "工作号".to_string(),
            access_token: "rotated-by-refresh".to_string(),
            refresh_token: Some("ref-1".to_string()),
            account: account.clone(),
            machine_id: "m1".to_string(),
            enabled: true,
            ..Default::default()
        };
        workbuddy_auth::save_credentials(std::slice::from_ref(&cred)).unwrap();
        workbuddy_auth::save_preferences(&PoolPreferences {
            default_identity_id: stable_id.clone(),
            ..Default::default()
        })
        .unwrap();

        let (text, _) = export_text(&ExportOptions::default()).unwrap();
        let preview = apply_import(&text, None, ImportMode::Replace).unwrap();

        // The id must survive the round trip...
        let after = workbuddy_auth::load_credentials();
        assert_eq!(after.len(), 1);
        assert_eq!(
            after[0].id, stable_id,
            "the stable account id must not be rewritten on import"
        );
        // ...and the default must still name it.
        assert_eq!(
            workbuddy_auth::load_preferences().default_identity_id,
            stable_id,
            "the default identity must survive importing your own export"
        );
        assert!(
            !preview.default_identity_dropped,
            "importing a valid export must never drop the default: {:?}",
            preview.warnings
        );
        assert_eq!(
            workbuddy_auth::effective_default_identity(),
            stable_id,
            "the account is still the identity in force"
        );
    }

    /// A local entry keyed by the *legacy* token hash must be recognized as the
    /// same account as a bundle entry keyed by uid, or a merge would import it
    /// twice.
    #[test]
    fn merge_matches_a_legacy_token_hash_id_to_the_same_account() {
        let _dir = TempDataDir::new("legacy-id-merge");

        let account = crate::workbuddy_auth::WorkBuddyAccount {
            uid: "uid-legacy".to_string(),
            enterprise_id: "ent-1".to_string(),
            nickname: "甲".to_string(),
        };
        // What an install that predates the stable-identity change has on disk:
        // the id is a hash of the token it had at import time.
        let legacy = WorkBuddyCredential {
            id: credential_id("token-at-import-time"),
            label: "旧记录".to_string(),
            access_token: "rotated-token".to_string(),
            refresh_token: Some("ref-local".to_string()),
            account: account.clone(),
            machine_id: "m1".to_string(),
            enabled: true,
            ..Default::default()
        };
        workbuddy_auth::save_credentials(std::slice::from_ref(&legacy)).unwrap();

        // The bundle carries the same account, keyed by uid.
        let stable_id = crate::workbuddy_auth::credential_id_for(&account, "rotated-token");
        let incoming = WorkBuddyCredential {
            id: stable_id.clone(),
            label: "新标签".to_string(),
            access_token: "rotated-token".to_string(),
            refresh_token: Some("ref-incoming".to_string()),
            account,
            machine_id: "m1".to_string(),
            enabled: true,
            ..Default::default()
        };
        let bundle = BundlePayload {
            preferences: BundlePreferences::default(),
            credentials: vec![incoming],
            api_keys: Vec::new(),
            client_accounts: Vec::new(),
        };

        let preview = apply_import(&plain_bundle(&bundle), None, ImportMode::Merge).unwrap();

        assert_eq!(
            preview.credentials_added, 0,
            "the same account must not be imported as a second row"
        );
        assert_eq!(preview.credentials_updated, 1);
        let after = workbuddy_auth::load_credentials();
        assert_eq!(after.len(), 1);
        // The stored (legacy) id stays, so anything already pointing at it keeps
        // working; the label comes from the bundle.
        assert_eq!(after[0].id, legacy.id);
        assert_eq!(after[0].label, "新标签");
    }

    #[test]
    fn merge_updates_only_what_changed_and_keeps_local_refresh_tokens() {
        let _dir = TempDataDir::new("merge");
        let mut local = cred("token-a", "旧的备注");
        local.refresh_token = Some("local-fresher-refresh".to_string());
        seed(&[local.clone(), cred("token-only-local", "只在本机")], &[]);

        // A bundle that renames the account but carries no refresh token.
        let mut traveling = cred("token-a", "新的备注");
        traveling.refresh_token = None;
        traveling.expires_at_ms = None;
        let bundle = BundlePayload {
            preferences: BundlePreferences::default(),
            credentials: vec![traveling],
            api_keys: Vec::new(),
            client_accounts: Vec::new(),
        };
        let text = plain_bundle(&bundle);

        let preview = apply_import(&text, None, ImportMode::Merge).unwrap();
        assert_eq!(preview.credentials_updated, 1);
        assert_eq!(preview.credentials_added, 0);
        assert_eq!(preview.credentials_removed, 0, "merge never removes");

        let after = workbuddy_auth::load_credentials();
        assert_eq!(after.len(), 2, "the local-only account survives a merge");
        let touched = after.iter().find(|c| c.id == local.id).unwrap();
        assert_eq!(touched.label, "新的备注");
        assert_eq!(
            touched.refresh_token.as_deref(),
            Some("local-fresher-refresh"),
            "a bundle without refresh material must not erase the local one"
        );
    }

    #[test]
    fn replacing_drops_local_entries_and_backs_the_files_up() {
        let _dir = TempDataDir::new("replace");
        seed(&[cred("token-old", "旧账号")], &[key("sk-old", "旧密钥")]);
        let credentials_path = workbuddy_auth::credentials_path();
        let keys_path = workbuddy_auth::api_keys_path();
        assert!(credentials_path.exists());

        let bundle = BundlePayload {
            preferences: BundlePreferences::default(),
            credentials: vec![cred("token-new", "新账号")],
            api_keys: vec![key("sk-new", "新密钥")],
            client_accounts: Vec::new(),
        };
        let preview = apply_import(&plain_bundle(&bundle), None, ImportMode::Replace).unwrap();

        assert_eq!(preview.credentials_removed, 1);
        assert_eq!(preview.keys_removed, 1);
        assert_eq!(workbuddy_auth::load_credentials().len(), 1);
        assert_eq!(workbuddy_auth::load_credentials()[0].label, "新账号");
        assert_eq!(workbuddy_auth::load_api_keys()[0].label, "新密钥");

        // Every replaced file must be recoverable by hand.
        assert!(
            backup_of(&credentials_path).is_some(),
            "credentials file was not backed up"
        );
        assert!(
            backup_of(&keys_path).is_some(),
            "key file was not backed up"
        );
    }

    #[test]
    fn dangling_default_identity_is_cleared_with_a_warning() {
        let _dir = TempDataDir::new("dangling");
        seed(&[cred("token-a", "工作号")], &[]);
        workbuddy_auth::save_preferences(&PoolPreferences {
            default_identity_id: credential_id("token-a"),
            ..Default::default()
        })
        .unwrap();

        let bundle = BundlePayload {
            preferences: BundlePreferences {
                default_identity_id: Some("wb-ffffff".to_string()),
                ..Default::default()
            },
            credentials: vec![cred("token-a", "工作号")],
            api_keys: Vec::new(),
            client_accounts: Vec::new(),
        };
        let preview = apply_import(&plain_bundle(&bundle), None, ImportMode::Merge).unwrap();

        assert!(preview.default_identity_dropped);
        assert!(preview.default_identity_applied.is_empty());
        assert!(
            preview.warnings.iter().any(|w| w.contains("wb-ffffff")),
            "the warning must name the id that was dropped: {:?}",
            preview.warnings
        );
        assert!(workbuddy_auth::load_preferences()
            .default_identity_id
            .is_empty());
    }

    #[test]
    fn an_id_that_disagrees_with_its_token_is_recomputed() {
        let _dir = TempDataDir::new("recompute-id");
        let mut tampered = cred("token-a", "工作号");
        tampered.id = "wb-000000".to_string();
        let bundle = BundlePayload {
            preferences: BundlePreferences::default(),
            credentials: vec![tampered],
            api_keys: Vec::new(),
            client_accounts: Vec::new(),
        };

        let preview = preview_import(&plain_bundle(&bundle), None, ImportMode::Merge).unwrap();
        assert!(preview.warnings.iter().any(|w| w.contains("重新计算")));
        apply_import(&plain_bundle(&bundle), None, ImportMode::Merge).unwrap();
        assert_eq!(
            workbuddy_auth::load_credentials()[0].id,
            credential_id("token-a")
        );
    }

    #[test]
    fn a_checksum_mismatch_warns_but_still_imports() {
        let _dir = TempDataDir::new("checksum");
        let bundle = BundlePayload {
            preferences: BundlePreferences::default(),
            credentials: vec![cred("token-a", "工作号")],
            api_keys: Vec::new(),
            client_accounts: Vec::new(),
        };
        let mut file: BundleFile = serde_json::from_str(&plain_bundle(&bundle)).unwrap();
        file.checksum = Some("deadbeef".to_string());
        let text = serde_json::to_string(&file).unwrap();

        let preview = apply_import(&text, None, ImportMode::Merge).unwrap();
        assert_eq!(workbuddy_auth::load_credentials().len(), 1);
        assert!(preview.credentials_added == 1);
        assert!(
            preview.warnings.iter().any(|w| w.contains("校验和")),
            "a hand-edited file must be reported, not imported silently: {:?}",
            preview.warnings
        );
    }

    #[test]
    fn malformed_input_is_rejected_with_a_readable_message() {
        let _dir = TempDataDir::new("malformed");
        for (text, needle) in [
            ("not json at all", "不是有效的 JSON"),
            (
                r#"{"format":"something-else","version":1,"kind":"plain"}"#,
                "不是 Proxy RS",
            ),
            (
                r#"{"format":"proxy-rs-pool","version":1,"kind":"martian"}"#,
                "未知的导出文件类型",
            ),
        ] {
            let err = preview_import(text, None, ImportMode::Merge).unwrap_err();
            assert!(
                err.to_string().contains(needle),
                "expected {needle:?} in {err:?}"
            );
        }
    }

    #[test]
    fn a_newer_format_version_warns_instead_of_failing() {
        let _dir = TempDataDir::new("newer-version");
        let bundle = BundlePayload {
            preferences: BundlePreferences::default(),
            credentials: vec![cred("token-a", "工作号")],
            api_keys: Vec::new(),
            client_accounts: Vec::new(),
        };
        let mut file: BundleFile = serde_json::from_str(&plain_bundle(&bundle)).unwrap();
        file.version = BUNDLE_VERSION + 7;
        let preview = apply_import(
            &serde_json::to_string(&file).unwrap(),
            None,
            ImportMode::Merge,
        )
        .unwrap();
        assert!(preview.warnings.iter().any(|w| w.contains("更新版本")));
        assert_eq!(workbuddy_auth::load_credentials().len(), 1);
    }

    #[test]
    fn oversized_input_is_refused_before_parsing() {
        let _dir = TempDataDir::new("oversized");
        let huge = "x".repeat(MAX_BUNDLE_BYTES + 1);
        let err = preview_import(&huge, None, ImportMode::Merge).unwrap_err();
        assert!(err.to_string().contains("文件过大"), "unexpected: {err}");
    }

    #[test]
    fn sanitize_filename_strips_paths_and_forces_a_json_suffix() {
        assert_eq!(sanitize_filename("备份"), "备份.json");
        assert_eq!(sanitize_filename("pool 2026"), "pool-2026.json");
        assert_eq!(
            sanitize_filename("../../.ssh/authorized_keys"),
            "authorized_keys.json"
        );
        assert_eq!(sanitize_filename("a/b/pool.json"), "pool.json");
        assert_eq!(sanitize_filename(""), "proxy-rs-pool.json");
        assert_eq!(sanitize_filename("..."), "proxy-rs-pool.json");
    }

    #[test]
    fn secret_files_are_written_owner_only() {
        let dir = TempDataDir::new("perms");
        seed(&[cred("token-a", "工作号")], &[key("sk-aaa", "主密钥")]);

        let (text, _) = export_text(&ExportOptions::default()).unwrap();
        let bundle = dir.dir.join("pool.json");
        write_bundle(&bundle, &text).unwrap();

        // Re-import so the credential/key stores are written by this module too.
        seed(&[], &[]);
        apply_import(&text, None, ImportMode::Merge).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode_of = |p: &Path| {
                std::fs::metadata(p)
                    .unwrap_or_else(|e| panic!("stat {}: {e}", p.display()))
                    .permissions()
                    .mode()
                    & 0o777
            };
            // The bundle and both token-bearing stores are 0600. The preferences
            // file holds no secret, so it is left at the process default.
            for path in [
                bundle.clone(),
                workbuddy_auth::credentials_path(),
                workbuddy_auth::api_keys_path(),
            ] {
                assert_eq!(mode_of(&path), 0o600, "{} is not 0600", path.display());
            }
        }
        #[cfg(not(unix))]
        let _ = &bundle;
    }

    #[test]
    fn the_export_stamp_follows_the_local_day_in_both_hemispheres() {
        // 2026-01-02T23:30:00Z. In UTC+8 that is already the 3rd; in UTC-5 it is
        // still the 2nd. A naive implementation that clamped the negative offset
        // to zero would stamp the western case as the 3rd.
        let now_ms = 1_767_396_600_000;
        assert_eq!(export_stamp(now_ms, 8 * 3600), "20260103");
        assert_eq!(export_stamp(now_ms, -5 * 3600), "20260102");
        assert_eq!(export_stamp(now_ms, 0), "20260102");
    }

    /// Build a plain bundle document for a payload, the way `export_text` does,
    /// so tests can seed arbitrary payloads (including tampered ones).
    fn plain_bundle(payload: &BundlePayload) -> String {
        let file = BundleFile {
            format: BUNDLE_FORMAT.to_string(),
            version: BUNDLE_VERSION,
            kind: KIND_PLAIN.to_string(),
            exported_at_ms: crate::util::unix_millis(),
            app_version: "test".to_string(),
            counts: BundleCounts {
                credentials: payload.credentials.len(),
                api_keys: payload.api_keys.len(),
                client_accounts: payload.client_accounts.len(),
            },
            checksum: Some(payload_checksum(payload).unwrap()),
            preferences: Some(payload.preferences.clone()),
            credentials: payload.credentials.clone(),
            api_keys: payload.api_keys.clone(),
            client_accounts: payload.client_accounts.clone(),
            kdf: None,
            cipher: None,
        };
        serde_json::to_string_pretty(&file).unwrap()
    }

    // ── Client access accounts in the bundle (v2) ──────────────────────────

    /// Seed the access-account store the way the GUI would.
    fn seed_accounts(labels: &[&str]) -> Vec<String> {
        let mut keys = Vec::new();
        for label in labels {
            let (_account, key) = crate::client_accounts::create_account(label, "").unwrap();
            keys.push(key);
        }
        keys
    }

    #[test]
    fn accounts_travel_with_the_pool_and_still_authenticate_after_import() {
        let _dir = TempDataDir::new("accounts-roundtrip");
        seed(&[cred("token-a", "工作号")], &[key("sk-aaa", "主密钥")]);
        let plaintext_keys = seed_accounts(&["笔记本", "CI"]);

        let (text, outcome) = export_text(&ExportOptions::default()).unwrap();
        assert_eq!(outcome.counts.client_accounts, 2);
        assert!(!outcome.is_empty());

        // Wipe every store, then import into the empty machine.
        workbuddy_auth::save_credentials(&[]).unwrap();
        workbuddy_auth::save_api_keys(&[]).unwrap();
        crate::client_accounts::save_accounts(&[]).unwrap();
        assert!(crate::client_accounts::load_accounts().is_empty());

        let preview = apply_import(&text, None, ImportMode::Replace).unwrap();
        assert_eq!(preview.client_accounts_added, 2);
        assert_eq!(crate::client_accounts::load_accounts().len(), 2);

        // The decisive property: the key the *client* already holds keeps
        // working on the target machine, because verification is a hash compare
        // and only the hash travelled.
        let accounts = crate::client_accounts::load_accounts();
        for key in &plaintext_keys {
            let verified = crate::client_accounts::verify(key, &accounts)
                .expect("a migrated key must still authenticate");
            assert!(verified.id.starts_with("ca-"));
        }
    }

    #[test]
    fn a_plain_bundle_never_contains_a_usable_key() {
        let _dir = TempDataDir::new("accounts-plaintext");
        let plaintext_keys = seed_accounts(&["笔记本"]);

        let (text, _) = export_text(&ExportOptions::default()).unwrap();
        for key in &plaintext_keys {
            assert!(
                !text.contains(key.as_str()),
                "a plain bundle must not carry the usable key"
            );
        }
        // The hash is what travels instead.
        let accounts = crate::client_accounts::load_accounts();
        assert!(text.contains(&accounts[0].key_hash));
    }

    #[test]
    fn an_encrypted_bundle_hides_the_accounts_in_the_envelope() {
        let _dir = TempDataDir::new("accounts-encrypted");
        seed_accounts(&["笔记本"]);

        let opts = ExportOptions {
            passphrase: Some("correct horse".to_string()),
        };
        let (text, outcome) = export_text(&opts).unwrap();
        assert!(outcome.encrypted);
        // Counts stay readable so the file can be previewed before unlocking.
        assert_eq!(outcome.counts.client_accounts, 1);

        let accounts = crate::client_accounts::load_accounts();
        // Neither the hash nor any plaintext may survive in the envelope.
        assert!(!text.contains(&accounts[0].key_hash));
        assert!(
            !text.contains("笔记本"),
            "even the label is inside the cipher"
        );

        // And it round-trips through the passphrase.
        crate::client_accounts::save_accounts(&[]).unwrap();
        apply_import(&text, Some("correct horse"), ImportMode::Merge).unwrap();
        assert_eq!(crate::client_accounts::load_accounts().len(), 1);
    }

    /// A v1 bundle (no `client_accounts` field at all) must still import, and
    /// must not resurrect accounts it never carried.
    #[test]
    fn a_v1_bundle_without_the_accounts_field_still_imports() {
        let _dir = TempDataDir::new("v1-compat");
        seed(&[cred("token-a", "工作号")], &[]);

        // Build the document a 2.x writer produced: version 1, and no
        // `client_accounts` key in the JSON at all.
        let payload = BundlePayload {
            preferences: BundlePreferences::default(),
            credentials: vec![cred("token-a", "工作号")],
            api_keys: Vec::new(),
            client_accounts: Vec::new(),
        };
        let mut file: BundleFile = serde_json::from_str(&plain_bundle(&payload)).unwrap();
        file.version = 1;
        let text = serde_json::to_string(&file).unwrap();
        // `skip_serializing_if = "Vec::is_empty"` already omits the *top-level*
        // array, which is exactly what a 2.x writer produced — so this is a
        // faithful v1 document without hand-editing the JSON. (`counts` still
        // carries a zeroed counter; that field is `#[serde(default)]` on the
        // reader side, so its absence in a real v1 file is equally harmless.)
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(
            value.get("client_accounts").is_none(),
            "a v1 document carries no top-level client_accounts array: {text}"
        );

        let preview = apply_import(&text, None, ImportMode::Replace).unwrap();
        assert_eq!(preview.credentials_added, 1);
        assert_eq!(preview.client_accounts_added, 0);
        // No scary version warning: v1 is older, not newer.
        assert!(
            !preview.warnings.iter().any(|w| w.contains("更新版本")),
            "a v1 bundle is not a future version: {:?}",
            preview.warnings
        );
    }

    /// An account whose id disagrees with its hash is repaired, because the id
    /// is derivable from the hash without any plaintext.
    #[test]
    fn an_account_id_that_disagrees_with_its_hash_is_recomputed() {
        let _dir = TempDataDir::new("accounts-id-repair");
        let (_a, _key) = crate::client_accounts::create_account("x", "").unwrap();
        let mut accounts = crate::client_accounts::load_accounts();
        let expected = accounts[0].id.clone();
        accounts[0].id = "ca-000000".to_string();

        let payload = BundlePayload {
            preferences: BundlePreferences::default(),
            credentials: Vec::new(),
            api_keys: Vec::new(),
            client_accounts: accounts,
        };
        let preview = apply_import(&plain_bundle(&payload), None, ImportMode::Merge).unwrap();
        assert!(
            preview.warnings.iter().any(|w| w.contains("重新计算")),
            "the repair must be reported: {:?}",
            preview.warnings
        );
        let restored = crate::client_accounts::load_accounts();
        assert_eq!(restored[0].id, expected);
    }

    #[test]
    fn an_account_without_a_hash_is_dropped_with_a_warning() {
        let _dir = TempDataDir::new("accounts-empty-hash");
        let payload = BundlePayload {
            preferences: BundlePreferences::default(),
            credentials: Vec::new(),
            api_keys: Vec::new(),
            client_accounts: vec![crate::client_accounts::ClientAccount {
                id: "ca-abcdef".to_string(),
                label: "broken".to_string(),
                key_hash: String::new(),
                ..Default::default()
            }],
        };
        let preview = apply_import(&plain_bundle(&payload), None, ImportMode::Merge).unwrap();
        assert_eq!(preview.client_accounts_added, 0);
        assert!(
            preview.warnings.iter().any(|w| w.contains("缺少密钥哈希")),
            "the drop must be reported: {:?}",
            preview.warnings
        );
    }

    #[test]
    fn merge_keeps_local_accounts_and_replace_drops_them() {
        let _dir = TempDataDir::new("accounts-modes");
        seed_accounts(&["本地账户"]);
        let local_count = crate::client_accounts::load_accounts().len();
        assert_eq!(local_count, 1, "the temp dir starts empty");

        let payload = BundlePayload {
            preferences: BundlePreferences::default(),
            credentials: Vec::new(),
            api_keys: Vec::new(),
            client_accounts: vec![crate::client_accounts::ClientAccount {
                id: crate::client_accounts::id_from_hash(&crate::client_accounts::hash_key(
                    "prs_bundled",
                )),
                label: "来自导出".to_string(),
                key_hash: crate::client_accounts::hash_key("prs_bundled"),
                key_prefix: "prs_bundled".to_string(),
                enabled: true,
                ..Default::default()
            }],
        };
        let text = plain_bundle(&payload);

        let preview = apply_import(&text, None, ImportMode::Merge).unwrap();
        assert_eq!(preview.client_accounts_added, 1);
        assert_eq!(preview.client_accounts_removed, 0, "merge never removes");
        assert_eq!(
            crate::client_accounts::load_accounts().len(),
            local_count + 1
        );

        let preview = apply_import(&text, None, ImportMode::Replace).unwrap();
        assert_eq!(preview.client_accounts_removed, local_count + 1);
        let remaining = crate::client_accounts::load_accounts();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].label, "来自导出");
    }

    /// The most recent `*.proxy-rs-backup-*` beside `path`, if one exists.
    fn backup_of(path: &Path) -> Option<PathBuf> {
        let dir = path.parent()?;
        let prefix = format!("{}.proxy-rs-backup-", path.file_name()?.to_string_lossy());
        std::fs::read_dir(dir)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with(&prefix))
                    .unwrap_or(false)
            })
    }

    /// An unencrypted export must never carry a usable inbound key.
    ///
    /// The pool file is something users mail to themselves and leave in
    /// Downloads. It has always held upstream credentials, but inbound access
    /// keys were safe to include because only their hashes existed. Now that the
    /// plaintext is kept — so client configs can be written — an unencrypted
    /// export would become a list of working gateway keys. It is stripped on the
    /// way out, and this asserts it stays stripped.
    #[test]
    fn a_plaintext_export_does_not_leak_access_keys() {
        let _dir = TempDataDir::new("no-key-leak");
        let (_account, key) = crate::client_accounts::create_account("笔记本", "").expect("create");
        assert!(!key.is_empty());

        let (text, outcome) = export_text(&ExportOptions::default()).unwrap();
        assert!(!outcome.encrypted);
        assert!(
            !text.contains(&key),
            "an unencrypted export must not contain a usable access key"
        );
        assert!(
            text.contains("key_hash"),
            "the account entry is still exported, minus its plaintext"
        );
    }

    /// The encrypted branch drops the whole account list, as before.
    #[test]
    fn an_encrypted_export_carries_no_accounts_at_all() {
        let _dir = TempDataDir::new("enc-no-accounts");
        let (_a, _key) = crate::client_accounts::create_account("x", "").expect("create");
        let opts = ExportOptions {
            passphrase: Some("correct horse".to_string()),
        };
        let (text, outcome) = export_text(&opts).unwrap();
        assert!(outcome.encrypted);
        assert!(!text.contains("\"key_hash\""), "{text}");
    }
}
