//! Gateway wallet balance (`GET /v1/credits`).
//!
//! Returns the upstream gateway's wallet balance, normalized to a single
//! `credits` figure plus per-package segment detail. The endpoint is
//! configurable:
//!
//! * `CREDITS_API_ENDPOINT` selects any gateway balance endpoint. When set,
//!   the proxy POSTs to it with `Authorization: Bearer <UPSTREAM_API_KEY>` and
//!   parses the first `credits`/`balance`/`total` number it finds.
//! * When unset and the active provider is the WorkBuddy/CodeBuddy flavor, the
//!   official billing resource endpoint is used automatically:
//!   `POST https://www.codebuddy.cn/v2/billing/meter/get-user-resource` with the
//!   `{ProductCode, Status, PackageEndTimeRange*}` body the web console sends.
//!
//! The response always exposes the *raw* upstream credits (no currency
//! conversion), matching the chosen `GET /v1/credits` contract.
//! ## File layout
//!
//! Split from one 753-line file into parsing, transport and the handler. The
//! boundary that matters is **parse.rs has no I/O**: the vendor payload shapes
//! change most often, and keeping every extractor pure means they can be pinned
//! with fixed JSON without a network or a clock.
//!
//! | file | owns |
//! |---|---|
//! | [`fields`] | the field-name tables payloads are read through |
//! | [`shapes`] | serialized response shapes |
//! | [`parse`] | pure parsing: timestamps, segment merge, per-vendor extractors |
//! | [`fetch`] | the outbound balance request and its error mapping |
//! | [`handler`] | the `/v1/credits` HTTP handler |

pub(crate) use crate::error::{ProxyError, ProxyResult};
pub(crate) use crate::settings::LogBuffer;
pub(crate) use axum::{
    http::HeaderMap,
    response::{IntoResponse, Response},
    Extension, Json,
};
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use serde_json::Value;
pub(crate) use std::sync::Arc;
pub(crate) use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) use crate::flavor::ModelsFlavor;

mod fetch;
mod fields;
mod handler;
mod parse;
mod shapes;

pub use fetch::*;
pub use fields::*;
pub use handler::*;
pub use parse::*;
pub use shapes::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_workbuddy_accounts() {
        let payload = serde_json::json!({
            "code": 0,
            "data": {
                "Response": {
                    "Data": {
                        "Accounts": [
                            {
                                "PackageName": "Pro 5000",
                                "SlicePeriodCapacityRemainPrecise": 1234.5,
                                "SlicePeriodCapacitySizePrecise": 5000.0,
                                "PackageEndTime": "2026-12-31 23:59:59"
                            },
                            {
                                "PackageName": "Free 100",
                                "Remain": 50.0,
                                "TotalCapacity": 100.0
                            }
                        ]
                    }
                }
            }
        });
        let segments = merge_segments(parse_workbuddy(&payload));
        let credits: f64 = segments.iter().map(|s| s.remaining).sum();
        assert_eq!(credits, 1284.5);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].source, "Pro 5000");
        assert_eq!(segments[0].total, 5000.0);
        assert!(segments[0].expires_at.is_some());
    }

    #[test]
    fn parses_generic_credits_field() {
        let payload = serde_json::json!({ "credits": 777.0, "currency": "CNY" });
        let (credits, label) = extract_generic_credits(&payload);
        assert_eq!(credits, 777.0);
        assert_eq!(label, "credits");
    }

    #[tokio::test]
    async fn no_endpoint_configured_returns_error() {
        // OpenAI flavor with no CREDITS_API_ENDPOINT must error before any network call.
        let client = reqwest::Client::new();
        let result = fetch_credits(
            &None,
            ModelsFlavor::OpenAI,
            &client,
            &Some("token".to_string()),
            CreditsIdentity::ApiKey,
        )
        .await;
        assert!(matches!(result, Err(ProxyError::Config(_))));
    }

    #[tokio::test]
    async fn missing_api_key_returns_error() {
        let client = reqwest::Client::new();
        let endpoint = Some("https://example.invalid/credits".to_string());
        let result = fetch_credits(
            &endpoint,
            ModelsFlavor::OpenAI,
            &client,
            &None,
            CreditsIdentity::ApiKey,
        )
        .await;
        assert!(matches!(result, Err(ProxyError::Config(_))));
    }
}
