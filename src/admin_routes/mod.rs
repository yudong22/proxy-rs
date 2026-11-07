//! HTTP routes for the server-mode admin console (`/admin/api/*`).
//!
//! ## Why a separate router from [`crate::router`]
//!
//! `router::build_app_router` serves the *data plane*: `/v1/*` for clients, plus
//! the read-only `/api/*` observation surface. Its trust model is documented and
//! load-bearing — `/api/*` is unauthenticated because the desktop console reads
//! it over loopback with `fetch`, and the public edge is expected to hide it at
//! the reverse proxy.
//!
//! These routes are the opposite: they are **authenticated** (session cookie),
//! they **mutate** stored credentials, and they are the only surface a browser is
//! meant to reach. Mixing them into the data router would mean one router with
//! two contradictory authentication stories, which is how a route eventually ends
//! up on the wrong side of the line.
//!
//! ## What is deliberately absent
//!
//! No service start/stop (the process lifecycle belongs to systemd), no "open log
//! directory" (no file manager), no OAuth QR login (a server has no browser to
//! complete a `localhost` callback). Offering a button for any of those would be
//! worse than omitting it.
//! ## File layout
//!
//! Split from one 1,023-line file. It was a single router plus ~50 handlers for
//! six unrelated resources, so "where does this endpoint go" had no answer other
//! than "somewhere in this file".
//!
//! | file | owns |
//! |---|---|
//! | [`login`] | password login/logout and the form-decoding helpers behind them |
//! | [`observe`] | read-only endpoints: status, stats, request logs, Claude sessions |
//! | [`settings`] | settings, providers/models lists, schedule preferences |
//! | [`identities`] | the credential / api-key pool and per-identity automation |
//! | [`automation`] | batch triggers: check-in all, growth all, growth preview |
//! | [`client_accounts`] | inbound client access-key accounts |
//! | [`pool`] | account-pool import (preview + apply) |
//! | [`error`] | `ApiError`, the one error type every handler returns |
//!
//! `build_admin_router` stays here: the route table is the one place that must
//! see every handler, and spreading it out would mean five files each knowing a
//! slice of the URL space.

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Extension, Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::admin;
use crate::admin_auth::{self, AdminSessions, SessionFailure};
use crate::stats::RequestLogFilter;

// The state moved to its own module so this file and `admin` do not import
// each other. Re-exported here because every existing `AdminState` reference
// names this path.

mod automation;
mod client_accounts;
mod error;
mod identities;
mod login;
mod observe;
mod pool;
mod settings;

pub(crate) use automation::*;
pub(crate) use client_accounts::*;
pub(crate) use error::*;
pub(crate) use identities::*;
pub use login::*;
pub(crate) use observe::*;
pub(crate) use pool::*;
pub(crate) use settings::*;

pub use crate::admin_state::AdminState;

/// Where the admin surface lives, in absolute paths.
pub const ADMIN_PREFIX: &str = "/admin/api";

/// Build the admin router.
///
/// Routes use **absolute** paths (`/admin/api/...`) rather than being built
/// relative and mounted with `Router::nest`. That is deliberate: `nest` reserves
/// its prefix and answers 404 for the bare prefix and its trailing-slash form
/// *before* any nested route, fallback or middleware can run. The console's front
/// door is exactly those two URLs, so nesting made the very first request an
/// operator makes — typing the address — fail.
///
/// Everything except `login` and the console assets sits behind
/// [`admin_auth::require_session`]. `logout` is inside the gate too: a logout
/// without a session has nothing to do, and leaving it outside would mean an
/// unauthenticated caller can make the server do work.
pub fn build_admin_router(state: Arc<AdminState>) -> Router<()> {
    let sessions = state.sessions.clone();

    // NOTE: the console page itself is served by `crate::console`, which ships
    // the *desktop* front-end verbatim over an RPC bridge. This router is the
    // REST surface only.
    let gated = Router::new()
        .route("/admin/api/status", get(status))
        .route("/admin/api/stats", get(stats))
        .route("/admin/api/request-logs", get(request_logs))
        .route("/admin/api/session-metrics", get(session_metrics))
        .route("/admin/api/claude-sessions", get(claude_sessions))
        .route("/admin/api/claude-sessions/:id", get(claude_session))
        .route(
            "/admin/api/claude-sessions/:id/subagents/:agent",
            get(claude_subagent),
        )
        .route("/admin/api/logs", get(logs_get))
        .route("/admin/api/logs/clear", post(logs_clear))
        .route("/admin/api/settings", get(settings_get).post(settings_save))
        .route("/admin/api/providers", get(providers))
        .route("/admin/api/models", get(models))
        // Identity pool
        .route("/admin/api/identities", get(identities))
        .route("/admin/api/identities/default", post(identity_set_default))
        .route("/admin/api/identities/order", post(identity_reorder))
        .route("/admin/api/identities/sticky-reset", post(sticky_reset))
        .route("/admin/api/credentials", post(credential_add))
        .route(
            "/admin/api/credentials/:id",
            delete(credential_delete).patch(credential_patch),
        )
        .route(
            "/admin/api/credentials/:id/relogin",
            post(credential_relogin),
        )
        .route(
            "/admin/api/credentials/:id/checkin",
            post(credential_checkin),
        )
        .route("/admin/api/credentials/:id/growth", post(credential_growth))
        .route("/admin/api/keys", post(key_add))
        .route("/admin/api/keys/:id", delete(key_delete).patch(key_patch))
        .route("/admin/api/preferences", get(preferences))
        .route("/admin/api/preferences/checkin", post(preferences_checkin))
        .route("/admin/api/checkin", post(checkin_all))
        .route("/admin/api/growth", post(growth_all))
        .route("/admin/api/growth/preview", get(growth_preview))
        // Inbound access accounts
        .route(
            "/admin/api/client-accounts",
            get(client_accounts_get).post(client_accounts_create),
        )
        .route(
            "/admin/api/client-accounts/:id",
            delete(client_accounts_delete).patch(client_accounts_patch),
        )
        // Re-issue: the only way an account created before the plaintext was
        // kept can be used by "write this gateway into my client config".
        .route(
            "/admin/api/client-accounts/:id/reissue",
            post(client_accounts_reissue),
        )
        // Pool import/export
        // `/pool/export` is served by `crate::console` (a browser download),
        // not here: the REST handler writes a file at a server path, which the
        // console path deliberately does not need.
        .route("/admin/api/pool/import/preview", post(pool_import_preview))
        .route("/admin/api/pool/import", post(pool_import))
        // Scheduler snapshot: the console renders the three loops on its
        // overview page, and a server operator has no other way to see them.
        .route("/admin/api/scheduler", get(scheduler_snapshot))
        .route_layer(axum::middleware::from_fn_with_state(
            sessions.clone(),
            admin_auth::require_session,
        ));

    Router::new()
        .merge(gated)
        .layer(Extension(state))
        .with_state(sessions)
}

// ── Session ────────────────────────────────────────────────────────────────
