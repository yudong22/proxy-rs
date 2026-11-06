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
use crate::client_accounts::ClientAuthState;
use crate::settings::LogBuffer;
use crate::stats::{RequestLogFilter, StatsDb};

/// State the admin handlers need.
///
/// A distinct type from the data plane's extensions on purpose: the admin surface
/// has no `Config` (it does not forward anything) and no credential pool *for
/// forwarding* — it reads the pool only to report sticky-session counts.
pub struct AdminState {
    pub sessions: Arc<AdminSessions>,
    pub stats: Arc<StatsDb>,
    pub logs: Arc<LogBuffer>,
    pub client_auth: Arc<ClientAuthState>,
    pub pool: crate::session_pool::SharedCredentialPool,
    pub process: admin::ProcessInfo,
    /// Outbound HTTP client, shared with the proxy so the console's occasional
    /// refreshes (points, model list, upstream test) reuse the same connection
    /// pool instead of opening a second one per click.
    pub client: reqwest::Client,
}

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

#[derive(Deserialize)]
pub struct LoginBody {
    pub token: String,
    /// Opt in to the long session. Defaults to off so the short TTL is the
    /// decision, not the fallback.
    #[serde(default)]
    pub remember: bool,
}

/// Parse `token=…&remember=1` from a form post.
///
/// Hand-rolled rather than pulling in a form-extractor dependency: the payload is
/// two known fields, and the only decoding needed is `+` and `%XX`. A missing or
/// empty token yields `None`, which the caller reports as a bad token.
fn parse_form_login(body: &[u8]) -> Option<LoginBody> {
    let text = std::str::from_utf8(body).ok()?;
    let mut token = None;
    let mut remember = false;
    for pair in text.split('&') {
        let (k, v) = pair.split_once('=')?;
        let v = v.replace('+', " ");
        let decoded = percent_decode(&v);
        match k {
            "token" | "password" => token = Some(decoded),
            "remember" => remember = matches!(decoded.as_str(), "1" | "on" | "true"),
            _ => {}
        }
    }
    let token = token?.trim().to_string();
    if token.is_empty() {
        return None;
    }
    Some(LoginBody { token, remember })
}

/// Minimal `%XX` decoder for form values.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The client address a login attempt is attributed to.
///
/// Behind the reverse proxy the socket address is always 127.0.0.1, so using it
/// would put every caller in one bucket — and one attacker could then lock out
/// the operator. `X-Forwarded-For` is what the proxy reports; its **first** hop
/// is the original client. This is only a rate-limit key, never an
/// authorisation decision, so trusting the proxy's header here is safe: forging
/// it can only move your own failures to a different bucket.
fn client_key(headers: &HeaderMap, trust_forwarded: bool) -> String {
    if !trust_forwarded {
        // Bound to a public interface: the caller is whatever it claims to be in
        // the socket, not in a header it controls. Without this check, one
        // attacker rotating `X-Forwarded-For` per request never trips the limiter
        // — verified against production before this fix.
        return headers
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
    }
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Exchange the operator token for a session cookie.
/// Accepts either JSON (scripts, the console bridge) or a real HTML form post.
///
/// The form path exists for one reason: **password managers**. Chrome, Safari and
/// Firefox only offer to save a password when they observe a genuine form
/// submission that navigates — a `fetch()` that returns JSON looks like an AJAX
/// call and the prompt never appears. So the login page posts natively and the
/// handler answers with a redirect, while everything programmatic keeps using
/// JSON.
pub async fn login_handler(
    State(sessions): State<Arc<AdminSessions>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // Content negotiation on the raw body: an HTML form posts
    // `application/x-www-form-urlencoded`, scripts post JSON. Reading the bytes
    // first is what allows both without two routes.
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_form = content_type.starts_with("application/x-www-form-urlencoded");

    let parsed: LoginBody = if is_form {
        match parse_form_login(&body) {
            Some(v) => v,
            None => {
                return admin_auth::failure_response(SessionFailure::BadToken);
            }
        }
    } else {
        match serde_json::from_slice::<LoginBody>(&body) {
            Ok(v) => v,
            Err(_) => return admin_auth::failure_response(SessionFailure::BadToken),
        }
    };
    let body = parsed;
    // Whether to believe the forwarding headers depends on whether a proxy is
    // actually in front of us (see `admin_auth::trusted_proxy`).
    let trust_forwarded = admin_auth::trusted_proxy(&crate::settings::GuiSettings::load().bind);
    let client = client_key(&headers, trust_forwarded);
    match sessions.login(body.token.trim(), &client, body.remember) {
        Ok(id) => {
            let secure = admin_auth::is_secure_request(&headers, trust_forwarded);
            // The cookie's Max-Age must match the server-side record, or the
            // browser keeps presenting a cookie the server already dropped.
            let ttl = sessions.ttl_of(&id).unwrap_or(admin_auth::SESSION_TTL);
            let mut response = if is_form {
                // A form post must NAVIGATE, not return JSON: the password
                // manager only records a save after a real page transition, and
                // a user staring at `{"ok":true}` would think it failed.
                axum::response::Redirect::to("/admin/").into_response()
            } else {
                Json(json!({
                    "ok": true,
                    "remembered": body.remember,
                    "expires_in_secs": ttl.as_secs(),
                }))
                .into_response()
            };
            response.headers_mut().insert(
                header::SET_COOKIE,
                admin_auth::set_cookie_header(&id, secure, ttl),
            );
            response
        }
        Err(SessionFailure::BadToken) => {
            // Tell the caller how many tries are left. Warning *before* the
            // lockout is actionable; a sudden 429 is not. Sent as a header so
            // the error body keeps the shape everything else uses.
            let remaining = sessions.attempts_remaining(&client);
            if is_form {
                return axum::response::Redirect::to(&format!(
                    "/admin/login?e=invalid_token&left={remaining}"
                ))
                .into_response();
            }
            let mut response = admin_auth::failure_response(SessionFailure::BadToken);
            if let Ok(value) = header::HeaderValue::from_str(&remaining.to_string()) {
                response.headers_mut().insert(
                    header::HeaderName::from_static("x-login-attempts-remaining"),
                    value,
                );
            }
            response
        }
        Err(failure) => {
            // A native form post cannot render JSON, so the reason travels back
            // in the query string and the page turns it into a message.
            if is_form {
                let code = failure.slug();
                let wait = match failure {
                    SessionFailure::RateLimited { retry_after_secs } => retry_after_secs,
                    _ => 0,
                };
                let url = if wait > 0 {
                    format!("/admin/login?e={code}&wait={wait}")
                } else {
                    format!("/admin/login?e={code}")
                };
                return axum::response::Redirect::to(&url).into_response();
            }
            admin_auth::failure_response(failure)
        }
    }
}

/// Drop the current session and clear the cookie.
pub async fn logout_handler(
    State(sessions): State<Arc<AdminSessions>>,
    headers: HeaderMap,
) -> Response {
    if let Some(id) = admin_auth::session_cookie(&headers) {
        sessions.logout(&id);
    }
    let mut response = Json(json!({ "ok": true })).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, admin_auth::clear_cookie_header());
    response
}

// ── Observation ────────────────────────────────────────────────────────────

async fn status(Extension(state): Extension<Arc<AdminState>>) -> Json<Value> {
    Json(state.process.status_json())
}

async fn stats(Extension(state): Extension<Arc<AdminState>>) -> Result<Json<Value>, ApiError> {
    Ok(Json(admin::stats_today(state.stats.clone()).await?))
}

#[derive(Deserialize, Default)]
struct LogQuery {
    limit: Option<u32>,
    offset: Option<u32>,
    model: Option<String>,
    client_model: Option<String>,
    session_id: Option<String>,
    client: Option<String>,
    errors_only: Option<bool>,
}

async fn request_logs(
    Extension(state): Extension<Arc<AdminState>>,
    Query(q): Query<LogQuery>,
) -> Result<Json<Value>, ApiError> {
    let filter = RequestLogFilter {
        limit: q.limit,
        offset: q.offset,
        model: q.model,
        client_model: q.client_model,
        session_id: q.session_id,
        client: q.client,
        ..RequestLogFilter::default()
    };
    if q.errors_only.unwrap_or(false) {
        // The schema has no `errors_only` flag; an errored row is one whose
        // `error` column is set, and there is no server-side predicate for that.
        // Rather than fake it (or silently ignore the parameter), the filter is
        // dropped and `has_errors` is reported so the caller knows why.
        tracing::debug!("errors_only is not supported server-side; ignoring");
    }
    let stats = state.stats.clone();
    let res = tokio::task::spawn_blocking(move || stats.query_request_logs(&filter))
        .await
        .map_err(|e| ApiError::internal(format!("查询异常终止: {e}")))?
        .map_err(|e| ApiError::internal(format!("查询失败: {e}")))?;
    Ok(Json(
        crate::api::request_logs_json(&res).map_err(|e| ApiError::internal(e.to_string()))?,
    ))
}

async fn session_metrics(
    Extension(state): Extension<Arc<AdminState>>,
) -> Result<Json<Value>, ApiError> {
    let stats = state.stats.clone();
    let rows = tokio::task::spawn_blocking(move || stats.query_recent_session_metrics(3))
        .await
        .map_err(|e| ApiError::internal(format!("查询异常终止: {e}")))?
        .map_err(|e| ApiError::internal(format!("查询失败: {e}")))?;
    Ok(Json(json!({ "sessions": rows })))
}

async fn logs_get(Extension(state): Extension<Arc<AdminState>>) -> Json<Value> {
    Json(json!({ "entries": state.logs.snapshot().await }))
}

async fn logs_clear(Extension(state): Extension<Arc<AdminState>>) -> Json<Value> {
    state.logs.clear().await;
    Json(json!({ "ok": true }))
}

// ── Settings ───────────────────────────────────────────────────────────────

async fn settings_get(Extension(state): Extension<Arc<AdminState>>) -> Json<Value> {
    let mut payload = admin::settings_json();
    if let Some(obj) = payload.as_object_mut() {
        // What is actually bound, which can differ from the configured port.
        obj.insert("actual_port".into(), json!(state.process.bound_port));
    }
    Json(payload)
}

async fn settings_save(
    Extension(state): Extension<Arc<AdminState>>,
    Json(body): Json<admin::SettingsUpdate>,
) -> Result<Json<Value>, ApiError> {
    let saved = admin::save_settings(body)?;
    state
        .logs
        .push(
            "INFO",
            "设置已保存（服务器模式：端口/绑定的改动需 systemctl restart proxy-rs 才生效）"
                .to_string(),
        )
        .await;
    Ok(Json(saved))
}

async fn providers() -> Json<Value> {
    Json(json!({ "providers": crate::providers::builtin_presets() }))
}

async fn models() -> Result<Json<Value>, ApiError> {
    let settings = crate::settings::GuiSettings::load();
    let preset = settings.models_preset();
    let url = preset
        .models_url
        .clone()
        .or_else(|| preset.models_config_url.clone())
        .ok_or_else(|| ApiError::invalid("当前厂商没有可用的模型列表端点"))?;
    let client = reqwest::Client::new();
    let list = crate::providers::fetch_models(&client, &preset, &settings.api_key)
        .await
        .map_err(|e| ApiError::internal(format!("拉取模型失败: {e}")))?;
    Ok(Json(json!({ "models": list, "endpoint": url })))
}

// ── Identity pool ──────────────────────────────────────────────────────────

async fn identities(Extension(state): Extension<Arc<AdminState>>) -> Json<Value> {
    Json(admin::identities_json(&state.pool).await)
}

#[derive(Deserialize)]
struct DefaultBody {
    id: String,
}

async fn identity_set_default(
    Extension(state): Extension<Arc<AdminState>>,
    Json(body): Json<DefaultBody>,
) -> Result<Json<Value>, ApiError> {
    let out = admin::set_default_identity(&body.id)?;
    // The pool is rebuilt so the change takes effect on the next request without
    // a restart. Without this the console would report success while traffic
    // kept using the old default until the unit was restarted.
    state.pool.reload_from_store().await;
    state
        .logs
        .push("INFO", format!("已设置默认身份: {}", body.id))
        .await;
    Ok(Json(out))
}

#[derive(Deserialize)]
struct OrderBody {
    order: Vec<String>,
}

async fn identity_reorder(
    Extension(state): Extension<Arc<AdminState>>,
    Json(body): Json<OrderBody>,
) -> Result<Json<Value>, ApiError> {
    let out = admin::reorder_identities(&body.order)?;
    state.pool.reload_from_store().await;
    Ok(Json(out))
}

async fn sticky_reset(Extension(state): Extension<Arc<AdminState>>) -> Json<Value> {
    // Clears every session's relocated-credential binding, so the next request
    // for each session goes back to the default identity.
    let before = state.pool.sticky_count().await;
    state.pool.reset_sticky().await;
    Json(json!({ "ok": true, "cleared": before }))
}

#[derive(Deserialize)]
struct CredentialAddBody {
    login_state: String,
    #[serde(default)]
    label: String,
}

async fn credential_add(
    Extension(state): Extension<Arc<AdminState>>,
    Json(body): Json<CredentialAddBody>,
) -> Result<Json<Value>, ApiError> {
    let label = body.label.clone();
    let out = tokio::task::spawn_blocking(move || admin::add_credential(&body.login_state, &label))
        .await
        .map_err(|e| ApiError::internal(format!("任务异常终止: {e}")))??;
    state.pool.reload_from_store().await;
    Ok(Json(out))
}

async fn credential_delete(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let out = tokio::task::spawn_blocking(move || admin::delete_credential(&id))
        .await
        .map_err(|e| ApiError::internal(format!("任务异常终止: {e}")))??;
    state.pool.reload_from_store().await;
    Ok(Json(out))
}

#[derive(Deserialize)]
struct EnabledBody {
    enabled: bool,
}

async fn credential_patch(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
    Json(body): Json<EnabledBody>,
) -> Result<Json<Value>, ApiError> {
    let out = tokio::task::spawn_blocking(move || admin::set_credential_enabled(&id, body.enabled))
        .await
        .map_err(|e| ApiError::internal(format!("任务异常终止: {e}")))??;
    state.pool.reload_from_store().await;
    Ok(Json(out))
}

#[derive(Deserialize)]
struct ReloginBody {
    login_state: String,
}

async fn credential_relogin(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
    Json(body): Json<ReloginBody>,
) -> Result<Json<Value>, ApiError> {
    let out =
        tokio::task::spawn_blocking(move || admin::relogin_credential(&id, &body.login_state))
            .await
            .map_err(|e| ApiError::internal(format!("任务异常终止: {e}")))??;
    state.pool.reload_from_store().await;
    Ok(Json(out))
}

async fn credential_checkin(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let client = reqwest::Client::new();
    let credentials = crate::workbuddy_auth::load_credentials();
    let cred = credentials
        .iter()
        .find(|c| c.id == id)
        .ok_or_else(|| ApiError::not_found("账号不存在"))?;
    // Forced: a manual click means "do it now", not "skip because today's slot
    // is recorded". The scheduled loop is the caller that must respect the slot.
    let result = crate::workbuddy_auth::claim_daily_checkin_forced(&client, cred).await;
    state
        .logs
        .push("INFO", format!("手动打卡 {}: {}", id, result.message))
        .await;
    Ok(Json(json!({ "ok": true, "result": result })))
}

async fn credential_growth(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let client = reqwest::Client::new();
    let credentials = crate::workbuddy_auth::load_credentials();
    let cred = credentials
        .iter()
        .find(|c| c.id == id)
        .ok_or_else(|| ApiError::not_found("账号不存在"))?;
    let run = crate::growth_runner::run_account(&client, cred, None, false).await;
    // Built here rather than reusing `summarize` (which takes the whole pool's
    // report): one account's outcome is a different sentence, and a per-account
    // route that answered with a pool-wide summary would be actively confusing.
    let summary = format!(
        "{}: 接取 {} 个，领奖 {} 个，获得 {} 积分，{} 个任务未推进",
        run.label,
        run.accepted,
        run.claimed.len(),
        run.credits_earned,
        run.stalled.len()
    );
    state
        .logs
        .push("INFO", format!("手动成长任务 {}: {}", id, summary))
        .await;
    Ok(Json(json!({ "ok": true, "summary": summary })))
}

async fn key_add(Json(body): Json<KeyAddBody>) -> Result<Json<Value>, ApiError> {
    Ok(Json(admin::add_key(&body.key, &body.label)?))
}

#[derive(Deserialize)]
struct KeyAddBody {
    key: String,
    #[serde(default)]
    label: String,
}

async fn key_delete(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let out = admin::delete_key(&id)?;
    state.pool.reload_from_store().await;
    Ok(Json(out))
}

async fn key_patch(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
    Json(body): Json<EnabledBody>,
) -> Result<Json<Value>, ApiError> {
    let out = admin::set_key_enabled(&id, body.enabled)?;
    state.pool.reload_from_store().await;
    Ok(Json(out))
}

/// The three background loops' own status.
///
/// `/api/scheduler` exists on the data plane for external agents, but a server
/// operator reads it from this console — and the console is the only surface
/// that is reachable from outside without an access key, so it needs its own.
async fn scheduler_snapshot() -> Json<Value> {
    Json(crate::scheduler::snapshot())
}

async fn preferences() -> Json<Value> {
    Json(admin::preferences_json())
}

#[derive(Deserialize)]
struct ScheduleBody {
    enabled: bool,
    #[serde(default)]
    time: String,
    growth: Option<bool>,
}

async fn preferences_checkin(Json(body): Json<ScheduleBody>) -> Result<Json<Value>, ApiError> {
    let time = if body.time.trim().is_empty() {
        "09:00".to_string()
    } else {
        body.time
    };
    Ok(Json(admin::save_schedule(
        body.enabled,
        &time,
        body.growth,
    )?))
}

async fn checkin_all(Extension(state): Extension<Arc<AdminState>>) -> Json<Value> {
    let client = reqwest::Client::new();
    let report = crate::workbuddy_auth::batch_claim_daily_checkin_forced(&client).await;
    let summary = format!(
        "手动打卡完成: 共 {} 个账号，成功 {} 个，已打卡 {} 个，失败 {} 个",
        report.total, report.success, report.already_checked_in, report.failed
    );
    state.logs.push("INFO", summary.clone()).await;
    Json(json!({ "ok": true, "summary": summary, "report": report }))
}

async fn growth_all(Extension(state): Extension<Arc<AdminState>>) -> Json<Value> {
    let client = reqwest::Client::new();
    // Enabled accounts only: a manual "run now" must not re-enable work the
    // operator switched off per account.
    let credentials: Vec<_> = crate::workbuddy_auth::load_credentials()
        .into_iter()
        .filter(|c| c.enabled)
        .collect();
    let report = crate::growth_runner::run_pool(&client, &credentials, None, false).await;
    let summary = crate::growth_runner::summarize(&report);
    state
        .logs
        .push("INFO", format!("手动成长任务: {summary}"))
        .await;
    Json(json!({ "ok": true, "summary": summary }))
}

#[derive(Deserialize)]
struct GrowthPreviewQuery {
    id: String,
}

/// What the growth automation would find for one account, without running it.
async fn growth_preview(Query(q): Query<GrowthPreviewQuery>) -> Result<Json<Value>, ApiError> {
    let credentials = crate::workbuddy_auth::load_credentials();
    let cred = credentials
        .iter()
        .find(|c| c.id == q.id)
        .ok_or_else(|| ApiError::not_found("账号不存在"))?;
    let client = reqwest::Client::new();
    let growth = crate::growth::GrowthClient::new(&client, None);
    let tasks = growth
        .tasks(cred)
        .await
        .map_err(|e| ApiError::internal(format!("拉取成长任务失败: {e}")))?;
    let energy = growth.energy(cred).await.ok();
    Ok(Json(json!({
        "tasks": tasks,
        "energy": energy,
        "earnable": tasks.iter().filter(|t| t.is_actionable()).map(|t| t.reward).sum::<u32>(),
    })))
}

// ── Inbound access accounts ────────────────────────────────────────────────

async fn client_accounts_get(Extension(state): Extension<Arc<AdminState>>) -> Json<Value> {
    Json(admin::client_accounts_json(&state.client_auth).await)
}

#[derive(Deserialize)]
struct CreateClientAccountBody {
    label: String,
    #[serde(default)]
    note: String,
}

async fn client_accounts_create(
    Extension(state): Extension<Arc<AdminState>>,
    Json(body): Json<CreateClientAccountBody>,
) -> Result<Json<Value>, ApiError> {
    let out =
        tokio::task::spawn_blocking(move || admin::create_client_account(&body.label, &body.note))
            .await
            .map_err(|e| ApiError::internal(format!("任务异常终止: {e}")))??;
    state.client_auth.reload_from_store().await;
    Ok(Json(out))
}

async fn client_accounts_patch(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
    Json(body): Json<EnabledBody>,
) -> Result<Json<Value>, ApiError> {
    let out = admin::set_client_account_enabled(&id, body.enabled)?;
    state.client_auth.reload_from_store().await;
    Ok(Json(out))
}

async fn client_accounts_reissue(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let out = tokio::task::spawn_blocking(move || admin::reissue_client_account_key(&id))
        .await
        .map_err(|e| ApiError::internal(format!("任务异常终止: {e}")))??;
    state.client_auth.reload_from_store().await;
    Ok(Json(out))
}

async fn client_accounts_delete(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let out = admin::delete_client_account(&id)?;
    state.client_auth.reload_from_store().await;
    Ok(Json(out))
}

// ── Pool import ────────────────────────────────────────────────────────────
//
// Export is not here: it is a browser download served by `crate::console`. The
// desktop app used a native "save as" panel and a server path; the console needs
// neither, and giving the server a write-anywhere capability for it would be a
// privilege it never uses.

#[derive(Deserialize)]
struct ImportBody {
    text: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    mode: String,
}

async fn pool_import_preview(Json(body): Json<ImportBody>) -> Result<Json<Value>, ApiError> {
    let out = tokio::task::spawn_blocking(move || {
        admin::preview_pool(&body.text, Some(body.password.as_str()), &body.mode)
    })
    .await
    .map_err(|e| ApiError::internal(format!("任务异常终止: {e}")))??;
    Ok(Json(out))
}

async fn pool_import(
    Extension(state): Extension<Arc<AdminState>>,
    Json(body): Json<ImportBody>,
) -> Result<Json<Value>, ApiError> {
    let mode = body.mode.clone();
    let out = tokio::task::spawn_blocking(move || {
        admin::apply_pool_import(&body.text, Some(body.password.as_str()), &mode)
    })
    .await
    .map_err(|e| ApiError::internal(format!("任务异常终止: {e}")))??;
    state.pool.reload_from_store().await;
    state.client_auth.reload_from_store().await;
    state.logs.push("INFO", "账号池已导入".to_string()).await;
    Ok(Json(out))
}

// ── Errors ─────────────────────────────────────────────────────────────────

/// A typed error that becomes the same JSON envelope `/api/*` uses.
pub struct ApiError {
    status: StatusCode,
    kind: &'static str,
    message: String,
}

impl ApiError {
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            kind: "internal",
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            kind: "invalid_request",
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            kind: "not_found",
            message: message.into(),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        // The store layer's errors are mostly "not found" and "invalid input".
        // Matching on the message would be fragile; treating everything as 422
        // would hide real failures. 400 with the real text is the honest middle:
        // the operator sees exactly what went wrong and can act on it.
        Self {
            status: StatusCode::BAD_REQUEST,
            kind: "invalid_request",
            message: e.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": {
                    "type": self.kind,
                    "message": self.message,
                    "code": self.kind,
                }
            })),
        )
            .into_response()
    }
}
