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
//! ## File layout
//!
//! Split from one 1,000-line file. The grouping is the *subject* of each
//! operation — settings, identities, client accounts, pool transfer, DSH config,
//! points, sessions, stats — which is also how `admin_rpc.rs` and
//! `admin_routes.rs` dispatch to it, so a new endpoint now has an obvious home.
//!
//! | file | owns |
//! |---|---|
//! | [`settings`] | read/save the GUI settings and hot-apply the inbound gate |
//! | [`identities`] | the credential + api-key pool: add/remove/toggle/default/reorder |
//! | [`client_accounts`] | inbound access-key accounts (CRUD) |
//! | [`pool`] | account-pool export / import preview / apply |
//! | [`dsh`] | reading and writing the DeepSeek Harness config |
//! | [`points`] | balance refresh for one or all identities |
//! | [`sessions`] | Claude Code session browsing (list / detail / sub-agent) |
//! | [`stats`] | the today aggregate |
//!
//! Every historical `crate::admin::X` path still resolves through the
//! re-exports in `mod.rs`.

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

// `ProcessInfo` lives with the admin state now (it describes the state's
// process). Re-exported so every existing `admin::ProcessInfo` path keeps
// working.

// `ProcessInfo` lives with the admin state now (it describes the state's
// process). Re-exported so every existing `admin::ProcessInfo` path keeps
// working.
pub use crate::admin_state::{current_identity_summary, ProcessInfo};

mod ops_client_accounts;
mod ops_dsh;
mod ops_identities;
mod ops_points;
mod ops_pool;
mod ops_sessions;
mod ops_settings;
mod ops_stats;

pub use ops_client_accounts::*;
pub use ops_dsh::*;
pub use ops_identities::*;
pub use ops_points::*;
pub use ops_pool::*;
pub use ops_sessions::*;
pub use ops_settings::*;
pub use ops_stats::*;

#[cfg(test)]
mod tests {
    use super::*;

    /// Saving the settings form must never clear the stored key.
    ///
    /// Regression: the form has no `api_key` field, so it posted an empty
    /// string on every save and the backend assigned it straight through —
    /// silently wiping a value `migrate_legacy_api_key` reads at startup.
    #[test]
    fn save_settings_keeps_the_stored_key_when_the_form_omits_it() {
        assert_eq!(resolve_api_key("", "ck-stored"), "ck-stored");
        assert_eq!(resolve_api_key("   ", "ck-stored"), "ck-stored");
    }

    /// A masked value is not a key: it is the UI echoing back a secret it was
    /// never given in the clear, so it must not overwrite the real one.
    ///
    /// Both mask characters are rejected. The desktop used to reject `•` and
    /// `*`, the server console only `•` — a defensive divergence that would
    /// become a bug the first time a `*` mask was produced.
    #[test]
    fn save_settings_does_not_store_a_masked_placeholder() {
        assert_eq!(resolve_api_key("ck-••••wxyz", "ck-stored"), "ck-stored");
        assert_eq!(resolve_api_key("ck-****wxyz", "ck-stored"), "ck-stored");
    }

    /// A genuine new key still replaces the old one, trimmed.
    #[test]
    fn save_settings_accepts_a_real_replacement_key() {
        assert_eq!(resolve_api_key("ck-new", "ck-stored"), "ck-new");
        assert_eq!(resolve_api_key("  ck-new  ", "ck-stored"), "ck-new");
        assert_eq!(resolve_api_key("", ""), "");
    }

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
