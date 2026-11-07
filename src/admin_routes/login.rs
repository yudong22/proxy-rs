//! Password login/logout and the form-decoding helpers behind them.

use super::*;

#[derive(Deserialize)]
pub struct LoginBody {
    pub token: String,
    /// Opt in to the long session. Defaults to off so the short TTL is the
    /// decision, not the fallback.
    #[serde(default)]
    pub remember: bool,
    /// Form-only: the tab to land on. JSON clients omit it.
    #[serde(default)]
    pub next: Option<String>,
}

/// Parse `token=…&remember=1` from a form post.
///
/// Hand-rolled rather than pulling in a form-extractor dependency: the payload is
/// two known fields, and the only decoding needed is `+` and `%XX`. A missing or
/// empty token yields `None`, which the caller reports as a bad token.
pub(crate) fn parse_form_login(body: &[u8]) -> Option<LoginBody> {
    let text = std::str::from_utf8(body).ok()?;
    let mut token = None;
    let mut remember = false;
    // The tab to land on after login (`#sessions`, …). Validated later in
    // `landing_path`; the raw value is never trusted as a path.
    let mut next = None;
    for pair in text.split('&') {
        let (k, v) = pair.split_once('=')?;
        let v = v.replace('+', " ");
        let decoded = percent_decode(&v);
        match k {
            "token" | "password" => token = Some(decoded),
            "remember" => remember = matches!(decoded.as_str(), "1" | "on" | "true"),
            "next" => next = Some(decoded),
            _ => {}
        }
    }
    let token = token?.trim().to_string();
    if token.is_empty() {
        return None;
    }
    Some(LoginBody {
        token,
        remember,
        next,
    })
}

/// Where a successful form login sends the browser.
///
/// `next` may only name a tab (`overview` / `logs` / `sessions` / `settings`).
/// Anything else — an absolute URL, `//evil.example`, a path — is ignored,
/// because a redirect target taken from a form field is an open redirect. The
/// hash is re-attached so the console opens the tab the operator was looking at
/// when the session expired.
pub(crate) fn landing_path(next: Option<&str>) -> String {
    const TABS: [&str; 4] = ["overview", "logs", "sessions", "settings"];
    let tab = next
        .map(|n| n.trim().trim_start_matches('#'))
        .filter(|n| TABS.contains(n));
    match tab {
        Some(tab) => format!("/admin/#{tab}"),
        None => "/admin/".to_string(),
    }
}

/// `&next=<tab>` for a failed form login, or empty.
///
/// A rejected login must stay on the tab it came from: the operator retries the
/// password, not the navigation. Same allow-list as [`landing_path`].
pub(crate) fn next_tab_query(next: Option<&str>) -> String {
    const TABS: [&str; 4] = ["overview", "logs", "sessions", "settings"];
    next.map(|n| n.trim().trim_start_matches('#'))
        .filter(|n| TABS.contains(n))
        .map(|n| format!("&next={n}"))
        .unwrap_or_default()
}

/// Minimal `%XX` decoder for form values.
pub(crate) fn percent_decode(value: &str) -> String {
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
pub(crate) fn client_key(headers: &HeaderMap, trust_forwarded: bool) -> String {
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
    // Only the form path carries `next` (a JSON client knows its own landing).
    let next = body.next.clone();
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
                axum::response::Redirect::to(&landing_path(next.as_deref())).into_response()
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
                    "/admin/login?e=invalid_token&left={remaining}{}",
                    next_tab_query(next.as_deref())
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
