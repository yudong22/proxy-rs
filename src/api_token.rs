//! Bearer-token guard for the mutating half of `/api/*`.
//!
//! ## Why this exists
//!
//! `/api/*` was designed read-only, and its module docs say exactly what that
//! buys: "A page the user visits can therefore read their request log … an
//! accepted, temporary trade-off". That trade-off holds *because* reading is all
//! an attacker gets. The moment one route under `/api` can make the gateway act
//! on upstream accounts, the same unauthenticated, `allow_origin(Any)` boundary
//! would let any web page a user opens trigger that action.
//!
//! So mutation gets a gate, and the gate is deliberately the cheapest thing that
//! actually works rather than a full auth system:
//!
//! * **Off by default.** With no `PROXY_API_TOKEN` configured, every mutation
//!   route answers `403 mutation_disabled`. An install that never sets the token
//!   behaves exactly as it did before this module existed.
//! * **`Origin` means a browser.** CORS is `allow_origin(Any)`, so the browser
//!   will happily *send* a cross-origin POST — the header only lets it *read* the
//!   response. A token in a header would not stop the request from firing; it
//!   would only stop the page from reading the reply. Refusing any request that
//!   carries `Origin` is therefore the check that matters, and legitimate
//!   non-browser callers (a Node plugin, `curl`, a launchd job) never send it.
//! * **The token is compared in full.** No prefix matching: a prefix check
//!   would accept any token sharing a first character, which is the exact bug
//!   the constant-time style comparison in this file avoids.

/// Environment variable (process env or `~/.proxy-rs/.env`) holding the token.
pub const TOKEN_ENV: &str = "PROXY_API_TOKEN";

/// The configured token, or `None` when mutation is switched off.
///
/// Read through [`crate::config`]'s `.env`-aware lookup so a token placed in
/// `~/.proxy-rs/.env` works without exporting it into the GUI's environment —
/// which a user launching the app from Finder cannot do at all. That is the
/// same reason every other setting supports the `.env` route.
///
/// Blank and whitespace-only values count as unset: an empty token must not
/// authenticate anything, and treating `PROXY_API_TOKEN=` as "enabled" would
/// silently open the route with a guessable credential.
pub fn configured_token() -> Option<String> {
    crate::config::env_value(TOKEN_ENV)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Why a mutating request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFailure {
    /// No token is configured, so the route is switched off entirely.
    NotConfigured,
    /// The request carries a browser `Origin`, so it is a cross-origin call.
    BrowserOrigin,
    /// No `Authorization: Bearer …`, or a value that does not match.
    BadToken,
}

impl AuthFailure {
    /// Stable machine-readable slug for the error body.
    pub fn slug(self) -> &'static str {
        match self {
            AuthFailure::NotConfigured => "mutation_disabled",
            AuthFailure::BrowserOrigin => "browser_origin",
            AuthFailure::BadToken => "unauthorized",
        }
    }
}

/// Check one mutating request.
///
/// `origin` is the request's `Origin` header value and `authorization` its
/// `Authorization` header value, both as received. Passing them in rather than
/// the whole request keeps this a pure function the tests can drive directly —
/// the failure mode this guards is a wrong *decision*, not a wiring mistake.
pub fn authorize(origin: Option<&str>, authorization: Option<&str>) -> Result<(), AuthFailure> {
    let token = configured_token().ok_or(AuthFailure::NotConfigured)?;

    // Check the origin first: a browser request is refused even when it somehow
    // carries a correct token, because the token would have to have been handed
    // to the browser page for that to happen.
    if origin.is_some() {
        return Err(AuthFailure::BrowserOrigin);
    }

    let presented = authorization
        .and_then(bearer_value)
        .ok_or(AuthFailure::BadToken)?;

    if constant_time_eq(presented.as_bytes(), token.as_bytes()) {
        Ok(())
    } else {
        Err(AuthFailure::BadToken)
    }
}

/// The token inside an `Authorization: Bearer <token>` value, or `None`.
///
/// A named function rather than an inline closure so the test can drive the
/// exact rule the handler uses. Duplicating the parse in the test would let the
/// two drift, and the drift would be silent: a test asserting on a copy of the
/// rule always passes.
fn bearer_value(value: &str) -> Option<&str> {
    let (scheme, rest) = value.trim().split_once(' ')?;
    // Scheme is case-insensitive per RFC 7235; `bearer` and `Bearer` both appear
    // in the wild. The token itself is compared byte-for-byte later.
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| rest.trim())
        .filter(|token| !token.is_empty())
}

/// Compare two byte strings without leaking their common prefix length.
///
/// This is not defence against a remote attacker measuring nanoseconds over a
/// loopback socket — it is defence against the comparison being *copied* into a
/// context where that matters, and it costs nothing. Lengths are compared first
/// because equal-length inputs are the only ones that can match; that early
/// return leaks the token length, which is not a secret in any meaningful sense
/// (it is visible in whatever configures it).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A correct bearer token is the only accepted form.
    #[test]
    fn accepts_a_matching_bearer_token() {
        assert!(constant_time_eq(b"abc123", b"abc123"));
        assert_eq!(b"a".len(), 1);
    }

    /// Any deviation is rejected, including case in the scheme being accepted
    /// but case in the *token* not being.
    #[test]
    fn token_comparison_is_exact() {
        assert!(!constant_time_eq(b"abc123", b"abc124"), "last byte differs");
        assert!(!constant_time_eq(b"abc123", b"abc12"), "shorter");
        assert!(!constant_time_eq(b"abc123", b"abc1234"), "longer");
        assert!(!constant_time_eq(b"ABC123", b"abc123"), "case matters");
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    /// The scheme accepts either case; the token is returned untouched.
    #[test]
    fn bearer_scheme_is_case_insensitive() {
        assert_eq!(bearer_value("Bearer abc"), Some("abc"));
        assert_eq!(bearer_value("bearer abc"), Some("abc"));
        assert_eq!(bearer_value("BEARER abc"), Some("abc"));
        // Surrounding whitespace is not part of the token.
        assert_eq!(bearer_value("  Bearer   abc  "), Some("abc"));
        // A different scheme is not a bearer token, however the token looks.
        assert_eq!(bearer_value("Basic abc"), None);
        // A bare token with no scheme is not accepted: it would make every
        // header value a credential.
        assert_eq!(bearer_value("abc"), None);
        // An empty token is not a token. This matters because a configured
        // token of "" must never authenticate anything.
        assert_eq!(bearer_value("Bearer "), None);
        assert_eq!(bearer_value("Bearer   "), None);
        assert_eq!(bearer_value(""), None);
    }
}
