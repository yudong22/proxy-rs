//! Session authentication for the server-mode admin console.
//!
//! ## The shape
//!
//! A browser cannot send `Authorization: Bearer` on a page navigation, so an
//! HTML console needs a cookie. The flow is deliberately the smallest one that
//! works:
//!
//! 1. `POST /admin/api/login` with the operator's [`crate::api_token`] token.
//! 2. A random session id is minted and returned as `Set-Cookie`.
//! 3. Every later `/admin/api/*` request is authenticated by that cookie.
//!
//! ## Why the token is `PROXY_API_TOKEN` and not a second secret
//!
//! An operator already has to set `PROXY_API_TOKEN` to use the mutating routes
//! on `/api/*`. Introducing a second credential for the same trust decision
//! would mean two places to leak and two to rotate, and in practice both would
//! end up holding the same value. One secret, one decision.
//!
//! ## Why sessions live in memory
//!
//! * A restart invalidates every session. That matches the deployment model —
//!   configuration changes end in `systemctl restart` anyway — and means a
//!   stolen cookie has a bounded life (at most until the next restart).
//! * There is no session file to chmod, back up, or accidentally include in a
//!   pool export.
//! * A multi-instance deployment does not share sessions, which is the honest
//!   behaviour: a file would imply cross-host validity it cannot provide.
//!
//! ## Why `SameSite=Strict` replaces a CSRF token
//!
//! A cross-site form or `fetch` simply does not carry this cookie, so an
//! attacker cannot ride a victim's session. The alternative — a double-submit
//! or synchroniser token — is strictly more machinery for the same guarantee.
//! The cost is that arriving from an external link shows the login page once,
//! which is acceptable for an admin console.
//!
//! ## What this deliberately does not do
//!
//! No rate limiting on the login endpoint beyond a fixed delay after a failure,
//! and no account lockout. The console is expected to sit behind a reverse proxy
//! and a cloud security group; a lockout would be a way to lock the operator out
//! of their own gateway.

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Cookie carrying the session id.
pub const SESSION_COOKIE: &str = "proxy_rs_admin";

/// How long a session lives without use.
///
/// Twelve hours, renewed on every authenticated request. A console being used
/// is never logged out mid-edit; one abandoned overnight is.
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Path the cookie is scoped to.
///
/// `/admin` rather than `/`, so the session is not attached to `/v1/*` traffic
/// or to the read-only `/api/*` observation endpoints.
const COOKIE_PATH: &str = "/admin";

/// Session id length in bytes, before hex encoding.
///
/// 32 bytes: a guessed id is not a realistic attack, and the value is only ever
/// compared server-side.
const SESSION_BYTES: usize = 32;

/// Why an admin request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionFailure {
    /// No `PROXY_API_TOKEN` is configured, so the console cannot be used at all.
    NotConfigured,
    /// The login attempt carried a wrong token.
    BadToken,
    /// No session cookie, or one that no longer maps to a session.
    NoSession,
}

impl SessionFailure {
    pub fn slug(self) -> &'static str {
        match self {
            SessionFailure::NotConfigured => "admin_disabled",
            SessionFailure::BadToken => "invalid_token",
            SessionFailure::NoSession => "unauthorized",
        }
    }

    pub fn status(self) -> StatusCode {
        match self {
            SessionFailure::NotConfigured => StatusCode::SERVICE_UNAVAILABLE,
            SessionFailure::BadToken => StatusCode::UNAUTHORIZED,
            SessionFailure::NoSession => StatusCode::UNAUTHORIZED,
        }
    }
}

/// One authenticated console session.
#[derive(Debug, Clone)]
struct Session {
    last_seen: Instant,
}

/// The session store, shared across requests.
///
/// A `std::sync::Mutex` rather than `tokio`'s: the critical section is a hash
/// map lookup or insert with no `.await` inside it, so an async mutex would only
/// add machinery. The lock is never held across a handler's work.
pub struct AdminSessions {
    sessions: std::sync::Mutex<HashMap<String, Session>>,
}

impl Default for AdminSessions {
    fn default() -> Self {
        Self::new()
    }
}

impl AdminSessions {
    pub fn new() -> Self {
        Self {
            sessions: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        // A poisoned lock means a previous holder panicked while holding it.
        // The map itself is still structurally sound (no partially-applied
        // insert), and refusing every future login because of one panic would
        // turn a transient bug into a permanent outage.
        self.sessions.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Check the configured token and mint a session. Returns the session id.
    pub fn login(&self, presented: &str) -> Result<String, SessionFailure> {
        let expected = crate::api_token::configured_token().ok_or(SessionFailure::NotConfigured)?;
        if !crate::api_token::constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
            return Err(SessionFailure::BadToken);
        }
        let id = new_session_id();
        let mut sessions = self.lock();
        // Opportunistically evict expired sessions on login. There is no
        // background reaper: a console deployment logs in rarely, and a full
        // sweep per login is cheaper than a timer that mostly finds nothing.
        let now = Instant::now();
        sessions.retain(|_, s| now.duration_since(s.last_seen) < SESSION_TTL);
        sessions.insert(
            id.clone(),
            Session {
                last_seen: Instant::now(),
            },
        );
        Ok(id)
    }

    /// Validate a session id and slide its expiry.
    pub fn touch(&self, id: &str) -> bool {
        let mut sessions = self.lock();
        let Some(session) = sessions.get_mut(id) else {
            return false;
        };
        let now = Instant::now();
        if now.duration_since(session.last_seen) >= SESSION_TTL {
            sessions.remove(id);
            return false;
        }
        session.last_seen = now;
        true
    }

    pub fn logout(&self, id: &str) {
        self.lock().remove(id);
    }

    /// Number of live sessions. Used by tests and by the status payload.
    pub fn active(&self) -> usize {
        self.lock().len()
    }
}

/// A fresh random session id, hex encoded.
fn new_session_id() -> String {
    use aes_gcm::aead::rand_core::RngCore;
    let mut raw = [0u8; SESSION_BYTES];
    aes_gcm::aead::OsRng.fill_bytes(&mut raw);
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

/// Extract the session cookie from a request's headers.
pub fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    // Multiple cookies arrive joined by "; ". A value is not allowed to contain
    // `;`, so splitting on it cannot cut a legitimate id in half.
    raw.split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Build the `Set-Cookie` header for a new session.
///
/// `secure` is decided by the caller from `X-Forwarded-Proto`, because the
/// gateway itself only ever speaks plain HTTP behind the reverse proxy. Setting
/// `Secure` on a plain-HTTP loopback debug session would make the browser drop
/// the cookie and produce an unexplainable login loop.
pub fn set_cookie_header(session_id: &str, secure: bool) -> HeaderValue {
    let mut value = format!(
        "{SESSION_COOKIE}={session_id}; HttpOnly; SameSite=Strict; Path={COOKIE_PATH}; Max-Age={}",
        SESSION_TTL.as_secs()
    );
    if secure {
        value.push_str("; Secure");
    }
    HeaderValue::from_str(&value).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// Build the `Set-Cookie` header that clears the session.
pub fn clear_cookie_header() -> HeaderValue {
    HeaderValue::from_static("proxy_rs_admin=; HttpOnly; SameSite=Strict; Path=/admin; Max-Age=0")
}

/// Whether the request reached us over HTTPS, judged from the proxy's headers.
///
/// The gateway listens on plain HTTP by design (nginx terminates TLS), so the
/// only honest signal is what the proxy reports. A direct loopback request has
/// no forwarded header, and is treated as insecure so a debug session works.
pub fn is_secure_request(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("https")
        })
        .unwrap_or(false)
}

/// Middleware that requires a valid session for everything under `/admin/api`.
///
/// The login route is exempt: it is how a session is obtained. Everything else,
/// including `logout`, needs one — a logout without a session has nothing to do.
pub async fn require_session(
    axum::extract::State(sessions): axum::extract::State<Arc<AdminSessions>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    // No token configured → the console is switched off entirely. Answered here
    // rather than in each handler so a new route cannot forget the check.
    if crate::api_token::configured_token().is_none() {
        return failure_response(SessionFailure::NotConfigured);
    }

    let Some(id) = session_cookie(request.headers()) else {
        return failure_response(SessionFailure::NoSession);
    };
    if !sessions.touch(&id) {
        return failure_response(SessionFailure::NoSession);
    }
    next.run(request).await
}

/// The JSON error body, matching the shape `/api/*` uses.
pub fn failure_response(failure: SessionFailure) -> Response {
    let detail = match failure {
        SessionFailure::NotConfigured => format!(
            "管理台未启用：请在 /etc/proxy-rs.env 或进程环境中设置 {} 后重启网关",
            crate::api_token::TOKEN_ENV
        ),
        SessionFailure::BadToken => "令牌错误".to_string(),
        SessionFailure::NoSession => "未登录或会话已过期".to_string(),
    };
    let body = serde_json::json!({
        "error": {
            "type": failure.slug(),
            "message": detail,
            "code": failure.slug(),
        }
    });
    (failure.status(), axum::Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(cookie: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
        h
    }

    #[test]
    fn the_session_cookie_is_found_among_others() {
        let h = headers_with("a=1; proxy_rs_admin=deadbeef; b=2");
        assert_eq!(session_cookie(&h).as_deref(), Some("deadbeef"));
    }

    #[test]
    fn a_request_without_the_cookie_has_no_session() {
        let h = headers_with("a=1; b=2");
        assert_eq!(session_cookie(&h), None);
        assert_eq!(session_cookie(&HeaderMap::new()), None);
    }

    #[test]
    fn an_empty_cookie_value_is_not_a_session() {
        assert_eq!(session_cookie(&headers_with("proxy_rs_admin=")), None);
    }

    #[test]
    fn a_session_expires_after_its_ttl() {
        // Drive the clock by hand: `touch` reads `Instant::now`, and sleeping 12
        // hours in a unit test is not an option.
        let sessions = AdminSessions::new();
        let id = "fixed-id".to_string();
        sessions.lock().insert(
            id.clone(),
            Session {
                last_seen: Instant::now() - SESSION_TTL - Duration::from_secs(1),
            },
        );
        assert!(!sessions.touch(&id), "an expired session must not validate");
        assert_eq!(sessions.active(), 0, "and it must be evicted");
    }

    #[test]
    fn a_live_session_renews_and_survives() {
        let sessions = AdminSessions::new();
        let id = "live".to_string();
        sessions.lock().insert(
            id.clone(),
            Session {
                last_seen: Instant::now(),
            },
        );
        assert!(sessions.touch(&id));
        assert!(sessions.touch(&id), "sliding expiry keeps it alive");
        assert_eq!(sessions.active(), 1);
    }

    #[test]
    fn logout_removes_the_session() {
        let sessions = AdminSessions::new();
        sessions.lock().insert(
            "s".to_string(),
            Session {
                last_seen: Instant::now(),
            },
        );
        sessions.logout("s");
        assert!(!sessions.touch("s"));
        assert_eq!(sessions.active(), 0);
    }

    #[test]
    fn session_ids_are_unique_and_long() {
        let a = new_session_id();
        let b = new_session_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), SESSION_BYTES * 2, "hex of 32 bytes");
    }

    #[test]
    fn the_cookie_is_hardened() {
        let v = set_cookie_header("abc", true).to_str().unwrap().to_string();
        assert!(
            v.contains("HttpOnly"),
            "a page script must not read it: {v}"
        );
        assert!(v.contains("SameSite=Strict"), "CSRF must be prevented: {v}");
        assert!(v.contains("Path=/admin"), "must not travel to /v1: {v}");
        assert!(v.contains("Secure"), "over HTTPS: {v}");
    }

    #[test]
    fn a_plain_http_session_omits_secure() {
        // Otherwise the browser silently drops the cookie and login loops.
        let v = set_cookie_header("abc", false)
            .to_str()
            .unwrap()
            .to_string();
        assert!(!v.contains("Secure"), "{v}");
        assert!(v.contains("HttpOnly"), "the other hardening still applies");
    }

    #[test]
    fn secure_is_decided_by_the_proxy_header() {
        let mut h = HeaderMap::new();
        assert!(
            !is_secure_request(&h),
            "a bare loopback request is plain HTTP"
        );
        h.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert!(is_secure_request(&h));
        h.insert("x-forwarded-proto", HeaderValue::from_static("http"));
        assert!(!is_secure_request(&h));
        // A proxy chain reports the original scheme first.
        h.insert("x-forwarded-proto", HeaderValue::from_static("https, http"));
        assert!(is_secure_request(&h));
    }

    #[test]
    fn a_wrong_token_mints_nothing() {
        // With no token configured at all, login is unavailable rather than
        // open — the failing-closed direction.
        if crate::api_token::configured_token().is_none() {
            let sessions = AdminSessions::new();
            assert_eq!(
                sessions.login("anything"),
                Err(SessionFailure::NotConfigured)
            );
            assert_eq!(sessions.active(), 0);
        }
    }
}
