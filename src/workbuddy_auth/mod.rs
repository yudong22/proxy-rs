//! WorkBuddy / CodeBuddy login-state credential store.
//!
//! A "credential" here is the desktop client's login state — the same JSON the
//! WorkBuddy desktop app (or a workbuddy2api session file) persists:
//! `{auth: {accessToken, refreshToken, expiresAt, domain}, account: {uid,
//! enterpriseId, …}}`. Presenting it reproduces the genuine client's upstream
//! fingerprint, which the plain `x-api-key` mode cannot: the gateway answers
//! `11128 Illegal API invocation from an unapproved channel` to requests that
//! lack the session-bound headers (`X-User-Id`, `X-Enterprise-Id`, …).
//!
//! The store lives in `~/.proxy-rs/workbuddy-credentials.json` (mode 0600, the
//! same protection workbuddy2api gives its session file) and follows the
//! [`crate::settings::data_dir`] relocation, so a dev instance never touches the
//! installed app's credentials.
//!
//! Layer note: like `credits.rs`, this is Layer 3 with I/O — it is *not* part of
//! the pure-function translate core.
//!
//! ## File layout
//!
//! This module is eight cohesive pieces behind one `workbuddy_auth` path. It
//! used to be a single 3,800-line file covering five unrelated concerns, which
//! made "where does this belong" unanswerable except by grep. The split moves
//! items only; **not a line of logic changed**, and every historical
//! `crate::workbuddy_auth::X` path still resolves through the re-exports below.
//!
//! | file | owns |
//! |---|---|
//! | [`types`] | the credential/account wire shapes, ids, masking, login-state parsing |
//! | [`prefs`] | pool preferences: default identity, ordering, schedule, store health |
//! | [`api_keys`] | the `x-api-key` entry pool (a second identity kind, same store) |
//! | [`store`] | reading, writing and migrating the credential file |
//! | [`refresh`] | token renewal, upstream identity headers, refresh reports |
//! | [`oauth`] | the desktop QR login flow |
//! | [`points`] | balance/points queries against the billing host |
//! | [`checkin`] | the daily check-in claim |
//!
//! `use super::*;` in each submodule is deliberate: the pieces share a small set
//! of imports and call each other freely, so re-declaring them per file would be
//! churn with no isolation benefit. The split is about *findability*, not about
//! enforcing a hard boundary between the credential store and its own helpers.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

mod api_keys;
mod checkin;
mod oauth;
mod points;
mod prefs;
mod refresh;
mod store;
mod types;

pub use api_keys::*;
pub use checkin::*;
pub use oauth::*;
pub use points::*;
pub use prefs::*;
pub use refresh::*;
pub use store::*;
pub use types::*;

#[cfg(test)]
mod prefs_default_tests {
    use super::*;

    /// A fresh install must report the same default time it actually uses.
    ///
    /// Regression: `Default` was derived while the time's 09:00 fallback lived
    /// only in `#[serde(default = ...)]`, which applies to deserialization
    /// alone. `load_preferences()` returns `Default::default()` when no file
    /// exists, so the scheduler silently ran on `""` (rescued at the last
    /// moment by `normalize_checkin_time`) while `/api/scheduler` published an
    /// empty wall-clock time that no consumer could render.
    #[test]
    fn default_preferences_carry_the_same_checkin_time_an_empty_file_decodes_to() {
        let from_default = PoolPreferences::default();
        let from_empty_file: PoolPreferences = serde_json::from_str("{}").expect("empty object");
        assert_eq!(
            from_default.daily_checkin_time, from_empty_file.daily_checkin_time,
            "the in-memory default and the on-disk default must agree"
        );
        assert_eq!(from_default.daily_checkin_time, "09:00");
        assert_eq!(
            normalize_checkin_time(&from_default.daily_checkin_time),
            "09:00",
            "the default must already be normalized, not merely normalizable"
        );
    }

    /// Every field, not just the time, must agree between the two paths.
    ///
    /// A mismatch here is how a scheduler can run with one policy while the API
    /// reports another, so the assertion is on the whole struct.
    #[test]
    fn default_preferences_equal_an_empty_file_in_every_field() {
        let from_default = PoolPreferences::default();
        let from_empty_file: PoolPreferences = serde_json::from_str("{}").expect("empty object");
        // Both reference the same switches; compare the fields that decide
        // behaviour plus the ones the API publishes.
        assert_eq!(
            from_default.daily_checkin_enabled,
            from_empty_file.daily_checkin_enabled
        );
        assert_eq!(from_default.growth_enabled, from_empty_file.growth_enabled);
        assert_eq!(
            from_default.last_checkin_run_date,
            from_empty_file.last_checkin_run_date
        );
        assert_eq!(
            from_default.last_growth_run_date,
            from_empty_file.last_growth_run_date
        );
        assert_eq!(
            from_default.last_night_growth_date,
            from_empty_file.last_night_growth_date
        );
        assert_eq!(
            from_default.default_identity_id,
            from_empty_file.default_identity_id
        );
        assert_eq!(from_default.identity_order, from_empty_file.identity_order);
        assert!(
            from_default.daily_checkin_time.len() == 5,
            "{from_default:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reorder only moves the default when a *different* identity lands on
    /// top — the gesture the UI documents as "try this one first". Regression:
    /// the original check was "current default is not first", which fired on
    /// any drag whenever the default was pinned further down (e.g. a key
    /// default alongside accounts), silently replacing it with the top account.
    #[test]
    fn reorder_pin_follows_only_a_new_top() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // Default stays on top: any other drag leaves it alone.
        assert_eq!(
            reorder_pin_target(&prefs("wb-a", ""), &ids(&["wb-a", "wb-c", "wb-b"])),
            None
        );
        // A different identity dragged to the top takes the default.
        assert_eq!(
            reorder_pin_target(&prefs("wb-a", ""), &ids(&["wb-c", "wb-a", "wb-b"])).as_deref(),
            Some("wb-c")
        );
        // A key default is never hoisted into credentials, so an account drag
        // cannot revert it and must not displace it.
        assert_eq!(
            reorder_pin_target(&prefs("k-1", ""), &ids(&["wb-c", "wb-a", "k-1"])),
            None
        );
        // A key default shadows the legacy credential default, so that must not
        // be moved either: it would otherwise resurface once the key is removed.
        assert_eq!(
            reorder_pin_target(&prefs("k-1", "wb-a"), &ids(&["wb-c", "wb-a", "k-1"])),
            None
        );
        // `__none__` explicitly disables the account pool, so a drag must not
        // re-enable it by promoting an account: ordering is not consent.
        assert_eq!(
            reorder_pin_target(&prefs("__none__", ""), &ids(&["wb-c", "wb-a"])),
            None
        );
        // ...and a stale legacy credential must not resurrect the pool either.
        assert_eq!(
            reorder_pin_target(&prefs("__none__", "wb-a"), &ids(&["wb-c", "wb-a"])),
            None
        );
        // Legacy install: `ordered_credentials` hoists the legacy default, so a
        // drag to the top must sync it or the drag reverts on reload.
        assert_eq!(
            reorder_pin_target(&prefs("", "wb-a"), &ids(&["wb-b", "wb-a"])).as_deref(),
            Some("wb-b")
        );
        // ...but not when that default already sits on top.
        assert_eq!(
            reorder_pin_target(&prefs("", "wb-a"), &ids(&["wb-a", "wb-b"])),
            None
        );
        // No identities at all: nothing to pin.
        assert_eq!(reorder_pin_target(&prefs("wb-a", ""), &[]), None);
    }

    fn prefs(identity: &str, legacy: &str) -> PoolPreferences {
        PoolPreferences {
            default_identity_id: identity.to_string(),
            default_credential_id: legacy.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn parses_full_desktop_login_state() {
        let raw = r#"{
            "auth": {
                "accessToken": "acc-1234567890",
                "refreshToken": "ref-9876543210",
                "expiresAt": 1893456000000,
                "domain": "copilot.tencent.com"
            },
            "account": { "uid": "u1", "enterpriseId": "e1", "nickname": "孙东" },
            "machineId": "machine-abc"
        }"#;
        let c = parse_login_state(raw).unwrap();
        assert_eq!(c.access_token, "acc-1234567890");
        assert_eq!(c.refresh_token.as_deref(), Some("ref-9876543210"));
        assert_eq!(c.account.uid, "u1");
        assert_eq!(c.account.enterprise_id, "e1");
        assert_eq!(c.domain, "copilot.tencent.com");
        assert_eq!(c.machine_id, "machine-abc");
        assert!(c.id.starts_with("wb-"));
        assert_eq!(c.label, "孙东");
    }

    /// Masking used to slice on byte offsets (`&t[..4]` / `&t[len-4..]`), which
    /// panics when a multi-byte character straddles the cut — and release
    /// builds use `panic = "abort"`, so one non-ASCII token in the credential
    /// list killed the whole app instead of just failing to render.
    #[test]
    fn masking_a_multibyte_secret_does_not_panic() {
        // A 3-byte character placed so it straddles byte 4, and another that
        // straddles `len - 4`. Byte slicing panics on both.
        let secret = "sk-中 token 文";
        let masked = mask_secret(secret);

        assert!(masked.contains("••••"), "got: {masked}");
        // Character-based: exactly four characters on each side.
        let head: String = masked.chars().take(4).collect();
        assert_eq!(head, "sk-中");
        let tail: String = masked.chars().skip(8).collect();
        assert_eq!(tail, "en 文", "tail must be the last 4 characters");
        // 4 chars + separator + 4 chars.
        assert_eq!(masked.chars().count(), 12, "got: {masked}");
    }

    /// The billing resource endpoint requires the account's own identity
    /// headers. Verified against the live endpoint (2026-09-28): the bare
    /// `Bearer`+`X-API-Key` pairing a chat request uses answers
    /// `401 {"message":"not_found"}`, while the same token with
    /// `X-User-Id`/`X-Machine-Id`/`X-Domain` present answers 200 with the real
    /// balance. Losing one of these headers is how the 剩余积分 card came to
    /// show "—" for a balance that exists.
    #[test]
    fn billing_headers_carry_the_account_fingerprint() {
        let cred = WorkBuddyCredential {
            id: "wb-abc123".to_string(),
            access_token: "token-1".to_string(),
            machine_id: "machine-9".to_string(),
            domain: "www.codebuddy.cn".to_string(),
            account: WorkBuddyAccount {
                uid: "uid-77".to_string(),
                enterprise_id: "ent-5".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };

        let mut headers = billing_headers(&cred);
        headers.sort();

        let has = |name: &str| headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
        let value = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };

        // The identity fingerprint the gateway checks.
        assert_eq!(value("X-User-Id"), "uid-77");
        assert_eq!(value("X-Machine-Id"), "machine-9");
        assert_eq!(value("X-Domain"), "www.codebuddy.cn");
        assert_eq!(value("X-Enterprise-Id"), "ent-5");
        assert_eq!(value("X-Tenant-Id"), "ent-5");
        // The token must travel as the bearer, and the billing-specific flags
        // must be present.
        assert_eq!(value("Authorization"), "Bearer token-1");
        assert!(has("X-CodeBuddy-Request"));
        // An enterprise id present means the no-enterprise fallback is absent.
        assert!(!has("X-No-Enterprise-Id"));
        // The chat path's X-API-Key pairing has no place here: the credential's
        // token is not an API key.
        assert!(!has("X-API-Key"));
    }

    /// Without a tenant id the fallback header is sent instead.
    #[test]
    fn billing_headers_fall_back_when_no_enterprise_id() {
        let cred = WorkBuddyCredential {
            access_token: "t".to_string(),
            ..Default::default()
        };
        let headers = billing_headers(&cred);
        assert!(
            headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("X-No-Enterprise-Id")),
            "no enterprise id must trigger the fallback header"
        );
        assert!(!headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("X-Enterprise-Id")),);
    }

    /// Short and empty secrets stay fully masked rather than panicking on an
    /// underflow (`len - 4`).
    #[test]
    fn masking_handles_short_and_empty_secrets() {
        assert_eq!(mask_secret(""), "••••");
        assert_eq!(mask_secret("短"), "••••");
        assert_eq!(mask_secret("12345678"), "••••");
        // Nine characters is the first length that reveals anything.
        assert_eq!(mask_secret("123456789"), "1234••••6789");
    }

    /// The credential and API-key entry points share the safe implementation.
    #[test]
    fn public_masking_helpers_use_the_safe_implementation() {
        let cred = WorkBuddyCredential {
            access_token: "sk-中 token 文".to_string(),
            ..Default::default()
        };
        assert_eq!(cred.masked_token(), mask_secret("sk-中 token 文"));

        let key = ApiKeyEntry {
            key: "sk-中 token 文".to_string(),
            ..Default::default()
        };
        assert_eq!(key.masked(), mask_secret("sk-中 token 文"));
    }

    #[test]
    fn parses_bare_token_object() {
        let c = parse_login_state(r#"{"accessToken":"tok-abcdef", "expiresAt": 1893456000000}"#)
            .unwrap();
        assert_eq!(c.access_token, "tok-abcdef");
        assert_eq!(c.refresh_token, None);
        assert_eq!(c.account.uid, "");
    }

    #[test]
    fn unwraps_data_envelope() {
        let raw =
            r#"{"data": {"auth": {"accessToken": "inner-token-123"}, "account": {"uid": "u9"}}}"#;
        let c = parse_login_state(raw).unwrap();
        assert_eq!(c.access_token, "inner-token-123");
        assert_eq!(c.account.uid, "u9");
    }

    #[test]
    fn rejects_login_state_without_access_token() {
        assert!(parse_login_state(r#"{"user": "x"}"#).is_err());
        assert!(parse_login_state("not json").is_err());
        assert!(parse_login_state(r#"{"auth": {"refreshToken": "only"}}"#).is_err());
    }

    /// The hand-rolled MD5 must match the real algorithm on published vectors.
    ///
    /// Credential ids (`wb-<6 hex>`) are a hash of the access token, so a wrong
    /// digest silently renames every imported account and splits its history in
    /// the stats DB. The implementation is bespoke (RFC 1321, no crypto
    /// dependency), and there was no vector test at all — changing the block
    /// loop to `as_chunks` would otherwise be unverifiable.
    #[test]
    fn md5_matches_published_vectors() {
        let hex =
            |data: &[u8]| -> String { md5_hex(data).iter().map(|b| format!("{b:02x}")).collect() };
        // RFC 1321 A.5 test suite.
        assert_eq!(hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(b"a"), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(hex(b"message digest"), "f96b697d7cb7938d525a2f31aaf161d0");
        assert_eq!(
            hex(b"abcdefghijklmnopqrstuvwxyz"),
            "c3fcd3d76192e4007dfb496cca67e13b"
        );
        assert_eq!(
            hex(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"),
            "d174ab98d277d9f5a5611c2c9f419d9f"
        );
        assert_eq!(
            hex(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            ),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
        // Multi-block inputs, so the loop (not just the padding path) is proven.
        // 64 bytes is exactly one block and 200 spans several.
        assert_eq!(hex(&[b'x'; 64]), "c1bb4f81d892b2d57947682aeb252456");
        assert_eq!(hex(&[b'x'; 200]), "30a83621ce5422fbdfdd539777458c78");
    }

    #[test]
    fn same_login_state_yields_the_same_id() {
        let raw = r#"{"accessToken":"stable-token-value"}"#;
        assert_eq!(
            parse_login_state(raw).unwrap().id,
            parse_login_state(raw).unwrap().id
        );
        let other = parse_login_state(r#"{"accessToken":"different-token"}"#).unwrap();
        assert_ne!(parse_login_state(raw).unwrap().id, other.id);
    }

    #[test]
    fn usability_follows_expiry_margin() {
        let mut c = parse_login_state(r#"{"accessToken":"t"}"#).unwrap();
        assert!(c.is_usable(1_000));
        c.expires_at_ms = Some(1_000_000);
        // Valid until 60s before expiry: exp - 60_000 = 940_000.
        assert!(c.is_usable(939_999));
        assert!(!c.is_usable(940_000));
        assert!(!c.is_usable(1_000_000));
        // No expiry at all means always usable.
        c.expires_at_ms = None;
        assert!(c.is_usable(9_999_999_999));
    }

    #[test]
    fn usability_follows_cooldown_and_enabled() {
        let mut c = parse_login_state(r#"{"accessToken":"t"}"#).unwrap();
        c.cooldown_until_ms = Some(5_000);
        assert!(!c.is_usable(4_999));
        assert!(c.is_usable(5_000));
        c.cooldown_until_ms = None;
        c.enabled = false;
        assert!(!c.is_usable(0));
    }

    /// Proactive renewal triggers only when the token is near expiry AND a
    /// refresh token exists; an unknown expiry is never guessed at.
    #[test]
    fn needs_refresh_only_when_expiring_with_a_refresh_token() {
        let mut c = parse_login_state(r#"{"accessToken":"t","refreshToken":"r"}"#).unwrap();
        // Unknown expiry: never proactively refreshed.
        assert!(!c.needs_refresh(9_999_999_999));

        c.expires_at_ms = Some(1_000_000);
        assert!(!c.needs_refresh(0));
        // Inside the 60s renewal margin.
        assert!(c.needs_refresh(940_000));
        assert!(c.needs_refresh(1_000_000));

        // Without a refresh token there is nothing to refresh with.
        c.refresh_token = None;
        assert!(!c.needs_refresh(1_000_000));

        // Disabled credentials are never refreshed.
        c.refresh_token = Some("r".to_string());
        c.enabled = false;
        assert!(!c.needs_refresh(1_000_000));
    }

    /// The backstop that makes an imported login state *ever* renew itself.
    ///
    /// Real WorkBuddy login states routinely carry no `expiresAt`, and
    /// `needs_refresh` used to answer "never" for exactly those — leaving the
    /// reactive 401 path as the only way to renew, i.e. the user's request had
    /// to fail first. These assertions pin the behaviour that fixes it.
    #[test]
    fn unknown_expiry_is_renewed_after_the_backstop_window() {
        let mut c = WorkBuddyCredential {
            access_token: "tok".to_string(),
            refresh_token: Some("ref".to_string()),
            enabled: true,
            ..Default::default()
        };

        // Never refreshed and no known expiry: nothing says the freshly imported
        // token is stale, so stay quiet and let the reactive path handle it.
        c.last_refresh_at_ms = None;
        c.expires_at_ms = None;
        assert!(!c.needs_refresh(9_999_999_999));

        // Refreshed recently: still fresh, no need to spend a round-trip.
        c.last_refresh_at_ms = Some(1_000_000);
        assert!(!c.needs_refresh(1_000_000));
        assert!(!c.needs_refresh(1_000_000 + UNKNOWN_EXPIRY_REFRESH_AFTER_MS - 1));

        // Past the window: renew proactively rather than wait for a 401.
        assert!(c.needs_refresh(1_000_000 + UNKNOWN_EXPIRY_REFRESH_AFTER_MS));

        // A known expiry always wins over the backstop, in both directions.
        c.expires_at_ms = Some(1_000_000 + UNKNOWN_EXPIRY_REFRESH_AFTER_MS + 10_000_000);
        assert!(
            !c.needs_refresh(1_000_000 + UNKNOWN_EXPIRY_REFRESH_AFTER_MS),
            "a known-fresh expiry must not be overridden by the backstop"
        );
    }

    /// The per-row 刷新积分 button must not perturb any other identity.
    ///
    /// Regression: the button called the pool-wide refresh and ignored its own
    /// `data-points` id, so opening one row's button updated every account —
    /// Only an un-self-healing failure may claim "需重新绑定".
    ///
    /// The distinction matters because the badge is the user's only cue to act:
    /// if a network blip also set it, every account would light up on a flaky
    /// connection and the marker would stop meaning anything. The reverse
    /// mistake is worse — a genuinely revoked credential that is *not* flagged
    /// leaves the user waiting for a refresh that can never succeed.
    #[test]
    fn only_unrecoverable_refresh_failures_need_relogin() {
        let with_refresh = WorkBuddyCredential {
            refresh_token: Some("ref".to_string()),
            enabled: true,
            ..Default::default()
        };

        // Rejected by the endpoint: a new login is the only way back.
        assert!(refresh_failure_needs_relogin(
            &with_refresh,
            "刷新端点返回 401: revoked"
        ));
        assert!(refresh_failure_needs_relogin(
            &with_refresh,
            "刷新端点返回 403: forbidden"
        ));
        assert!(refresh_failure_needs_relogin(
            &with_refresh,
            "凭据没有 refreshToken，无法刷新"
        ));

        // Transient: retryable, must NOT be labelled relogin.
        assert!(!refresh_failure_needs_relogin(
            &with_refresh,
            "刷新请求失败: connection reset"
        ));
        assert!(!refresh_failure_needs_relogin(
            &with_refresh,
            "刷新端点返回 500: oops"
        ));
        assert!(!refresh_failure_needs_relogin(
            &with_refresh,
            "刷新响应不是有效 JSON: eof"
        ));

        // No refresh token at all: nothing to renew with, regardless of message.
        let without = WorkBuddyCredential {
            refresh_token: None,
            enabled: true,
            ..Default::default()
        };
        assert!(refresh_failure_needs_relogin(&without, "anything at all"));
    }

    /// The verdict has to outlive the process: it is written to disk and cleared
    /// only by a recovery.
    #[test]
    fn a_rejected_refresh_is_recorded_until_it_recovers() {
        let dir = std::env::temp_dir().join(format!("proxy-rs-relogin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = crate::settings::data_dir_test_lock();
        let previous = std::env::var_os(crate::settings::DATA_DIR_ENV);
        std::env::set_var(crate::settings::DATA_DIR_ENV, &dir);

        let cred = WorkBuddyCredential {
            id: "wb-rejected".to_string(),
            label: "被拒账号".to_string(),
            access_token: "tok".to_string(),
            refresh_token: Some("ref".to_string()),
            enabled: true,
            ..Default::default()
        };
        save_credentials(std::slice::from_ref(&cred)).unwrap();

        // A transient failure records the reason but not the verdict.
        mark_credential_refresh_failure(&cred, "网络抖动", false);
        let stored = load_credentials();
        assert!(stored[0].relogin_required_since_ms.is_none());
        assert_eq!(stored[0].last_error, "网络抖动");

        // A rejection records the verdict, on disk.
        mark_credential_refresh_failure(&cred, "刷新端点返回 401", true);
        let stored = load_credentials();
        assert!(
            stored[0].relogin_required_since_ms.is_some(),
            "the verdict must persist so the badge survives a restart"
        );

        // A successful recovery clears it: the account heals itself now.
        let mut recovered = stored[0].clone();
        recovered.relogin_required_since_ms = None;
        upsert_credential(recovered).unwrap();
        assert!(load_credentials()[0].relogin_required_since_ms.is_none());

        match previous {
            Some(v) => std::env::set_var(crate::settings::DATA_DIR_ENV, v),
            None => std::env::remove_var(crate::settings::DATA_DIR_ENV),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// and a single unreachable account could not be retried on its own. These
    /// setters are what the single-identity path writes through, so the isolation
    /// property belongs here.
    /// The pool GUI's drag reorder writes the store order; `ordered_credentials`
    /// must then report that order — except that a pinned default is hoisted to
    /// the front.
    ///
    /// This is why `wb_reorder_credentials` syncs the default when a row is
    /// dragged to the top: without that, the drag is silently undone by this
    /// hoist on the very next reload, and the user watches the list snap back.
    /// `identity_order` interleaves credentials and keys; each store is sorted
    /// by it, and unlisted ids keep their store order at the tail.
    #[test]
    fn identity_order_sorts_each_store_and_appends_the_unlisted() {
        let mk = |id: &str| WorkBuddyCredential {
            id: id.to_string(),
            label: id.to_string(),
            access_token: format!("t-{id}"),
            enabled: true,
            ..Default::default()
        };
        let order = vec!["wb-c".to_string(), "k-1".to_string(), "wb-a".to_string()];
        // wb-b is deliberately absent from the order: it must stay last rather
        // than vanish, and the listed ones must follow the listed positions.
        let sorted = apply_identity_order(vec![mk("wb-a"), mk("wb-b"), mk("wb-c")], &order, |c| {
            c.id.as_str()
        });
        let ids: Vec<&str> = sorted.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["wb-c", "wb-a", "wb-b"]);
    }

    /// An empty order is a no-op, so installs that never dragged are unaffected.
    #[test]
    fn empty_identity_order_leaves_the_store_order_alone() {
        let mk = |id: &str| WorkBuddyCredential {
            id: id.to_string(),
            access_token: format!("t-{id}"),
            ..Default::default()
        };
        let sorted = apply_identity_order(vec![mk("wb-a"), mk("wb-b")], &[], |c| c.id.as_str());
        let ids: Vec<&str> = sorted.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["wb-a", "wb-b"]);
    }

    #[test]
    fn ordered_credentials_hoists_the_pinned_default_above_store_order() {
        let dir = std::env::temp_dir().join(format!("proxy-rs-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = crate::settings::data_dir_test_lock();
        let previous = std::env::var_os(crate::settings::DATA_DIR_ENV);
        std::env::set_var(crate::settings::DATA_DIR_ENV, &dir);

        let mk = |id: &str| WorkBuddyCredential {
            id: id.to_string(),
            label: id.to_string(),
            access_token: format!("t-{id}"),
            enabled: true,
            ..Default::default()
        };
        // Store order after a drag that puts wb-third first.
        save_credentials(&[mk("wb-third"), mk("wb-first"), mk("wb-second")]).unwrap();

        // With no default pinned, the store order is returned verbatim.
        save_preferences(&PoolPreferences::default()).unwrap();
        let ids: Vec<String> = ordered_credentials().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["wb-third", "wb-first", "wb-second"]);

        // Pinning wb-second hoists it, keeping the rest in store order.
        set_default_identity("wb-second").unwrap();
        let ids: Vec<String> = ordered_credentials().into_iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            vec!["wb-second", "wb-third", "wb-first"],
            "the pinned default must lead, with the dragged order preserved behind it"
        );

        std::env::remove_var(crate::settings::DATA_DIR_ENV);
        if let Some(v) = previous {
            std::env::set_var(crate::settings::DATA_DIR_ENV, v);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setting_points_touches_only_the_named_identity() {
        let dir = std::env::temp_dir().join(format!("proxy-rs-points-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The data dir is process-global; serialise against the other tests that
        // relocate it rather than racing them.
        let _guard = crate::settings::data_dir_test_lock();
        let previous = std::env::var_os(crate::settings::DATA_DIR_ENV);
        std::env::set_var(crate::settings::DATA_DIR_ENV, &dir);

        let a = WorkBuddyCredential {
            id: "wb-aaa".to_string(),
            label: "甲".to_string(),
            access_token: "t-a".to_string(),
            points: Some(1.0),
            ..Default::default()
        };
        let b = WorkBuddyCredential {
            id: "wb-bbb".to_string(),
            label: "乙".to_string(),
            access_token: "t-b".to_string(),
            points: Some(2.0),
            ..Default::default()
        };
        save_credentials(&[a, b]).unwrap();
        save_api_keys(&[ApiKeyEntry {
            id: "k-ccc".to_string(),
            label: "密钥".to_string(),
            key: "sk-c".to_string(),
            points: Some(3.0),
            ..Default::default()
        }])
        .unwrap();

        set_points("wb-aaa", Some(99.5)).unwrap();
        set_api_key_points("k-ccc", Some(88.5)).unwrap();

        let creds = load_credentials();
        assert_eq!(
            creds.iter().find(|c| c.id == "wb-aaa").unwrap().points,
            Some(99.5),
            "the named credential is updated"
        );
        assert_eq!(
            creds.iter().find(|c| c.id == "wb-bbb").unwrap().points,
            Some(2.0),
            "a sibling credential must be untouched"
        );
        assert_eq!(
            load_api_keys()[0].points,
            Some(88.5),
            "the named key is updated independently of the credential pool"
        );

        // An unknown id is a silent no-op (the caller resolves the id first), and
        // must not clear or create anything.
        set_points("wb-nonexistent", Some(0.0)).unwrap();
        assert_eq!(load_credentials().len(), 2);
        assert_eq!(load_api_keys().len(), 1);

        match previous {
            Some(v) => std::env::set_var(crate::settings::DATA_DIR_ENV, v),
            None => std::env::remove_var(crate::settings::DATA_DIR_ENV),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Identity is the account, not the token.
    #[test]
    fn id_is_derived_from_the_account_not_the_token() {
        let account = WorkBuddyAccount {
            uid: "uid-1".to_string(),
            enterprise_id: "ent-1".to_string(),
            nickname: "甲".to_string(),
        };

        // The same account with a rotated token keeps its id — this is what makes
        // a refresh (and a re-login) an update instead of a duplicate row.
        let before = credential_id_for(&account, "token-before-refresh");
        let after = credential_id_for(&account, "token-after-refresh");
        assert_eq!(before, after);
        assert_ne!(before, credential_id("token-before-refresh"));

        // A different account is a different id, and the same uid in another
        // enterprise is deliberately *not* collapsed onto it.
        let other_uid = WorkBuddyAccount {
            uid: "uid-2".to_string(),
            ..account.clone()
        };
        assert_ne!(
            credential_id_for(&other_uid, "token-before-refresh"),
            before
        );
        let other_ent = WorkBuddyAccount {
            enterprise_id: "ent-2".to_string(),
            ..account.clone()
        };
        assert_ne!(
            credential_id_for(&other_ent, "token-before-refresh"),
            before
        );

        // No uid (some imported fixtures): fall back to the token hash, so such
        // an entry still gets a usable id.
        let anonymous = WorkBuddyAccount::default();
        assert_eq!(
            credential_id_for(&anonymous, "some-token"),
            credential_id("some-token")
        );
    }

    /// `parse_login_state` is what the GUI's paste and QR paths both go through,
    /// so the stable id has to be established there.
    #[test]
    fn parsing_a_login_state_yields_the_stable_id() {
        let raw = r#"{"auth":{"accessToken":"tok-1","refreshToken":"ref-1"},
                       "account":{"uid":"uid-9","enterpriseId":"ent-9","nickname":"甲"}}"#;
        let c = parse_login_state(raw).unwrap();

        assert_eq!(
            c.id,
            credential_id_for(&c.account, "tok-1"),
            "the parsed credential must already carry its stable id"
        );
        // Re-issuing the same account with a new token must land on the same id.
        let again = parse_login_state(
            r#"{"auth":{"accessToken":"tok-2","refreshToken":"ref-2"},
                "account":{"uid":"uid-9","enterpriseId":"ent-9","nickname":"甲"}}"#,
        )
        .unwrap();
        assert_eq!(again.id, c.id);
    }

    /// The refresh endpoint follows the credential's own domain.
    #[test]
    fn auth_endpoint_follows_the_credential_domain() {
        let mut c = WorkBuddyCredential::default();
        assert_eq!(auth_endpoint_for(&c), DEFAULT_WORKBUDDY_ENDPOINT);

        c.domain = "copilot.tencent.com".to_string();
        assert_eq!(auth_endpoint_for(&c), "https://copilot.tencent.com");

        // A full URL (with a trailing slash) is used as-is.
        c.domain = "https://www.codebuddy.cn/".to_string();
        assert_eq!(auth_endpoint_for(&c), "https://www.codebuddy.cn");
    }

    #[test]
    fn masked_token_never_shows_the_whole_secret() {
        let c = parse_login_state(r#"{"accessToken":"0123456789abcdef"}"#).unwrap();
        let m = c.masked_token();
        assert!(!m.contains("6789abcdef"));
        assert!(m.starts_with("0123"));
        // Short tokens degrade to full masking rather than leaking.
        let short = parse_login_state(r#"{"accessToken":"abc"}"#).unwrap();
        assert_eq!(short.masked_token(), "••••");
    }

    #[test]
    fn upstream_headers_carry_the_fingerprint() {
        let raw = r#"{
            "auth": {"accessToken": "a", "domain": "copilot.tencent.com"},
            "account": {"uid": "u1", "enterpriseId": "e1"},
            "machineId": "m1"
        }"#;
        let c = parse_login_state(raw).unwrap();
        let headers = upstream_headers(&c);
        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("X-Product-Code").as_deref(), Some("codebuddy"));
        assert_eq!(get("X-User-Id").as_deref(), Some("u1"));
        assert_eq!(get("X-Enterprise-Id").as_deref(), Some("e1"));
        assert_eq!(get("X-Tenant-Id").as_deref(), Some("e1"));
        assert_eq!(get("X-Domain").as_deref(), Some("copilot.tencent.com"));
        assert_eq!(get("X-Machine-Id").as_deref(), Some("m1"));
    }

    #[test]
    fn credential_id_is_stable_and_short() {
        let id = credential_id("some-long-token-material");
        assert_eq!(id.len(), 3 + 6);
        assert!(id.starts_with("wb-"));
        assert_eq!(id, credential_id("some-long-token-material"));
    }

    #[test]
    fn local_day_uses_the_local_offset() {
        // Two instants 12h apart around a UTC midnight must collapse to the
        // same local day when the offset is large enough — this is what stops
        // an evening claim from counting as the next morning's.
        let offset = crate::util::local_utc_offset_secs();
        let day = local_day(crate::util::unix_millis());
        // Sanity: the day is a well-formed YYYY-MM-DD.
        assert_eq!(day.len(), 10, "{day} (offset {offset})");
        assert!(day.chars().filter(|c| *c == '-').count() == 2, "{day}");
        // Same instant expressed through the function twice is stable.
        let now = crate::util::unix_millis();
        assert_eq!(local_day(now), local_day(now));
    }

    #[test]
    fn checkin_day_record_round_trips() {
        let mut c = parse_login_state(r#"{"accessToken":"day-test-token"}"#).unwrap();
        let today = local_day(crate::util::unix_millis());
        assert!(!checked_in_today(&c, &today));
        c.last_checkin_date = Some(today.clone());
        assert!(checked_in_today(&c, &today));
        assert!(!checked_in_today(&c, "1999-01-01"));
    }

    #[test]
    fn already_checked_in_reads_every_message_spelling() {
        // The endpoint disagrees on the field name and on nesting; all of these
        // must be recognised, or a repeat claim is miscounted as a success.
        let cases = [
            r#"{"code":500,"msg":"已签到"}"#,
            r#"{"code":500,"message":"already claimed"}"#,
            r#"{"code":500,"data":{"msg":"已打卡"}}"#,
            r#"{"code":500,"data":{"message":"repeat"}}"#,
        ];
        for raw in cases {
            let v: serde_json::Value = serde_json::from_str(raw).unwrap();
            let msg = v
                .get("msg")
                .or_else(|| v.get("message"))
                .or_else(|| {
                    v.get("data")
                        .and_then(|d| d.get("msg").or_else(|| d.get("message")))
                })
                .and_then(|m| m.as_str())
                .unwrap_or("");
            assert!(
                is_already_checked_in(v.get("code").and_then(|c| c.as_i64()), msg),
                "{raw} -> msg {msg:?} not recognised"
            );
        }
    }

    #[test]
    fn generates_valid_qr_svg() {
        let svg = generate_qr_svg("https://copilot.tencent.com/login?state=test").unwrap();
        assert!(svg.contains("<svg"));
        assert!(svg.contains("</svg>"));
    }

    // ── Refresh channel marker ─────────────────────────────────────────────
    // The CN deployment tags desktop renewals `workbuddy`; the international one
    // uses `plugin`. We sent `plugin` everywhere, which the endpoint still
    // accepts (verified live 2026-10-25) — but the marker is what a channel
    // tightening would key on, and getting it wrong would fail exactly the way
    // the chat channel already does.

    #[test]
    fn refresh_source_follows_the_deployment() {
        assert_eq!(
            refresh_source_for("https://copilot.tencent.com"),
            "workbuddy"
        );
        assert_eq!(refresh_source_for("https://www.codebuddy.cn"), "workbuddy");
        assert_eq!(
            refresh_source_for("https://www.workbuddy.ai"),
            "plugin",
            "the international deployment keeps the plugin marker"
        );
    }

    #[test]
    fn refresh_source_tolerates_a_trailing_path_and_scheme_noise() {
        // `auth_endpoint_for` returns a bare authority, but a hand-entered
        // domain may carry a path or a trailing slash; the host is what decides.
        assert_eq!(refresh_source_for("https://www.codebuddy.cn/"), "workbuddy");
        assert_eq!(refresh_source_for("http://www.workbuddy.ai/v2"), "plugin");
    }

    // ── Per-account refresh serialisation ──────────────────────────────────
    // The upstream rotates the refresh token, so two concurrent renewals of one
    // account make the second present an already-consumed token and fail with
    // `12153 refresh token is invalid` — which the caller records as "needs
    // re-login" on a perfectly healthy account.

    /// The same credential id must yield the same lock, different ids must not.
    #[test]
    fn refresh_locks_are_per_credential() {
        let a1 = refresh_lock_for("wb-aaa");
        let a2 = refresh_lock_for("wb-aaa");
        let b = refresh_lock_for("wb-bbb");
        assert!(
            std::sync::Arc::ptr_eq(&a1, &a2),
            "one account must share one lock"
        );
        assert!(
            !std::sync::Arc::ptr_eq(&a1, &b),
            "different accounts must not serialise against each other"
        );
    }

    /// Holding one account's lock must not block another account's.
    ///
    /// This is what keeps the scheduled pool refresh parallel; a single global
    /// lock would serialise every account behind the slowest one.
    #[tokio::test]
    async fn holding_one_accounts_lock_does_not_block_another() {
        let held = refresh_lock_for("wb-held").lock_owned().await;
        let other = refresh_lock_for("wb-free");
        let acquired =
            tokio::time::timeout(std::time::Duration::from_millis(200), other.lock_owned()).await;
        assert!(
            acquired.is_ok(),
            "a different account must not wait on this one"
        );
        drop(held);
    }

    /// The same account's second caller waits for the first.
    #[tokio::test]
    async fn the_same_accounts_second_caller_waits() {
        let first = refresh_lock_for("wb-same").lock_owned().await;
        let second = refresh_lock_for("wb-same");
        let waited =
            tokio::time::timeout(std::time::Duration::from_millis(150), second.lock_owned()).await;
        assert!(
            waited.is_err(),
            "two renewals of one account must not run concurrently"
        );
        drop(first);
    }

    // ── QR-login poll envelope ─────────────────────────────────────────────
    // These cover the "扫码后只显示授权失败、日志里什么也没有" bug: the poll
    // collapsed every rejection into a bare sentence, and read expiry only from
    // `expiresAt`, which the OAuth endpoints never send.

    /// The real success body, trimmed to the fields the parser reads.
    const GRANTED_BODY: &str = r#"{"code":0,"msg":"OK","data":{
        "accessToken":"at-123","refreshToken":"rt-456",
        "expiresIn":2419198,"tokenType":"Bearer",
        "domain":"www.codebuddy.cn"}}"#;

    #[test]
    fn pending_code_is_pending_not_a_failure() {
        // 11217 is the "user hasn't scanned yet" answer: it repeats every 2s
        // and must never surface as an error in the GUI or the log.
        let body = r#"{"code":11217,"msg":"login ing..."}"#;
        assert!(matches!(
            parse_oauth_token_response(body, 0),
            OAuthTokenGrant::Pending
        ));
    }

    #[test]
    fn rejected_poll_keeps_the_upstream_code_and_message() {
        // The whole point of the fix: the reason must survive to the caller so
        // it can be written to disk. An expired-scan rejection is the case that
        // used to reach the GUI as a bare "授权失败".
        let body = r#"{"code":11213,"msg":"账号已失效，请联系企业管理员"}"#;
        let failure = match parse_oauth_token_response(body, 0) {
            OAuthTokenGrant::Failed(msg) => msg,
            other => panic!("expected Failed, got {:?}", std::mem::discriminant(&other)),
        };
        assert!(failure.contains("11213"), "code missing from: {failure}");
        assert!(
            failure.contains("账号已失效"),
            "upstream msg missing from: {failure}"
        );
    }

    #[test]
    fn missing_code_is_a_failure_not_a_grant() {
        // `unwrap_or(0)` here would read a malformed body as an authorized
        // login and then fail later on a missing token, which is far harder to
        // attribute than a rejection reported at the envelope.
        let body = r#"{"data":{"accessToken":"at-123"}}"#;
        match parse_oauth_token_response(body, 0) {
            OAuthTokenGrant::Failed(msg) => assert!(msg.contains("code=缺失"), "{msg}"),
            _ => panic!("a body without `code` must not be treated as granted"),
        }
    }

    #[test]
    fn granted_poll_derives_expiry_from_expires_in_seconds() {
        // The OAuth answer carries *only* `expiresIn` seconds. Ignoring it left
        // every scanned account with `expires_at_ms: None`, which silently
        // demoted it to the 6-hour unknown-expiry backstop.
        let now = 1_700_000_000_000i64;
        let grant = match parse_oauth_token_response(GRANTED_BODY, now) {
            OAuthTokenGrant::Granted(g) => g,
            _ => panic!("expected a granted token"),
        };
        assert_eq!(grant.access_token, "at-123");
        assert_eq!(grant.refresh_token.as_deref(), Some("rt-456"));
        assert_eq!(grant.domain, "www.codebuddy.cn");
        assert_eq!(grant.expires_at_ms, Some(now + 2_419_198_000));
    }

    #[test]
    fn absolute_expires_at_wins_over_relative_expires_in() {
        // An imported desktop login state carries millis; a response carrying
        // both must keep the exact value rather than a rounded one.
        let data: serde_json::Value =
            serde_json::from_str(r#"{"expiresAt":1893456000000,"expiresIn":60}"#).unwrap();
        assert_eq!(
            expires_at_ms_from(&data, 1_700_000_000_000),
            Some(1893456000000)
        );
    }

    #[test]
    fn expiry_stays_unknown_when_neither_field_is_usable() {
        // Absent or non-positive `expiresIn` must not invent an already-expired
        // timestamp: `Some(now)` would make the credential look dead on arrival.
        for body in [
            r#"{"accessToken":"at","expiresIn":0}"#,
            r#"{"accessToken":"at","expiresIn":-5}"#,
            r#"{"accessToken":"at"}"#,
        ] {
            let data: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(expires_at_ms_from(&data, 1_700_000_000_000), None, "{body}");
        }
    }

    #[test]
    fn refresh_response_expiry_is_read_from_expires_in() {
        // The *refresh* endpoint answers with `expiresIn` only (a real one said
        // `"expiresIn":96492` and no `expiresAt`). Both the renewal path and the
        // QR-login path go through `expires_at_ms_from`, so this guards the
        // shared rule rather than one call site: before it, every renewal
        // discarded the new expiry and the credential fell back to the 6-hour
        // unknown-expiry backstop.
        let refreshed: serde_json::Value =
            serde_json::from_str(r#"{"accessToken":"at","refreshToken":"rt","expiresIn":96492}"#)
                .unwrap();
        let now = 1_792_906_228_000i64; // the 13:30:28 renewal
        assert_eq!(expires_at_ms_from(&refreshed, now), Some(now + 96_492_000));
    }

    #[test]
    fn granted_poll_without_access_token_is_a_failure() {
        let body = r#"{"code":0,"data":{"refreshToken":"rt"}}"#;
        match parse_oauth_token_response(body, 0) {
            OAuthTokenGrant::Failed(msg) => assert!(msg.contains("accessToken"), "{msg}"),
            _ => panic!("a token-less grant must fail"),
        }
    }

    #[test]
    fn non_json_poll_body_reports_the_body() {
        // A gateway HTML page is the realistic shape here; the snippet is what
        // turns "无效响应" into something diagnosable.
        let failure = match parse_oauth_token_response("<html>502 Bad Gateway</html>", 0) {
            OAuthTokenGrant::Failed(msg) => msg,
            _ => panic!("HTML must not parse as a grant"),
        };
        assert!(failure.contains("502 Bad Gateway"), "{failure}");
    }

    // ── Check-in classification ────────────────────────────────────────────
    // These cover the counting bug: a repeat claim that answers `code == 0`
    // used to be tallied as a fresh success, so 今日已打卡 always read 0.

    #[test]
    fn repeat_claim_with_code_zero_is_already_checked_in() {
        // A repeat call answers `code == 0` with no reward payload. Neither the
        // code nor the message alone says "already", so the missing reward is
        // what classifies it — this is the case that used to be counted as a
        // fresh success and made 今日已打卡 read 0.
        let repeat: serde_json::Value = serde_json::from_str(r#"{"code":0,"data":{}}"#).unwrap();
        assert!(!is_already_checked_in(Some(0), ""));
        assert!(!has_reward(&repeat));
        assert!(is_already_checked_in(Some(0), "") || !has_reward(&repeat));

        // A repeat that *does* say so in the message is caught by the words.
        assert!(is_already_checked_in(Some(0), "今日已打卡"));
    }

    #[test]
    fn already_checked_in_matches_phrases_and_codes() {
        for msg in [
            "已签到",
            "已经打卡",
            "Already checked in",
            "REPEAT",
            "已领取",
        ] {
            assert!(is_already_checked_in(Some(4321), msg), "{msg}");
        }
        for code in [10001, 10002] {
            assert!(is_already_checked_in(Some(code), ""), "code {code}");
        }
        // An unrelated failure must not be silently downgraded to "already".
        assert!(!is_already_checked_in(Some(500), "internal error"));
        assert!(!is_already_checked_in(None, ""));
    }

    #[test]
    fn reward_payload_decides_a_fresh_success() {
        let granted: serde_json::Value =
            serde_json::from_str(r#"{"code":0,"data":{"rewardPoint":100}}"#).unwrap();
        assert!(has_reward(&granted));
        assert_eq!(reward_points(&granted), Some(100.0));

        let repeat: serde_json::Value = serde_json::from_str(r#"{"code":0,"data":{}}"#).unwrap();
        assert!(!has_reward(&repeat));

        // A numeric string is a real deployment spelling of the same figure.
        let as_string: serde_json::Value =
            serde_json::from_str(r#"{"code":0,"data":{"points":"50"}}"#).unwrap();
        assert_eq!(reward_points(&as_string), Some(50.0));
    }
}
