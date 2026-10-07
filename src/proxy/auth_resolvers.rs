//! Caller identity: which API key a request presents.
//!
//! Split out of `proxy.rs`: these resolvers answer one question — what key did the
//! client supply (or which one does the config mandate) — and all three flavors read
//! them.

use super::*;

/// Resolve the API key for the Anthropic Messages API (`x-api-key` header).
pub(crate) fn resolve_api_key(config: &Config, headers: &HeaderMap) -> Option<String> {
    if config.passthrough_api_key {
        header_value(headers, "x-api-key")
    } else {
        config.api_key.clone()
    }
}

/// Resolve the API key for the Responses API (bearer token or `x-api-key`).
pub(crate) fn resolve_responses_api_key(config: &Config, headers: &HeaderMap) -> Option<String> {
    if config.passthrough_api_key {
        bearer_or_api_key(headers)
    } else {
        config.api_key.clone()
    }
}

/// Resolve the API key for Chat Completions, where the client-supplied key may
/// be used even when a static key is configured (the static key wins then).
pub(crate) fn resolve_chat_api_key(config: &Config, headers: &HeaderMap) -> Option<String> {
    let header_key = bearer_or_api_key(headers);
    if config.passthrough_api_key {
        header_key.or_else(|| config.api_key.clone())
    } else {
        config.api_key.clone().or(header_key)
    }
}

/// A single non-empty header value.
pub(crate) fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// Extract a bearer token from `authorization`, falling back to `x-api-key`.
pub(crate) fn bearer_or_api_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s))
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| header_value(headers, "x-api-key"))
}
