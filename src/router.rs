//! Shared axum router construction for the proxy API.
//!
//! Every route the listener serves — the proxied `/v1/*` protocols, the gateway
//! balance (`GET /v1/credits`), `/metrics` and the read-only dashboard
//! observation endpoints under `/api/*` — is registered here, in one place, so a
//! new endpoint cannot exist in one front-end and not the other. The Tauri GUI
//! (`src-tauri/src/main.rs`) is the only current caller; the function is kept
//! free of GUI types so a headless front-end can reuse it unchanged.
//!
//! The `/api/*` routes are deliberately in this router rather than a second
//! listener: they answer questions about the same `StatsDb` the proxy writes to,
//! and a dashboard that only worked while a second port was open would be worse
//! than the Tauri commands it replaces.

use crate::api;
use crate::credits;
use crate::metrics;
use crate::proxy;
use crate::service::ServiceController;
use crate::settings::LogBuffer;
use crate::stats::StatsDb;
use crate::util;
use crate::Config;
use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
    Extension, Router,
};
use reqwest::Client;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

/// Build the proxy API router with all extensions already attached.
///
/// `metrics_handle` is the handle returned by [`metrics::install`]; the
/// `/metrics` route clones it per request.
#[allow(clippy::too_many_arguments)]
pub fn build_app_router(
    service_ctrl: Arc<ServiceController>,
    logs: Arc<LogBuffer>,
    config: Arc<Config>,
    client: Client,
    stats: Arc<StatsDb>,
    metrics_handle: metrics::PrometheusHandle,
    pool: crate::session_pool::SharedCredentialPool,
    client_auth: Arc<crate::client_accounts::ClientAuthState>,
) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Claude Code's request bodies grow past 2 MB on a long conversation, which
    // is axum's default cap for `Bytes`/`String`/`Json`. The handlers buffer the
    // body themselves with the same figure (see `util::max_body_bytes`), so the
    // two must agree: this layer is what bounds the *extractor*, and the handler
    // is what bounds the peek and reports a 413 with the reason.
    let max_body = util::max_body_bytes();

    Router::new()
        .route("/v1/messages", post(proxy::proxy_handler))
        .route("/v1/responses", post(proxy::responses_proxy_handler))
        .route("/responses", post(proxy::responses_proxy_handler))
        .route(
            "/backend-api/codex/responses",
            post(proxy::responses_proxy_handler),
        )
        .route(
            "/v1/chat/completions",
            post(proxy::chat_completions_proxy_handler),
        )
        .route(
            "/chat/completions",
            post(proxy::chat_completions_proxy_handler),
        )
        .route("/v1/models", get(proxy::list_models_handler))
        .route("/models", get(proxy::list_models_handler))
        .route("/v1/credits", get(credits::credits_handler))
        .route("/credits", get(credits::credits_handler))
        // Read-only dashboard observation. Not under `/v1` — see `api`'s module
        // docs for why the client-facing protocol namespace must stay clean.
        .route("/api/stats/today", get(api::stats_today_handler))
        .route("/api/request-logs", get(api::request_logs_handler))
        // The three background loops' own status, plus the single mutating route
        // under `/api`. Both are purely additive: no `/v1/*` route changes, so a
        // client pointed at the proxy cannot observe that they exist. The POST is
        // token-gated and origin-checked inside the handler — see `api_token`.
        .route("/api/scheduler", get(api::scheduler_handler))
        // Client-access-account admin. Token-gated inside each handler (same
        // `PROXY_API_TOKEN` as the scheduler trigger below), which is why the
        // inbound gate above leaves all `/api/*` open: these routes carry their
        // own, stronger check.
        .route(
            "/api/admin/client-accounts",
            get(api::client_accounts_list_handler).post(api::client_accounts_create_handler),
        )
        .route(
            // Axum 0.7 uses `:param`, not `{param}` — see the note on the
            // scheduler route below.
            "/api/admin/client-accounts/:id",
            axum::routing::patch(api::client_accounts_update_handler)
                .delete(api::client_accounts_delete_handler),
        )
        .route(
            // Axum 0.7 path-parameter syntax is `:name` — braces became the
            // syntax only in 0.8. With braces this route registrations still
            // succeed but never match a real path, so every trigger 404s while
            // the snapshot route works, which is a maddening half-working
            // surface. `tests/api_scheduler.rs` pins the behaviour.
            "/api/scheduler/run/:loop_id",
            post(api::scheduler_run_handler),
        )
        .route("/health", get(|| async { "OK" }))
        .route(
            "/metrics",
            get(move || {
                let handle = metrics_handle.clone();
                async move { handle.render() }
            }),
        )
        .layer(DefaultBodyLimit::max(max_body))
        .layer(Extension(service_ctrl))
        .layer(Extension(logs))
        .layer(Extension(config))
        .layer(Extension(client))
        .layer(Extension(stats))
        .layer(Extension(pool))
        .layer(Extension(client_auth.clone()))
        // Inbound access-key gate. `.route_layer` keeps it off the fallback and
        // out of the way of 404s; `TraceLayer`/`cors` sit *outside* it so a 401
        // still carries CORS headers and reaches the browser as a readable
        // error rather than an opaque network failure.
        .route_layer(middleware::from_fn_with_state(
            client_auth,
            crate::client_auth::guard,
        ))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
}

/// Attach the admin surface **outside** the data plane's CORS layer.
///
/// `build_app_router` answers `Access-Control-Allow-Origin: *`, deliberately: the
/// desktop GUI reads `/api/*` from the webview, and [`crate::api`]'s module docs
/// describe that as an accepted trade-off for a loopback developer tool.
///
/// The admin console is a different trust domain and must not inherit it. It is
/// same-origin by construction (the page is served by this same listener), so it
/// needs no cross-origin permission at all — and emitting `*` on responses that
/// carry a session cookie is simply wrong: any page a user visits could read the
/// reply to a request the browser made with that cookie (the cookie itself stays
/// unreadable thanks to `SameSite=Strict`, which is why this is a correctness bug
/// rather than an open door).
///
/// Merging here, after `.layer(cors)`, keeps the two policies independent: a
/// future change to the data-plane CORS story cannot silently widen the admin one.
pub fn with_admin(base: Router, admin: Option<Router>) -> Router {
    match admin {
        // Merged, not nested: the admin router registers its own absolute
        // `/admin/api/*` paths because `nest` reserves the prefix and answers 404
        // for the bare prefix before any nested route or fallback runs — and the
        // bare prefix is the console's front door.
        Some(router) => base.merge(router),
        None => base,
    }
}
