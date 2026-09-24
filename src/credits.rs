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

use crate::config::Config;
use crate::error::{ProxyError, ProxyResult};
use crate::settings::LogBuffer;
use axum::{
    http::HeaderMap,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::ModelsFlavor;

/// WorkBuddy/CodeBuddy billing host. The CLI chat endpoint (`copilot.tencent.com`)
/// serves inference; billing lives on the product site.
const WORKBUDDY_BILLING_HOST: &str = "https://www.codebuddy.cn";
const WORKBUDDY_RESOURCE_PATH: &str = "/v2/billing/meter/get-user-resource";
/// Product code used by the official balance console for CodeBuddy packages.
const WORKBUDDY_RESOURCE_PRODUCT_CODE: &str = "p_tcaca";

/// Fields that may carry the *remaining* credit amount of a package.
const REMAINING_FIELDS: &[&str] = &[
    "SlicePeriodCapacityRemainPrecise",
    "SlicePeriodCapacityRemain",
    "CycleCapacityRemainPrecise",
    "CycleCapacityRemain",
    "CapacityRemainPrecise",
    "CapacityRemain",
    "RemainPrecise",
    "Remain",
    "Remaining",
    "Balance",
];

/// Fields that may carry the *total* capacity of a package.
const TOTAL_FIELDS: &[&str] = &[
    "SlicePeriodCapacitySizePrecise",
    "SlicePeriodCapacitySize",
    "CycleCapacitySizePrecise",
    "CycleCapacitySize",
    "CycleCapacityPrecise",
    "CycleCapacity",
    "CapacityPrecise",
    "Capacity",
    "TotalCapacityPrecise",
    "TotalCapacity",
    "PackageCapacity",
    "Quota",
    "Amount",
];

/// Fields that may carry a package expiry timestamp.
const EXPIRY_FIELDS: &[&str] = &[
    "DeductionEndTime",
    "ExpiredTime",
    "SlicePeriodEndTime",
    "PackageEndTime",
    "EndTime",
    "CycleEndTime",
    "ExpireTime",
    "ExpirationTime",
    "ValidEndTime",
    "ValidPeriodEndTime",
    "EndAt",
    "ExpireAt",
];

/// Fields that may carry a human-readable package label.
const LABEL_FIELDS: &[&str] = &[
    "PackageName",
    "PackageTypeName",
    "AccountName",
    "ProductName",
    "Name",
    "RuleName",
    "Description",
];

/// Account-level summary remaining fields, used when aggregating the total
/// wallet balance (mirrors the upstream `CycleCapacityRemain*` priority).
const SUMMARY_REMAINING_FIELDS: &[&str] = &[
    "CycleCapacityRemainPrecise",
    "CycleCapacityRemain",
    "CapacityRemainPrecise",
    "CapacityRemain",
];

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

/// Resolve the balance endpoint URL for the active configuration.
///
/// Honors an explicit `CREDITS_API_ENDPOINT`, then falls back to the
/// WorkBuddy billing resource for the WorkBuddy flavor. Returns `None` when no
/// balance source is configured (the handler then returns 501).
fn credits_endpoint_for(config: &Config) -> Option<String> {
    if let Some(ref ep) = config.credits_endpoint {
        let trimmed = ep.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if config.models_flavor == ModelsFlavor::WorkBuddyConfig {
        return Some(format!(
            "{}{}",
            WORKBUDDY_BILLING_HOST, WORKBUDDY_RESOURCE_PATH
        ));
    }
    None
}

/// Body posted to the WorkBuddy resource endpoint (mirrors the web console).
fn workbuddy_resource_body() -> Value {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let fmt = "%Y-%m-%d %H:%M:%S";
    let begin = chrono_like(now, fmt);
    let end = chrono_like(now + 101 * 365 * 86_400, fmt);
    serde_json::json!({
        "PageNumber": 1,
        "PageSize": 100,
        "ProductCode": WORKBUDDY_RESOURCE_PRODUCT_CODE,
        "Status": [0, 3],
        "PackageEndTimeRangeBegin": begin,
        "PackageEndTimeRangeEnd": end,
    })
}

/// Format epoch seconds as a local "YYYY-MM-DD HH:MM:SS" string without
/// pulling in `chrono`. Good enough for the upstream date-range filter.
fn chrono_like(epoch: i64, _fmt: &str) -> String {
    let days = epoch / 86_400;
    let rem = epoch % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, m, s)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    crate::util::civil_from_days(z)
}

fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn first_number(item: &Value, fields: &[&str]) -> Option<f64> {
    for f in fields {
        let raw = match item.get(*f) {
            Some(v) => v,
            None => continue,
        };
        if raw.is_null() {
            continue;
        }
        if let Some(n) = raw.as_f64() {
            if n > 0.0 {
                return Some(n);
            }
        } else if let Some(s) = raw.as_str() {
            if let Ok(n) = s.trim().parse::<f64>() {
                if n > 0.0 {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn parse_ts(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => {
            let n = n.as_f64()?;
            if n >= 1e12 {
                Some(n / 1000.0)
            } else {
                Some(n)
            }
        }
        Value::String(s) => {
            let s = s.trim();
            if let Ok(n) = s.parse::<f64>() {
                return if n >= 1e12 { Some(n / 1000.0) } else { Some(n) };
            }
            // Try "YYYY-MM-DD HH:MM:SS"; otherwise give up.
            if s.len() >= 19 {
                parse_datetime(s)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn parse_datetime(s: &str) -> Option<f64> {
    let (date, time) = s.split_once(' ')?;
    let (y, rest) = date.split_once('-')?;
    let (mo, d) = rest.split_once('-')?;
    let (hh, rest) = time.split_once(':')?;
    let (mm, ss) = rest.split_once(':')?;
    let (y, mo, d) = (
        y.parse::<i64>().ok()?,
        mo.parse::<u32>().ok()?,
        d.parse::<u32>().ok()?,
    );
    let (hh, mm, ss) = (
        hh.parse::<i64>().ok()?,
        mm.parse::<i64>().ok()?,
        ss.parse::<i64>().ok()?,
    );
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss;
    Some(secs as f64)
}

fn days_from_civil(y: i64, mo: u32, d: u32) -> i64 {
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (367 * mo as u64 - 362) / 12 + (if mo <= 2 { 0 } else { 1 }) as u64 + d as u64
        - (if (mo == 2 && ((y % 4 == 0 && y % 100 != 0) || y % 400 == 0)) && d >= 29 {
            1
        } else {
            0
        });
    let moe = (yoe * 365 + yoe / 4 - yoe / 100) as i64 + doy as i64;
    era * 146097 + moe - 719468
}

fn first_text(item: &Value, fields: &[&str]) -> String {
    for f in fields {
        if let Some(s) = item.get(f).and_then(|v| v.as_str()) {
            let s = s.trim();
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

/// Walk `value`, yielding every object that looks like a credit package.
fn collect_accounts(value: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    // WorkBuddy wrapped accounts.
    if let Some(accounts) = value
        .pointer("/data/Response/Data/Accounts")
        .or_else(|| value.pointer("/data/data/Response/Data/Accounts"))
        .or_else(|| value.pointer("/data/accounts"))
        .and_then(|v| v.as_array())
    {
        out.extend(accounts.iter().cloned());
    }
    // Generic `accounts` array at the top level.
    if let Some(arr) = value.get("accounts").and_then(|v| v.as_array()) {
        out.extend(arr.iter().cloned());
    }
    out
}

/// Extract a single positive credit number from a generic (unknown) payload.
///
/// Tries explicit `credits`/`balance`/`total` fields, then any numeric value
/// plausibly in the [0, 1e12) range. Returns `(credits, label)`.
fn extract_generic_credits(value: &Value) -> (f64, String) {
    for key in [
        "credits",
        "balance",
        "total",
        "remain",
        "remaining",
        "amount",
    ] {
        if let Some(n) = value.get(key).and_then(|v| v.as_f64()) {
            if n > 0.0 {
                return (n, key.to_string());
            }
        }
    }
    // Fall back to the largest positive number we can find one level deep.
    let mut best: Option<(f64, String)> = None;
    if let Some(obj) = value.as_object() {
        for (k, v) in obj {
            if let Some(n) = v.as_f64() {
                if n > 0.0 && n < 1e12 {
                    let replace = match &best {
                        Some((b, _)) => n > *b,
                        None => true,
                    };
                    if replace {
                        best = Some((n, k.clone()));
                    }
                }
            }
        }
    }
    best.unwrap_or((0.0, String::new()))
}

/// Parse a WorkBuddy-style balance payload into segments.
fn parse_workbuddy(value: &Value) -> Vec<CreditSegment> {
    let mut segments = Vec::new();
    for account in collect_accounts(value) {
        // Expand slice-level usage details when present.
        let details = account
            .get("SlicePeriodUsageDetails")
            .and_then(|v| v.as_array());
        let items: Vec<Value> = if let Some(details) = details {
            details
                .iter()
                .filter(|d| d.is_object())
                .map(|d| {
                    let mut merged = account.clone();
                    if let Some(obj) = merged.as_object_mut() {
                        if let Some(d_obj) = d.as_object() {
                            for (k, v) in d_obj {
                                obj.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    merged
                })
                .collect()
        } else {
            vec![account]
        };

        for item in items {
            let remaining = match first_number(&item, REMAINING_FIELDS) {
                Some(r) => r,
                None => continue,
            };
            let total = first_number(&item, TOTAL_FIELDS).unwrap_or(remaining);
            let expires_at = EXPIRY_FIELDS
                .iter()
                .find_map(|f| item.get(f).and_then(parse_ts))
                .filter(|ts| *ts > now_epoch());
            let label = first_text(&item, LABEL_FIELDS);
            segments.push(CreditSegment {
                source: label,
                total: total.max(remaining),
                remaining,
                expires_at,
            });
        }
    }
    segments
}

fn merge_segments(mut segments: Vec<CreditSegment>) -> Vec<CreditSegment> {
    // Merge by (label, expires_at) so duplicate packages aggregate. We avoid
    // BTreeMap because `Option<f64>` is not `Ord` (f64 lacks total ordering).
    let mut merged: Vec<CreditSegment> = Vec::new();
    for s in segments.drain(..) {
        if s.remaining <= 0.0 {
            continue;
        }
        let key = (s.source.clone(), s.expires_at);
        if let Some(existing) = merged
            .iter_mut()
            .find(|e| (e.source.clone(), e.expires_at) == key)
        {
            existing.remaining += s.remaining;
            existing.total += s.total;
        } else {
            merged.push(s);
        }
    }
    let mut result = merged;
    for s in &mut result {
        s.remaining = (s.remaining * 100.0).round() / 100.0;
        s.total = (s.total * 100.0).round() / 100.0;
    }
    // Unknown expiries sort last.
    result.sort_by(|a, b| match (a.expires_at, b.expires_at) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
    });
    result
}

fn soonest_expiry(segments: &[CreditSegment]) -> Option<f64> {
    let now = now_epoch();
    segments
        .iter()
        .filter_map(|s| s.expires_at)
        .filter(|ts| *ts > now)
        .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
}

/// Fetch and normalize the gateway wallet balance.
pub async fn fetch_credits(
    config: &Config,
    client: &reqwest::Client,
    api_key: &Option<String>,
) -> ProxyResult<CreditsResponse> {
    let endpoint = credits_endpoint_for(config).ok_or_else(|| {
        ProxyError::Config(
            "no credits endpoint configured (set CREDITS_API_ENDPOINT, or use the WorkBuddy flavor)".to_string(),
        )
    })?;

    let key = api_key.clone().ok_or_else(|| {
        ProxyError::Config("UPSTREAM_API_KEY is required to query gateway credits".to_string())
    })?;

    let is_workbuddy = endpoint.ends_with(WORKBUDDY_RESOURCE_PATH);

    let mut req = client
        .post(&endpoint)
        .timeout(Duration::from_secs(30))
        .header("Authorization", format!("Bearer {}", key))
        .header("X-API-Key", &key)
        .header("Accept", "application/json");
    if is_workbuddy {
        req = req
            .header("Content-Type", "application/json")
            .header("X-Client-Platform", "web")
            .header("Origin", WORKBUDDY_BILLING_HOST)
            .header("Referer", format!("{}/profile/plans-usage", WORKBUDDY_BILLING_HOST))
            .header(
                "User-Agent",
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36",
            )
            .json(&workbuddy_resource_body());
    } else {
        // Generic endpoint: probe with an empty JSON body; many gateways accept it.
        req = req
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({}));
    }

    // Upstream failures are reported *inside* a 200 response (mirrors the
    // reference "don't break the caller" contract for balance side-info).
    let send = req.send().await;
    let resp = match send {
        Ok(r) => r,
        Err(e) => {
            return Ok(error_response(
                &key,
                format!("credits request failed: {}", e),
            ))
        }
    };
    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Ok(error_response(
            &key,
            format!(
                "credits endpoint returned {}: {}",
                status,
                truncate(&body_text, 400)
            ),
        ));
    }

    let value: Value = match serde_json::from_str(&body_text) {
        Ok(v) => v,
        Err(e) => {
            return Ok(error_response(
                &key,
                format!("credits endpoint returned invalid JSON: {}", e),
            ))
        }
    };

    // Surface upstream business errors (code != 0).
    if let Some(code) = value.get("code").and_then(|c| c.as_i64()) {
        if code != 0 {
            let msg = value
                .get("msg")
                .or_else(|| value.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown business error");
            return Ok(error_response(
                &key,
                format!("credits endpoint business error code {}: {}", code, msg),
            ));
        }
    }

    let (segments, label) = if is_workbuddy {
        (parse_workbuddy(&value), "workbuddy".to_string())
    } else if !collect_accounts(&value).is_empty() {
        (parse_workbuddy(&value), "generic".to_string())
    } else {
        let (credits, label) = extract_generic_credits(&value);
        (
            vec![CreditSegment {
                source: label,
                total: credits,
                remaining: credits,
                expires_at: None,
            }],
            "generic".to_string(),
        )
    };

    let segments = merge_segments(segments);
    // For WorkBuddy, aggregate the *summary* remaining fields at the account
    // level (CycleCapacityRemain*), matching the upstream console's total.
    let credits: f64 = if is_workbuddy {
        let accounts = collect_accounts(&value);
        let mut sum = 0.0_f64;
        for account in &accounts {
            if let Some(r) = first_number(account, SUMMARY_REMAINING_FIELDS) {
                sum += r;
            }
        }
        if sum > 0.0 {
            sum
        } else {
            segments.iter().map(|s| s.remaining).sum()
        }
    } else {
        segments.iter().map(|s| s.remaining).sum()
    };
    let soonest = soonest_expiry(&segments);
    let total_dosage = value
        .pointer("/data/Response/Data/TotalDosage")
        .or_else(|| value.pointer("/data/data/Response/Data/TotalDosage"))
        .and_then(|v| v.as_f64());

    Ok(CreditsResponse {
        credits: (credits * 100.0).round() / 100.0,
        provider: label,
        segments,
        soonest_expiry: soonest,
        total_dosage,
        fetched_at: now_epoch(),
        partial: false,
        credit_error: None,
        raw: value,
    })
}

/// Build a 200 response carrying a `credit_error` message instead of hard-failing
/// (balance is side-information; the caller must keep working without it).
fn error_response(_key: &str, message: String) -> CreditsResponse {
    CreditsResponse {
        credits: 0.0,
        provider: String::new(),
        segments: Vec::new(),
        soonest_expiry: None,
        total_dosage: None,
        fetched_at: now_epoch(),
        partial: false,
        credit_error: Some(message),
        raw: Value::Null,
    }
}

fn truncate(s: &str, max: usize) -> String {
    crate::util::truncate(s, max)
}

/// axum handler for `GET /v1/credits`.
pub async fn credits_handler(
    Extension(config): Extension<Arc<Config>>,
    Extension(client): Extension<reqwest::Client>,
    Extension(gui_logs): Extension<Arc<LogBuffer>>,
    Extension(service): Extension<Arc<crate::service::ServiceController>>,
    headers: HeaderMap,
) -> ProxyResult<Response> {
    // Route through the shared redactor: this line is mirrored to
    // `~/.proxy-rs/logs/proxy.log`, and callers reach `/v1/credits` with their
    // own `authorization` / `x-api-key` header attached. Hand-rolling the
    // header loop here wrote those values out in the clear.
    let incoming = crate::util::format_headers(&headers);

    gui_logs
        .push("INFO", format!("GET /v1/credits headers: {}", incoming))
        .await;
    tracing::info!("GET /v1/credits headers: {}", incoming);

    if !service.is_running() {
        return Ok(crate::service::service_unavailable_response());
    }

    let api_key = config.api_key.clone();
    let result = fetch_credits(&config, &client, &api_key).await;

    match result {
        Ok(resp) => {
            if let Some(ref err) = resp.credit_error {
                gui_logs
                    .push("WARN", format!("GET /v1/credits unavailable: {}", err))
                    .await;
            } else {
                gui_logs
                    .push(
                        "INFO",
                        format!(
                            "GET /v1/credits ok credits={} segments={}",
                            resp.credits,
                            resp.segments.len()
                        ),
                    )
                    .await;
            }
            Ok(Json(resp).into_response())
        }
        Err(e) => {
            gui_logs
                .push("ERROR", format!("GET /v1/credits failed | {}", e))
                .await;
            Err(e)
        }
    }
}

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
        let config = Config {
            models_flavor: ModelsFlavor::OpenAI,
            credits_endpoint: None,
            ..Default::default()
        };
        let client = reqwest::Client::new();
        let result = fetch_credits(&config, &client, &Some("token".to_string())).await;
        assert!(matches!(result, Err(ProxyError::Config(_))));
    }

    #[tokio::test]
    async fn missing_api_key_returns_error() {
        let config = Config {
            models_flavor: ModelsFlavor::OpenAI,
            credits_endpoint: Some("https://example.invalid/credits".to_string()),
            ..Default::default()
        };
        let client = reqwest::Client::new();
        let result = fetch_credits(&config, &client, &None).await;
        assert!(matches!(result, Err(ProxyError::Config(_))));
    }
}
