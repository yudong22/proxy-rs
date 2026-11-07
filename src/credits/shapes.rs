//! Serialized response shapes.

use super::*;

/// One normalized credit segment (a grant / package with remaining capacity).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditSegment {
    /// Human-readable package label, if known.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// Total capacity of the package.
    pub total: f64,
    /// Remaining (usable) capacity.
    pub remaining: f64,
    /// Epoch seconds when the package expires; `None` when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<f64>,
}

/// Normalized balance response returned by `GET /v1/credits`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditsResponse {
    /// Sum of all segment `remaining` values (the usable wallet balance).
    pub credits: f64,
    /// Provider/profile the balance was fetched from.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// Per-package detail, sorted by soonest expiry.
    #[serde(default)]
    pub segments: Vec<CreditSegment>,
    /// Earliest known expiry across segments, epoch seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soonest_expiry: Option<f64>,
    /// Upstream `TotalDosage` (cumulative consumption), if reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_dosage: Option<f64>,
    /// Epoch seconds when this snapshot was produced.
    pub fetched_at: f64,
    /// True when the upstream response looked incomplete (e.g. pagination cap).
    #[serde(default)]
    pub partial: bool,
    /// Friendly error message when the upstream query failed but the proxy
    /// still answered (mirrors the upstream "don't break the caller" contract).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit_error: Option<String>,
    /// Raw upstream payload, kept for debugging proxy consumers.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub raw: Value,
}

/// Which identity the `GET /v1/credits` request authenticates with.
///
/// The two are not interchangeable, and sending the wrong pairing is a 401: the
/// billing gateway reads `X-API-Key` as authoritative when it is present, so a
/// Keycloak login token placed there answers `401 {"message":"not_found"}` even
/// though the same token as a bare bearer works. Verified against the live
/// endpoint (2026-09-28):
///
///   * login token, `Bearer` only                            → 200 `code 0`
///   * login token, `Bearer` + `X-API-Key`                   → 401 not_found
///   * static key, `Bearer` + `X-API-Key`                    → 200 `code 0`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditsIdentity {
    /// A static gateway API key: `Authorization` **and** `X-API-Key`.
    ApiKey,
    /// A WorkBuddy login-state access token: `Authorization` **only**.
    LoginState,
}
