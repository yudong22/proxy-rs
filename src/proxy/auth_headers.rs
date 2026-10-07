//! Upstream authentication and client fingerprint headers.
//!
//! Split out of `proxy.rs` because this is the one place the two identity kinds
//! diverge on the wire, and the divergence is a correctness rule (see
//! `upstream_auth_headers`), not a detail: a login-state token travels as
//! `Authorization: Bearer` only and must never be mirrored into `x-api-key`.

use super::*;

/// Attach the upstream authentication + CLI fingerprint headers.
///
/// For the WorkBuddy/CodeBuddy flavor we mirror the official client's request
/// fingerprint: the official `User-Agent`, and a small set of low-risk
/// correlation headers the gateway expects from the genuine CLI
/// (`X-CodeBuddy-Request`, `Accept`, `X-Requested-With`). These are cheap and
/// were validated as passing in the troubleshooting doc's "full headers" test,
/// so we send them proactively. Other providers keep the plain `Authorization`
/// flow.
///
/// `credential` — when the request is served by a login-state credential — is
/// authoritative: its bearer token takes over `Authorization`, and any
/// static-key `x-api-key` is dropped rather than mirrored, because the login
/// token is not an API key (see [`upstream_auth_headers`]). This must be a
/// single header map because reqwest's `.header()` *appends* rather than
/// replaces, so issuing a static-key `Authorization` and then a credential
/// `Authorization` would put two conflicting values on the wire and the Tencent
/// `stgw` gateway answers that with a bare HTML `400 Bad Request`.
pub(crate) fn apply_upstream_auth(
    mut req: reqwest::RequestBuilder,
    config: &Config,
    api_key: &Option<String>,
    credential: Option<&crate::workbuddy_auth::WorkBuddyCredential>,
) -> reqwest::RequestBuilder {
    let headers = upstream_auth_headers(config, api_key, credential);
    if !headers.is_empty() {
        req = req.headers(headers);
    }
    req
}

/// Build the outbound auth/fingerprint header map (pure; unit-tested).
///
/// Every value is inserted with replace semantics. This matters because the
/// caller applies the map to a `reqwest::RequestBuilder` whose `.header()` API
/// *appends*: building the map here and applying it once means a credential and
/// a static key can never both contribute an `Authorization`/`x-api-key` value,
/// which is what produced the duplicate-header `400 Bad Request` from `stgw`.
///
/// The two identities carry *different* auth headers, and that asymmetry is
/// deliberate:
///
///   * a **static/API key** (`None`) is an api-key credential, so it travels as
///     `Authorization: Bearer <key>` **and** `x-api-key: <key>`;
///   * a **login-state credential** (`Some`) is a Keycloak bearer and travels as
///     `Authorization: Bearer <accessToken>` **only** — no `x-api-key` at all.
///
/// Sending an account token in `x-api-key` makes the gateway treat the api-key
/// as authoritative and answer `401 {"message":"not_found"}` even though the
/// bearer is perfectly valid; that is what made every account-pool request fail
/// over to the key pool.
pub(crate) fn upstream_auth_headers(
    config: &Config,
    api_key: &Option<String>,
    credential: Option<&crate::workbuddy_auth::WorkBuddyCredential>,
) -> HeaderMap {
    let workbuddy = config.models_flavor == ModelsFlavor::WorkBuddyConfig;
    let mut headers = HeaderMap::new();

    if workbuddy {
        // Official-client fingerprint; cheap and validated as passing.
        let mut put = |name: &'static str, value: &str| {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static("")),
            );
        };
        put("user-agent", crate::providers::WORKBUDDY_USER_AGENT);
        put("x-codebuddy-request", "1");
        put("accept", "application/json, text/event-stream");
        put("x-requested-with", "XMLHttpRequest");
    }

    match credential {
        // Login-state credential: one consistent identity, carried by
        // `Authorization: Bearer <accessToken>` plus the session-bound
        // fingerprint headers. No static-key value is ever inserted, so none can
        // survive.
        //
        // The access token must NOT be mirrored into `x-api-key`. The login
        // token is a Keycloak bearer, not an API key, and the gateway routes a
        // chat request sent with *both* headers as if the api-key were
        // authoritative: `Bearer <token>` alone answers 200, while
        // `Bearer <token>` + `x-api-key: <token>` answers
        // `401 {"message":"not_found"}`. That single mirrored header is why
        // every account-pool request failed and fell through to the API-key
        // pool. (`credits.rs` pairing Bearer + X-API-Key is a *static gateway
        // key*, and `billing_headers` deliberately sends no X-API-Key for a
        // login state for exactly this reason.)
        Some(cred) => {
            let token = cred.bearer();
            headers.insert(
                HeaderName::from_static("authorization"),
                HeaderValue::from_str(&format!("Bearer {}", token))
                    .unwrap_or_else(|_| HeaderValue::from_static("")),
            );
            for (name, value) in crate::workbuddy_auth::upstream_headers(cred) {
                if let (Ok(header_name), Ok(header_value)) = (
                    HeaderName::from_bytes(name.as_bytes()),
                    HeaderValue::from_str(&value),
                ) {
                    headers.insert(header_name, header_value);
                }
            }
            // Defensive: some WorkBuddy presets may have seeded an x-api-key
            // from the static-key path before this branch. A credential request
            // must never carry one.
            headers.remove("x-api-key");
        }
        // Static key path (unchanged behaviour): Authorization always, plus the
        // WorkBuddy `x-api-key` mirror.
        None => {
            if let Some(key) = api_key {
                if workbuddy {
                    headers.insert(
                        HeaderName::from_static("x-api-key"),
                        HeaderValue::from_str(key).unwrap_or_else(|_| HeaderValue::from_static("")),
                    );
                }
                headers.insert(
                    HeaderName::from_static("authorization"),
                    HeaderValue::from_str(&format!("Bearer {}", key))
                        .unwrap_or_else(|_| HeaderValue::from_static("")),
                );
            }
        }
    }

    headers
}
