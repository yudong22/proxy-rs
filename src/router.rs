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
pub fn build_app_router(
    service_ctrl: Arc<ServiceController>,
    logs: Arc<LogBuffer>,
    config: Arc<Config>,
    client: Client,
    stats: Arc<StatsDb>,
    metrics_handle: metrics::PrometheusHandle,
    pool: crate::session_pool::SharedCredentialPool,
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
        .layer(TraceLayer::new_for_http())
        .layer(cors)
}
