//! Inbound access-key gate for the `/v1/*` data plane.
//!
//! ## What this guards, and what it deliberately does not
//!
//! Only the **forwarding** surface — the routes that spend the upstream
//! account's quota. `/health` stays open so a reverse proxy or an uptime probe
//! can reach it, and `/api/*` stays as it was because the desktop console reads
//! it over loopback with `fetch`; putting a key in front of those would break
//! the GUI on every machine. The public edge is expected to hide `/api/*` and
//! `/metrics` at the reverse proxy instead (see the plan doc §4.4).
//!
//! ## Where the key comes from
//!
//! [`KEY_HEADER`], and nothing else. It does **not** read `x-api-key` or
//! `authorization`: those belong to `UPSTREAM_API_KEY_PASSTHROUGH` and the
//! upstream credential path, and one header meaning two things depending on a
//! switch is exactly how a gateway key ends up being forwarded upstream.
//!
//! ## Failure shape
//!
//! A 401 with an Anthropic-style `authentication_error` body, not the project's
//! generic `proxy_error` envelope. The distinction matters: Claude Code and
//! Codex treat `proxy_error` as a transient gateway fault and **retry**, which
//! for a wrong key means an infinite retry loop. `authentication_error` is the
//! shape their SDKs branch on to stop and tell the user to fix their config.

use crate::client_accounts::{self, ClientAccountIdentity, ClientAuthState, VerifyFailure};
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderName, HeaderValue, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

/// Request paths that must keep working without a key.
///
/// Matched on the exact path (no prefix magic) so adding a route cannot
/// accidentally inherit an exemption.
const OPEN_PATHS: &[&str] = &["/health", "/metrics"];

/// Whether `path` is outside the gate.
///
/// `/api/*` is open here on purpose — see the module docs. The admin routes
/// under `/api/admin/*` carry their own `PROXY_API_TOKEN` check inside the
/// handler, so an open middleware does not leave them unauthenticated.
fn is_open_path(path: &str) -> bool {
    OPEN_PATHS.contains(&path) || path.starts_with("/api/")
}

/// Build the 401 body.
///
/// `type: authentication_error` is what makes a client stop rather than retry;
/// the message names the header so a user can act on it without reading docs.
fn unauthorized(failure: VerifyFailure) -> Response {
    let detail = match failure {
        VerifyFailure::Missing => {
            format!(
                "Missing gateway access key. Send it in the `{}` header.",
                client_accounts::KEY_HEADER
            )
        }
        VerifyFailure::Unknown => "Invalid gateway access key.".to_string(),
        VerifyFailure::Disabled => "This gateway access key has been disabled.".to_string(),
    };
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "authentication_error",
            "message": detail,
            "code": failure.slug(),
        }
    });
    let mut response = (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response();
    // RFC 7235: a 401 says which scheme to use. Clients that surface this header
    // show a far more useful message than "401".
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"proxy-rs\""),
    );
    response
}

/// The gate itself.
///
/// Runs before the handler. On success it inserts [`ClientAccountIdentity`] into
/// the request extensions (so the handler can log *who* called) and removes the
/// key header before forwarding on — the outbound header map is built from an
/// allow-list, so the key could not reach the upstream anyway, but stripping it
/// here means a future change to "forward inbound headers" cannot silently leak
/// a gateway key upstream.
pub async fn guard(
    State(state): State<Arc<ClientAuthState>>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    // Switched off: behave exactly as the gateway did before this feature
    // existed. An upgrade must not break existing CLI configs, so this path is
    // the default and must stay allocation-free-ish and side-effect free.
    if !state.is_enabled() {
        return next.run(request).await;
    }

    let path = request.uri().path().to_string();
    if is_open_path(&path) {
        return next.run(request).await;
    }

    // The key never reaches the handler: taken out before the check so a failed
    // request cannot be logged with it present either.
    let presented = request
        .headers_mut()
        .remove(HeaderName::from_static(client_accounts::KEY_HEADER))
        .and_then(|value| value.to_str().ok().map(str::to_owned));

    let Some(presented) = presented else {
        return unauthorized(VerifyFailure::Missing);
    };

    match state.verify(&presented).await {
        Ok(identity) => {
            let id = identity.id.clone();
            request.extensions_mut().insert(identity);
            // Observational only, throttled internally; cannot fail the request.
            state.touch(&id).await;
            next.run(request).await
        }
        Err(failure) => unauthorized(failure),
    }
}

/// The account that authenticated a request, for the handler's log line.
///
/// Returns an empty string when the gate is off (or the route was exempt), so
/// the log format stays uniform instead of sometimes omitting the field.
pub fn account_label(request: &Request<Body>) -> String {
    request
        .extensions()
        .get::<ClientAccountIdentity>()
        .map(|identity| identity.label.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_and_metrics_stay_open() {
        assert!(is_open_path("/health"));
        assert!(is_open_path("/metrics"));
    }

    #[test]
    fn the_observation_api_stays_open_for_the_desktop_console() {
        // The GUI reads these over loopback with `fetch`. Gating them here would
        // break the console on every machine; the public edge hides them.
        assert!(is_open_path("/api/stats/today"));
        assert!(is_open_path("/api/request-logs"));
        assert!(is_open_path("/api/admin/client-accounts"));
    }

    #[test]
    fn the_forwarding_surface_is_gated() {
        for path in [
            "/v1/messages",
            "/v1/responses",
            "/v1/chat/completions",
            "/responses",
            "/chat/completions",
            "/backend-api/codex/responses",
            "/v1/models",
            "/models",
            "/v1/credits",
            "/credits",
        ] {
            assert!(!is_open_path(path), "{path} must require a key");
        }
    }

    #[test]
    fn a_missing_or_wrong_key_answers_401_with_an_authentication_error() {
        for failure in [
            VerifyFailure::Missing,
            VerifyFailure::Unknown,
            VerifyFailure::Disabled,
        ] {
            let response = unauthorized(failure);
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                response
                    .headers()
                    .get(header::WWW_AUTHENTICATE)
                    .and_then(|v| v.to_str().ok()),
                Some("Bearer realm=\"proxy-rs\"")
            );
        }
    }
}
