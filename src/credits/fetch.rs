//! The outbound balance request and its error mapping.

use super::*;

/// Resolve the balance endpoint URL for the active configuration.
///
/// Honors an explicit `CREDITS_API_ENDPOINT`, then falls back to the
/// WorkBuddy billing resource for the WorkBuddy flavor. Returns `None` when no
/// balance source is configured (the handler then returns 501).
pub(crate) fn credits_endpoint_for(
    credits_endpoint: &Option<String>,
    flavor: ModelsFlavor,
) -> Option<String> {
    if let Some(ref ep) = credits_endpoint {
        let trimmed = ep.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if flavor == ModelsFlavor::WorkBuddyConfig {
        return Some(format!(
            "{}{}",
            WORKBUDDY_BILLING_HOST, WORKBUDDY_RESOURCE_PATH
        ));
    }
    None
}

/// Body posted to the WorkBuddy resource endpoint (mirrors the web console).
pub(crate) fn workbuddy_resource_body() -> Value {
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

/// Fetch and normalize the gateway wallet balance.
pub async fn fetch_credits(
    credits_endpoint: &Option<String>,
    flavor: ModelsFlavor,
    client: &reqwest::Client,
    api_key: &Option<String>,
    identity: CreditsIdentity,
) -> ProxyResult<CreditsResponse> {
    let endpoint = credits_endpoint_for(credits_endpoint, flavor).ok_or_else(|| {
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
        .header("Accept", "application/json");
    // A login token must not appear as an api key; see `CreditsIdentity`.
    if identity == CreditsIdentity::ApiKey {
        req = req.header("X-API-Key", &key);
    }
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
pub(crate) fn error_response(_key: &str, message: String) -> CreditsResponse {
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

pub(crate) fn truncate(s: &str, max: usize) -> String {
    crate::util::truncate(s, max)
}
