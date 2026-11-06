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

/// How long a session lives without use, by default.
///
/// Twelve hours, renewed on every authenticated request. A console being used
/// is never logged out mid-edit; one abandoned overnight is.
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// How long a "remember me" session lives.
///
/// Seven days. The operator opted in explicitly, and the threat this widens —
/// a stolen cookie on a shared machine — is one they just chose to accept. The
/// default stays short precisely so that choice has to be made, not defaulted
/// into.
pub const SESSION_TTL_REMEMBERED: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How many consecutive failures a client may accumulate before login is
/// refused outright.
///
/// Five: enough for a genuine typo or two (tokens get copy-pasted from a file
/// and a trailing newline is a classic), few enough that guessing is hopeless.
const LOGIN_MAX_ATTEMPTS: u32 = 5;

/// How long a client stays locked out after [`LOGIN_MAX_ATTEMPTS`].
///
/// Exponentially extended on repeat offence, up to [`LOGIN_LOCKOUT_MAX`].
const LOGIN_LOCKOUT: Duration = Duration::from_secs(60);

/// Ceiling for the escalating lockout.
const LOGIN_LOCKOUT_MAX: Duration = Duration::from_secs(60 * 60);

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
    /// Too many failed attempts from this client; login is temporarily refused.
    ///
    /// Distinct from `BadToken` because the operator must not be told "wrong
    /// token" when the truth is "wait a minute". Retrying a correct token while
    /// locked out is what turns a fumbled paste into a support call.
    RateLimited { retry_after_secs: u64 },
}

impl SessionFailure {
    pub fn slug(self) -> &'static str {
        match self {
            SessionFailure::NotConfigured => "admin_disabled",
            SessionFailure::BadToken => "invalid_token",
            SessionFailure::NoSession => "unauthorized",
            SessionFailure::RateLimited { .. } => "rate_limited",
        }
    }

    pub fn status(self) -> StatusCode {
        match self {
            SessionFailure::NotConfigured => StatusCode::SERVICE_UNAVAILABLE,
            SessionFailure::BadToken => StatusCode::UNAUTHORIZED,
            SessionFailure::NoSession => StatusCode::UNAUTHORIZED,
            // 429, not 401: the credential may well have been right.
            SessionFailure::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
        }
    }
}

/// One authenticated console session.
#[derive(Debug, Clone)]
struct Session {
    last_seen: Instant,
    /// Per-session expiry horizon, so "remember me" does not require a second
    /// store. `touch` compares against this, not the global default.
    ttl: Duration,
}

/// Failed-attempt bookkeeping for one client address.
#[derive(Debug, Clone)]
struct Attempts {
    /// Consecutive failures since the last success.
    count: u32,
    /// When the current lockout ends, if one is in force.
    locked_until: Option<Instant>,
    /// How many times this client has been locked out, for the escalation.
    lockouts: u32,
    /// When this entry was last touched, for eviction.
    ///
    /// The map is keyed on a client-supplied value, so it must be bounded: an
    /// attacker rotating `X-Forwarded-For` could otherwise add an entry per
    /// request and grow it without limit (~100 bytes each; a million forged
    /// addresses is ~100 MB in a process that should stay small). Entries are
    /// swept once they are stale, and a hard cap bounds the pathological case.
    last_seen: Instant,
}

/// Entries kept in the attempts map before the oldest are dropped.
///
/// A backstop for the sweep, not the primary bound: a real deployment has a
/// handful of operators, so anything approaching this is an attack and dropping
/// history (which only *shortens* a lockout) is the safe direction.
const ATTEMPTS_MAX_ENTRIES: usize = 4096;

/// How long an idle attempts entry survives.
///
/// Comfortably longer than the maximum lockout, so an entry never disappears
/// while it is still supposed to be enforcing a wait.
const ATTEMPTS_IDLE_TTL: Duration = Duration::from_secs(60 * 60 * 2);

/// The session store, shared across requests.
///
/// A `std::sync::Mutex` rather than `tokio`'s: the critical section is a hash
/// map lookup or insert with no `.await` inside it, so an async mutex would only
/// add machinery. The lock is never held across a handler's work.
pub struct AdminSessions {
    sessions: std::sync::Mutex<HashMap<String, Session>>,
    /// Failed attempts keyed by client address.
    ///
    /// Keyed on the address rather than a global counter: a global one lets any
    /// stranger lock the operator out of their own gateway, which converts a
    /// brute-force defence into a denial-of-service primitive.
    attempts: std::sync::Mutex<HashMap<String, Attempts>>,
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
            attempts: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn lock_attempts(&self) -> std::sync::MutexGuard<'_, HashMap<String, Attempts>> {
        self.attempts.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Whether `client` may attempt a login right now.
    ///
    /// Returns the seconds to wait when it may not. A lockout that has elapsed
    /// is cleared here, so the caller does not have to remember to do it.
    pub fn check_rate_limit(&self, client: &str) -> Result<(), SessionFailure> {
        let mut attempts = self.lock_attempts();
        let Some(entry) = attempts.get_mut(client) else {
            return Ok(());
        };
        let Some(until) = entry.locked_until else {
            return Ok(());
        };
        let now = Instant::now();
        if now >= until {
            // Lockout served. Keep the failure count until a *success* clears it,
            // so an attacker cannot reset the escalation by simply waiting.
            entry.locked_until = None;
            return Ok(());
        }
        Err(SessionFailure::RateLimited {
            retry_after_secs: (until - now).as_secs().max(1),
        })
    }

    /// Record one failed attempt, locking the client out once the budget is gone.
    fn record_failure(&self, client: &str) {
        let mut attempts = self.lock_attempts();
        let now = Instant::now();

        // Sweep before inserting, so the map cannot grow without bound.
        attempts.retain(|_, a| now.duration_since(a.last_seen) < ATTEMPTS_IDLE_TTL);
        if attempts.len() >= ATTEMPTS_MAX_ENTRIES && !attempts.contains_key(client) {
            // Still full after the sweep: drop the least recently seen. Losing a
            // stale entry only shortens some other client's lockout, which is the
            // safe direction to fail in.
            if let Some(oldest) = attempts
                .iter()
                .min_by_key(|(_, a)| a.last_seen)
                .map(|(k, _)| k.clone())
            {
                attempts.remove(&oldest);
            }
        }

        let entry = attempts.entry(client.to_string()).or_insert(Attempts {
            count: 0,
            locked_until: None,
            lockouts: 0,
            last_seen: now,
        });
        entry.last_seen = now;
        entry.count += 1;
        if entry.count >= LOGIN_MAX_ATTEMPTS {
            // Escalate: 1 min, 2 min, 4 min … capped. A client that keeps
            // hammering after being unlocked earns a longer wait each time.
            let factor = 1u32 << entry.lockouts.min(6);
            let wait = (LOGIN_LOCKOUT * factor).min(LOGIN_LOCKOUT_MAX);
            entry.locked_until = Some(Instant::now() + wait);
            entry.lockouts += 1;
            entry.count = 0;
        }
    }

    /// Clear a client's failure history. Called only on a successful login.
    fn record_success(&self, client: &str) {
        self.lock_attempts().remove(client);
    }

    /// How many failures remain before the client is locked out.
    ///
    /// Surfaced to the login page so it can warn *before* the lockout rather
    /// than after — "还剩 2 次" is actionable, a sudden 429 is not.
    pub fn attempts_remaining(&self, client: &str) -> u32 {
        let attempts = self.lock_attempts();
        let used = attempts.get(client).map(|a| a.count).unwrap_or(0);
        LOGIN_MAX_ATTEMPTS.saturating_sub(used)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        // A poisoned lock means a previous holder panicked while holding it.
        // The map itself is still structurally sound (no partially-applied
        // insert), and refusing every future login because of one panic would
        // turn a transient bug into a permanent outage.
        self.sessions.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Check the configured token and mint a session. Returns the session id.
    ///
    /// `client` is the caller's address, used for the failed-attempt budget.
    /// `remember` selects the long TTL — an explicit opt-in, so the short
    /// default is a decision rather than an accident.
    pub fn login(
        &self,
        presented: &str,
        client: &str,
        remember: bool,
    ) -> Result<String, SessionFailure> {
        let expected = crate::api_token::configured_token().ok_or(SessionFailure::NotConfigured)?;

        // Refuse before comparing: a locked-out client must not learn anything
        // from this request, not even whether the token length matched.
        self.check_rate_limit(client)?;

        if !crate::api_token::constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
            self.record_failure(client);
            // Tell the caller how many tries are left, so the UI can warn before
            // the lockout lands rather than only after it.
            return Err(SessionFailure::BadToken);
        }
        self.record_success(client);

        let id = new_session_id();
        let ttl = if remember {
            SESSION_TTL_REMEMBERED
        } else {
            SESSION_TTL
        };
        let mut sessions = self.lock();
        // Opportunistically evict expired sessions on login. There is no
        // background reaper: a console deployment logs in rarely, and a full
        // sweep per login is cheaper than a timer that mostly finds nothing.
        let now = Instant::now();
        sessions.retain(|_, s| now.duration_since(s.last_seen) < s.ttl);
        sessions.insert(
            id.clone(),
            Session {
                last_seen: Instant::now(),
                ttl,
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
        // Compared against the session's own TTL, so a "remember me" session is
        // not silently shortened to the default one.
        if now.duration_since(session.last_seen) >= session.ttl {
            sessions.remove(id);
            return false;
        }
        session.last_seen = now;
        true
    }

    /// The TTL of a live session, for the cookie's `Max-Age`.
    ///
    /// The cookie and the server-side record must agree, or the browser keeps
    /// presenting a cookie the server has already dropped (which looks like a
    /// random logout).
    pub fn ttl_of(&self, id: &str) -> Option<Duration> {
        self.lock().get(id).map(|s| s.ttl)
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
pub fn set_cookie_header(session_id: &str, secure: bool, ttl: Duration) -> HeaderValue {
    let mut value = format!(
        "{SESSION_COOKIE}={session_id}; HttpOnly; SameSite=Strict; Path={COOKIE_PATH}; Max-Age={}",
        ttl.as_secs()
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

/// Whether forwarded headers may be trusted, decided by the bind address.
///
/// They are trustworthy exactly when a reverse proxy sits in front, and on this
/// deployment that is signalled by binding loopback: only the proxy can reach
/// `127.0.0.1:3456`, so anything arriving there came through it. A gateway bound
/// to `0.0.0.0` is directly reachable, and then `X-Forwarded-For` is whatever the
/// caller typed — trusting it would let one attacker defeat both the rate limiter
/// and the `Secure` cookie decision.
///
/// Read from the settings file each call rather than cached: it is a small JSON
/// read on a path that already does file I/O (the session store), and caching
/// would make a bind change silently keep the old trust decision.
pub fn trusted_proxy(bind_addr: &str) -> bool {
    let bind = bind_addr.trim();
    bind == "127.0.0.1" || bind == "::1" || bind == "localhost"
}

/// Whether the request reached us over HTTPS, judged from the proxy's headers.
///
/// The gateway listens on plain HTTP by design (nginx terminates TLS), so the
/// only honest signal is what the proxy reports. A direct loopback request has
/// no forwarded header, and is treated as insecure so a debug session works.
pub fn is_secure_request(headers: &HeaderMap, trust_forwarded: bool) -> bool {
    if !trust_forwarded {
        // Direct connection: no proxy to report the original scheme, and the
        // gateway speaks plain HTTP. Reporting `false` is the honest answer, and
        // it only means the cookie omits `Secure` — which is correct for the
        // plain-HTTP loopback case it describes.
        return false;
    }
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
        SessionFailure::RateLimited { retry_after_secs } => {
            format!("登录尝试过于频繁，请 {retry_after_secs} 秒后重试")
        }
    };
    let mut error = serde_json::json!({
        "type": failure.slug(),
        "message": detail,
        "code": failure.slug(),
    });
    // Structured wait, not just prose. The login page re-enables its button from
    // this number, and a client parsing the Chinese sentence to find it would be
    // one wording change away from breaking.
    if let SessionFailure::RateLimited { retry_after_secs } = failure {
        if let Some(obj) = error.as_object_mut() {
            obj.insert(
                "retry_after_secs".into(),
                serde_json::json!(retry_after_secs),
            );
        }
    }
    let body = serde_json::json!({ "error": error });
    let mut response = (failure.status(), axum::Json(body)).into_response();
    if let SessionFailure::RateLimited { retry_after_secs } = failure {
        // RFC 6585: a 429 SHOULD carry Retry-After. Costs one line and makes the
        // response correct for any generic client, not just our own page.
        if let Ok(value) = axum::http::HeaderValue::from_str(&retry_after_secs.to_string()) {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
    }
    response
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
                ttl: SESSION_TTL,
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
                ttl: SESSION_TTL,
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
                ttl: SESSION_TTL,
            },
        );
        sessions.logout("s");
        assert!(!sessions.touch("s"));
        assert_eq!(sessions.active(), 0);
    }

    #[test]
    fn repeated_failures_lock_the_client_out() {
        // The token is not configured under `cargo test` in this unit, so use the
        // limiter directly: it is the part that carries the logic.
        let sessions = AdminSessions::new();
        for _ in 0..LOGIN_MAX_ATTEMPTS {
            assert!(sessions.check_rate_limit("1.2.3.4").is_ok());
            sessions.record_failure("1.2.3.4");
        }
        match sessions.check_rate_limit("1.2.3.4") {
            Err(SessionFailure::RateLimited { retry_after_secs }) => {
                assert!(retry_after_secs > 0, "a lockout must state a wait");
            }
            other => panic!("expected a lockout, got {other:?}"),
        }
    }

    #[test]
    fn a_lockout_is_per_client_not_global() {
        // A global counter would let one stranger lock the operator out of their
        // own gateway — turning the brute-force defence into a DoS primitive.
        let sessions = AdminSessions::new();
        for _ in 0..LOGIN_MAX_ATTEMPTS {
            sessions.record_failure("attacker");
        }
        assert!(sessions.check_rate_limit("attacker").is_err());
        assert!(
            sessions.check_rate_limit("operator").is_ok(),
            "an unrelated client must not be affected"
        );
    }

    #[test]
    fn a_success_clears_the_failure_history() {
        let sessions = AdminSessions::new();
        for _ in 0..(LOGIN_MAX_ATTEMPTS - 1) {
            sessions.record_failure("c");
        }
        assert_eq!(sessions.attempts_remaining("c"), 1);
        sessions.record_success("c");
        assert_eq!(
            sessions.attempts_remaining("c"),
            LOGIN_MAX_ATTEMPTS,
            "a successful login resets the budget"
        );
    }

    #[test]
    fn attempts_remaining_counts_down_before_the_lockout() {
        let sessions = AdminSessions::new();
        assert_eq!(sessions.attempts_remaining("fresh"), LOGIN_MAX_ATTEMPTS);
        sessions.record_failure("fresh");
        assert_eq!(sessions.attempts_remaining("fresh"), LOGIN_MAX_ATTEMPTS - 1);
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
        let v = set_cookie_header("abc", true, SESSION_TTL)
            .to_str()
            .unwrap()
            .to_string();
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
        let v = set_cookie_header("abc", false, SESSION_TTL)
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
            !is_secure_request(&h, true),
            "a bare loopback request is plain HTTP"
        );
        h.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert!(is_secure_request(&h, true));
        h.insert("x-forwarded-proto", HeaderValue::from_static("http"));
        assert!(!is_secure_request(&h, true));
        // A proxy chain reports the original scheme first.
        h.insert("x-forwarded-proto", HeaderValue::from_static("https, http"));
        assert!(is_secure_request(&h, true));
    }

    #[test]
    fn the_scheme_header_is_ignored_without_a_trusted_proxy() {
        // A gateway bound to a public interface is directly reachable, so a
        // caller can set this header itself. Believing it would be believing the
        // attacker.
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert!(!is_secure_request(&h, false));
    }

    #[test]
    fn only_loopback_bindings_trust_forwarded_headers() {
        assert!(trusted_proxy("127.0.0.1"));
        assert!(trusted_proxy("::1"));
        assert!(trusted_proxy(" localhost "));
        // Anything publicly reachable must not believe client-supplied headers.
        assert!(!trusted_proxy("0.0.0.0"));
        assert!(!trusted_proxy("::"));
        assert!(!trusted_proxy("10.0.0.5"));
    }

    #[test]
    fn a_wrong_token_mints_nothing() {
        // With no token configured at all, login is unavailable rather than
        // open — the failing-closed direction.
        if crate::api_token::configured_token().is_none() {
            let sessions = AdminSessions::new();
            assert_eq!(
                sessions.login("anything", "test", false),
                Err(SessionFailure::NotConfigured)
            );
            assert_eq!(sessions.active(), 0);
        }
    }
}
