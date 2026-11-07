//! Serve the desktop console in the browser.
//!
//! ## Why the same front-end, not a second one
//!
//! The first cut of server mode shipped a purpose-built admin page. It was a
//! mistake, and the reason is not aesthetic: it duplicated every view, drifted
//! from the desktop console within one release, and meant a fix made in one place
//! silently did not reach the other. Two front-ends over one backend is two
//! products to keep in sync, and nobody asked for the second.
//!
//! So the gateway serves **the exact bytes the Tauri window loads** — the same
//! `ui/index.html`, the same modules, the same stylesheets — and the only piece
//! that differs is the transport underneath. [`ipc.js`] already funnels every
//! call through one `invoke()`; the bridge below fills that function with HTTP
//! instead of Tauri IPC. Every view keeps working unmodified, including its
//! layout, its copy, its empty states and its keyboard shortcuts.
//!
//! ## What the bridge has to translate
//!
//! Tauri commands take named arguments (`invoke('wb_credentials_toggle', { id,
//! enabled })`) and return a JS value or throw a string. The HTTP admin API is
//! REST-shaped. Rather than rewrite 54 call sites, the bridge keeps the command
//! names as the wire vocabulary: `POST /admin/api/rpc/<command>` with the
//! argument object as the body. One dispatch table in [`crate::admin`] maps each
//! command to its implementation, so the command surface stays exactly the one
//! the UI already speaks.
//!
//! ## What cannot work in a browser, and how it fails
//!
//! Three desktop capabilities have no server meaning, and the UI must not
//! pretend otherwise:
//!
//! * **start/stop the service** — the process lifecycle belongs to systemd. The
//!   status payload reports `managed_by: "systemd"`, and the bridge answers
//!   `start_service` / `stop_service` with a clear error instead of a silent
//!   success that leaves the button lying.
//! * **open the log directory / reveal a path** — there is no file manager. The
//!   bridge returns a no-op success, because the only thing the UI does with it
//!   is dismiss a toast; failing would be noise.
//! * **native "save as" panel** — `pool_export` becomes an HTTP download that the
//!   browser writes, so the server never needs permission to write outside its
//!   own data directory.
//!
//! Each of those is handled where it belongs and commented, rather than being
//! papered over in the UI.

use axum::{
    extract::{Path, State},
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::post,
    Extension, Json, Router,
};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::admin_auth;

/// Files served to the browser, compiled in.
///
/// `include_str!` per file rather than a directory walk at runtime: the
/// deployment is one executable plus a data directory, and a console that 404s
/// because nobody copied `ui/` next to the binary is a footgun with no upside.
/// ~360 KB of text, compressed to a few tens of KB by the reverse proxy.
const ASSETS: &[(&str, &str, &str)] = &[
    (
        "/index.html",
        "text/html; charset=utf-8",
        include_str!("../ui/index.html"),
    ),
    (
        "/src/main.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/main.js"),
    ),
    (
        "/src/core/api.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/core/api.js"),
    ),
    (
        "/src/core/dom.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/core/dom.js"),
    ),
    (
        "/src/core/ipc.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/core/ipc.js"),
    ),
    (
        "/src/core/state.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/core/state.js"),
    ),
    (
        "/src/lib/format.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/lib/format.js"),
    ),
    (
        "/src/components/confirm.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/components/confirm.js"),
    ),
    (
        "/src/components/modal.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/components/modal.js"),
    ),
    (
        "/src/components/overlay.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/components/overlay.js"),
    ),
    (
        "/src/components/pills.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/components/pills.js"),
    ),
    (
        "/src/components/request-modal.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/components/request-modal.js"),
    ),
    (
        "/src/components/table.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/components/table.js"),
    ),
    (
        "/src/components/toast.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/components/toast.js"),
    ),
    (
        "/src/views/tabs.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/tabs.js"),
    ),
    (
        "/src/views/overview.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/overview.js"),
    ),
    (
        "/src/views/logs.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/logs.js"),
    ),
    (
        "/src/views/sessions/index.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/sessions/index.js"),
    ),
    (
        "/src/views/sessions/list.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/sessions/list.js"),
    ),
    (
        "/src/views/sessions/timeline.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/sessions/timeline.js"),
    ),
    (
        "/src/views/sessions/entry.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/sessions/entry.js"),
    ),
    (
        "/src/views/palette.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/palette.js"),
    ),
    (
        "/src/views/settings/index.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/settings/index.js"),
    ),
    (
        "/src/views/settings/form.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/settings/form.js"),
    ),
    (
        "/src/views/settings/fields.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/settings/fields.js"),
    ),
    (
        "/src/views/settings/load.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/settings/load.js"),
    ),
    (
        "/src/views/settings/save.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/settings/save.js"),
    ),
    (
        "/src/views/settings/badges.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/settings/badges.js"),
    ),
    (
        "/src/views/settings/checkin.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/settings/checkin.js"),
    ),
    (
        "/src/views/settings/client-accounts.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/settings/client-accounts.js"),
    ),
    (
        "/src/views/settings/credential-pool.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/settings/credential-pool.js"),
    ),
    (
        "/src/views/settings/dsh.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/settings/dsh.js"),
    ),
    (
        "/src/views/settings/oauth-qr.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/settings/oauth-qr.js"),
    ),
    (
        "/src/views/settings/pool-transfer.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/settings/pool-transfer.js"),
    ),
    (
        "/src/views/settings/relogin.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/src/views/settings/relogin.js"),
    ),
    (
        "/style/tokens.css",
        "text/css; charset=utf-8",
        include_str!("../ui/style/tokens.css"),
    ),
    (
        "/style/base.css",
        "text/css; charset=utf-8",
        include_str!("../ui/style/base.css"),
    ),
    (
        "/style/controls.css",
        "text/css; charset=utf-8",
        include_str!("../ui/style/controls.css"),
    ),
    (
        "/style/badges.css",
        "text/css; charset=utf-8",
        include_str!("../ui/style/badges.css"),
    ),
    (
        "/style/overlays.css",
        "text/css; charset=utf-8",
        include_str!("../ui/style/overlays.css"),
    ),
    (
        "/style/layout.css",
        "text/css; charset=utf-8",
        include_str!("../ui/style/layout.css"),
    ),
    (
        "/style/cards.css",
        "text/css; charset=utf-8",
        include_str!("../ui/style/cards.css"),
    ),
    (
        "/style/logs.css",
        "text/css; charset=utf-8",
        include_str!("../ui/style/logs.css"),
    ),
    (
        "/style/views.css",
        "text/css; charset=utf-8",
        include_str!("../ui/style/views.css"),
    ),
    (
        "/style/workbuddy.css",
        "text/css; charset=utf-8",
        include_str!("../ui/style/workbuddy.css"),
    ),
    (
        "/assets/logo.svg",
        "image/svg+xml",
        include_str!("../ui/assets/logo.svg"),
    ),
    (
        "/src/bridge.js",
        "text/javascript; charset=utf-8",
        include_str!("../ui/server/bridge.js"),
    ),
];

/// Where the console lives in the URL space.
pub const CONSOLE_PREFIX: &str = "/admin";

/// Build the console router.
///
/// Public routes (no session): the page and its assets, plus the login endpoint.
/// Everything under `/admin/api/rpc/*` requires a session, same as the REST
/// surface in [`crate::admin_routes`].
pub fn build_console_router(state: Arc<crate::admin_routes::AdminState>) -> Router<()> {
    let sessions = state.sessions.clone();

    let assets = Router::new()
        .route(CONSOLE_PREFIX, axum::routing::get(serve_page))
        .route("/admin/", axum::routing::get(serve_page))
        // The login page is a *separate* document, not a modal in the desktop UI:
        // that UI has no notion of authentication, and adding one would mean
        // changing the file both front-ends share. A small standalone page keeps
        // `ui/` untouched and still gives the operator a way in.
        // GET shows the form; POST is where a *native* form submission lands.
        //
        // This route exists because the login page posts for real (password
        // managers ignore `fetch` logins). Pointing the form at
        // `/admin/api/login` would also work but puts the API path in the
        // browser's URL bar and history, so the page-owned path is used instead.
        .route(
            "/admin/login",
            axum::routing::get(serve_login).post(crate::admin_routes::login_handler),
        )
        .route("/admin/login.js", axum::routing::get(serve_login_js))
        // One wildcard per asset root. A single `/*path` would have to guess
        // which root a name came from, and guessing wrong turns a stylesheet
        // into a 404 with a misleading message.
        .route("/admin/src/*path", axum::routing::get(serve_src))
        .route("/admin/style/*path", axum::routing::get(serve_style))
        .route("/admin/assets/*path", axum::routing::get(serve_assets));

    let gated = Router::new()
        .route("/admin/api/rpc/:command", post(rpc))
        .route("/admin/api/pool/export", axum::routing::get(pool_export))
        .route_layer(axum::middleware::from_fn_with_state(
            sessions.clone(),
            admin_auth::require_session,
        ));

    Router::new()
        // Login/logout live here as well as on the REST router: an operator
        // bookmarks `/admin/`, and the form it serves must be able to mint a
        // session without a second prefix in the URL. Both mount points share the
        // same `AdminSessions`, so the cookie works on either.
        .route("/admin/api/login", post(crate::admin_routes::login_handler))
        .route(
            "/admin/api/logout",
            post(crate::admin_routes::logout_handler),
        )
        .route("/admin/api/session", axum::routing::get(session_info))
        // Desktop-only session bootstrap. Exists as a route on both builds (so
        // the console page can probe it without a 404-vs-timeout distinction in
        // the JS), but the handler answers 404 unless this state is a desktop
        // build AND the request came from loopback. See [`desktop_boot`].
        .route("/admin/api/desktop-boot", axum::routing::get(desktop_boot))
        .merge(assets)
        .merge(gated)
        .fallback(console_fallback)
        .layer(Extension(state))
        .with_state(sessions)
}

/// Mint a session for the desktop app's own webview, without a token exchange.
///
/// ## Why this exists
///
/// The desktop webview loads the same `/admin/` console the server serves, but
/// there is no human to type `PROXY_API_TOKEN` into: the desktop process *is*
/// the operator. `AdminSessions::mint_session` creates the session in-process;
/// this endpoint hands the cookie to the webview the same way
/// `/admin/api/login` does for a server operator.
///
/// ## Why it is safe
///
/// Two independent gates, and both must pass:
///
/// 1. **Build gate**: `AdminState::desktop` is `true` only in the desktop
///    binary. On a server the endpoint answers **404** — it does not exist for
///    a remote operator, so there is nothing to probe, rate-limit or abuse.
/// 2. **Transport gate**: only a loopback peer is answered. The desktop binds
///    and talks to itself; a desktop listening on a LAN address still cannot
///    have its console logged in from another machine, because the minting
///    request must originate from the same host.
///
/// The session is identical in kind to a token-login session (same map, same
/// TTL, same sliding expiry), so `require_session` cannot tell them apart —
/// which is the point: one authorisation path, two transports.
///
/// Idempotent by design: the page may call this on every (re)connect, and each
/// call simply mints a fresh session; the previous one expires on its own.
async fn desktop_boot(
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    axum::extract::Extension(state): axum::extract::Extension<Arc<crate::admin_routes::AdminState>>,
) -> Response {
    use axum::http::{header, StatusCode};

    // Build gate first, so a server build does not even hint that the route
    // has special behaviour. Same JSON 404 shape the console fallback answers,
    // so a probe cannot distinguish "absent" from "not for you".
    if !state.desktop {
        return not_found_response();
    }
    // Transport gate: the request must come from the same machine. This also
    // covers a desktop bound to 0.0.0.0 (an odd but possible configuration):
    // remote peers are refused even though the port is reachable.
    if !peer.ip().is_loopback() {
        return not_found_response();
    }
    let secure = false; // loopback plain HTTP; Secure would make the webview drop the cookie
    let id = state.sessions.mint_session();
    let ttl = admin_auth::SESSION_TTL;
    let mut response = (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "ok": true,
            "expires_in_secs": ttl.as_secs(),
        })),
    )
        .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        admin_auth::set_cookie_header(&id, secure, ttl),
    );
    response
}

/// The console's JSON 404, usable from a non-async helper context.
fn not_found_response() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": {
                "type": "not_found",
                "message": "未知的管理接口",
                "code": "not_found",
            }
        })),
    )
        .into_response()
}

/// Report the session and **re-issue the cookie** with a fresh `Max-Age`.
///
/// Re-issuing matters, not just reporting: the session's expiry slides on the
/// server with every request, but the browser's cookie has a fixed `Max-Age`
/// from the moment it was set. Without re-issuing, a console left open for a day
/// would have its cookie dropped by the browser mid-use — the server still
/// holding a valid session — and the operator would be logged out for no reason
/// they could see. The bridge calls this hourly to keep the two in step.
///
/// Returns 401 when there is no live session, so the caller can route to the
/// login page.
pub async fn session_info(
    State(sessions): State<Arc<crate::admin_auth::AdminSessions>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(id) = crate::admin_auth::session_cookie(&headers) else {
        return crate::admin_auth::failure_response(crate::admin_auth::SessionFailure::NoSession);
    };
    if !sessions.touch(&id) {
        return crate::admin_auth::failure_response(crate::admin_auth::SessionFailure::NoSession);
    }
    let ttl = sessions
        .ttl_of(&id)
        .unwrap_or(crate::admin_auth::SESSION_TTL);
    let trust_forwarded =
        crate::admin_auth::trusted_proxy(&crate::settings::GuiSettings::load().bind);
    let secure = crate::admin_auth::is_secure_request(&headers, trust_forwarded);
    let mut response = Json(json!({
        "authenticated": true,
        "ok": true,
        "expires_in_secs": ttl.as_secs(),
    }))
    .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        crate::admin_auth::set_cookie_header(&id, secure, ttl),
    );
    response
}

/// Dispatch one Tauri command name to its server-side implementation.
///
/// The body is the argument object the UI passed to `invoke`. The reply is
/// `{"result": …}` so a legitimate `null` stays distinguishable from an error.
async fn rpc(
    Extension(state): Extension<Arc<crate::admin_routes::AdminState>>,
    Path(command): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let args = body.map(|Json(v)| v).unwrap_or(Value::Null);
    match crate::admin_rpc::dispatch(&state, &command, &args).await {
        Ok(result) => Json(json!({ "result": result })).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "type": "command_failed",
                    "message": e.to_string(),
                    "code": "command_failed",
                }
            })),
        )
            .into_response(),
    }
}

/// Download the account pool.
///
/// The desktop flow opens a native "save as" panel; the browser downloads the
/// same bytes instead. That is *less* privileged, not more: the server never
/// learns a destination path, so it needs no ability to write outside its data
/// directory.
async fn pool_export(_state: Extension<Arc<crate::admin_routes::AdminState>>) -> Response {
    match crate::admin::export_pool(None) {
        Ok(text) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/json".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!(
                        "attachment; filename=\"proxy-rs-pool-{}.json\"",
                        crate::util::unix_millis() / 1000
                    ),
                ),
                // A pool export carries live tokens: nothing may cache it.
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
            text,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": { "type": "export_failed", "message": e.to_string() } })),
        )
            .into_response(),
    }
}

/// The login form. Self-contained: no stylesheet or script from `ui/`, because
/// those are built for the console's authenticated layout and would fight a
/// single centred card.
fn login_page() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        LOGIN_HTML,
    )
        .into_response()
}

async fn serve_login() -> Response {
    login_page()
}

async fn serve_login_js() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        LOGIN_JS,
    )
        .into_response()
}

/// Serve the login page or the console, depending on whether the session is
/// **valid**.
///
/// Validation happens here rather than only in the RPC middleware because this
/// page is what a stale cookie would otherwise land on. That was a real bug:
/// checking only that the cookie *existed* served the console to expired
/// sessions, every RPC on it answered 401, the bridge navigated to the login
/// page, and the stale cookie was still there — an infinite reload loop. See the
/// body for the fix.
async fn serve_page(
    State(sessions): State<Arc<crate::admin_auth::AdminSessions>>,
    headers: axum::http::HeaderMap,
) -> Response {
    // **Validate**, not merely detect. Checking only that a cookie *exists* was
    // an infinite-reload bug in production: a stale cookie (expired after 12h
    // idle, or invalidated by `systemctl restart`) got the console page, every
    // RPC on it answered 401, the bridge reloaded, and the stale cookie was
    // still there — so the same page came back, forever. The user sees the
    // console "不断重启".
    //
    // `touch` also slides the expiry, which is right: loading the page is the
    // user being active.
    match crate::admin_auth::session_cookie(&headers) {
        Some(id) if sessions.touch(&id) => console_page(),
        _ => login_page(),
    }
}

/// The console page, with the server bridge injected.
///
/// The bridge cannot live in `ui/index.html` itself: the Tauri shell loads that
/// same file and must keep its real IPC, so a `<script>` tag there would either
/// shadow the real bridge in the desktop app or force the tag to be conditional
/// on something the file cannot see. Injecting here keeps `ui/` byte-identical to
/// what the desktop app ships — which is the entire point of serving the same
/// front-end — and puts the one mode-specific tag in the one place that knows
/// which mode it is in.
fn console_page() -> Response {
    let index = ASSETS
        .iter()
        .find(|(path, _, _)| *path == "/index.html")
        .map(|(_, _, body)| *body)
        .unwrap_or_default();

    // Inserted immediately before `main.js`, and as a *classic* script: modules
    // are deferred until after parsing, but a classic script still executes
    // before the module graph runs, so `window.__TAURI__` exists by the time any
    // view reads it.
    let bridge_tag = "<script src=\"src/bridge.js\"></script>\n  ";
    let html = index.replace(
        "<script type=\"module\" src=\"src/main.js\"></script>",
        &format!("{bridge_tag}<script type=\"module\" src=\"src/main.js\"></script>"),
    );

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        html,
    )
        .into_response()
}

/// Scripts: `/admin/src/<rest>` → `/src/<rest>`.
async fn serve_src(Path(rest): Path<String>, headers: axum::http::HeaderMap) -> Response {
    static_asset(&format!("/src/{rest}"), &headers)
}

/// Stylesheets: `/admin/style/<rest>` → `/style/<rest>`.
async fn serve_style(Path(rest): Path<String>, headers: axum::http::HeaderMap) -> Response {
    static_asset(&format!("/style/{rest}"), &headers)
}

/// Static files: `/admin/assets/<rest>` → `/assets/<rest>`.
async fn serve_assets(Path(rest): Path<String>, headers: axum::http::HeaderMap) -> Response {
    static_asset(&format!("/assets/{rest}"), &headers)
}

fn static_asset(key: &str, request_headers: &axum::http::HeaderMap) -> Response {
    match ASSETS.iter().find(|(path, _, _)| *path == key) {
        Some((_, content_type, body)) => {
            // An ETag derived from the build version AND the path.
            //
            // This was a real upgrade bug: the response said `no-cache` but
            // carried no validator, so the browser had nothing to revalidate
            // against and simply reused its copy — a reload after upgrading the
            // binary kept running the previous build's modules, which then called
            // endpoints that no longer existed. Including the version means a new
            // binary invalidates every asset at once, with no manual cache
            // busting and no hashed filenames to thread through the imports.
            let etag = format!("\"v{}-{key}\"", env!("CARGO_PKG_VERSION"));
            // Honour a conditional request, so a normal reload costs headers and
            // no body at all.
            if request_headers
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.split(',').any(|c| c.trim() == etag))
                .unwrap_or(false)
            {
                return (
                    StatusCode::NOT_MODIFIED,
                    [
                        (header::CACHE_CONTROL, "no-cache"),
                        (header::ETAG, etag.as_str()),
                    ],
                )
                    .into_response();
            }
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, *content_type),
                    (header::CACHE_CONTROL, "no-cache"),
                    (header::ETAG, etag.as_str()),
                ],
                *body,
            )
                .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!("no such console asset: {key}"),
        )
            .into_response(),
    }
}

/// Unknown paths under `/admin` answer JSON, so a mistyped endpoint is an error
/// rather than the HTML shell with a 200.
async fn console_fallback(uri: Uri) -> Response {
    let path = uri.path();
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": {
                "type": "not_found",
                "message": format!("未知的管理接口: {path}"),
                "code": "not_found",
            }
        })),
    )
        .into_response()
}

/// The login document, compiled in.
///
/// Kept as a file rather than an inline string literal so it can be edited and
/// read without scrolling through Rust.
const LOGIN_HTML: &str = include_str!("../ui/server/login.html");
const LOGIN_JS: &str = include_str!("../ui/server/login.js");
