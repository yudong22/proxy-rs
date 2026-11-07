//! See the module table in `workbuddy_auth/mod.rs` for what lives here.

use super::*;

/// Store the queried points for one credential.
pub fn set_points(id: &str, points: Option<f64>) -> Result<()> {
    let mut items = load_credentials();
    let Some(c) = items.iter_mut().find(|c| c.id == id) else {
        return Ok(());
    };
    c.points = points;
    c.points_fetched_at_ms = Some(crate::util::unix_millis());
    save_credentials(&items)
}

/// Fetch user profile from `/v2/plugin/account`.
pub async fn fetch_account(
    client: &reqwest::Client,
    endpoint: &str,
    access_token: &str,
) -> Result<WorkBuddyAccount> {
    let url = format!("{}/v2/plugin/account", endpoint.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .header("User-Agent", crate::providers::WORKBUDDY_USER_AGENT)
        .header("X-Product-Code", "codebuddy")
        .header("Authorization", format!("Bearer {}", access_token))
        .send()
        .await?;

    let body = resp.text().await.unwrap_or_default();
    let val: serde_json::Value = serde_json::from_str(&body)?;
    let data = val.get("data").cloned().unwrap_or(val);
    let account: WorkBuddyAccount = serde_json::from_value(data).unwrap_or_default();
    Ok(account)
}

/// The headers the billing resource endpoint requires for an account query.
///
/// Mirrors `wb_accounts.py::headers(purpose="billing")` in
/// workbuddy2api-hub, which is the reference that proves this endpoint answers
/// a login-state token: the billing gateway does **not** accept the bare
/// `Bearer`+`X-API-Key` pairing a chat request uses — it wants the account's
/// own identity headers (`X-User-Id`, `X-Domain`, `X-Enterprise-Id`…) present,
/// or it answers 401 and the GUI shows "—" for a balance that exists.
///
/// `upstream_headers` supplies those identity headers from the same source the
/// chat path uses, so both share one account fingerprint.
pub(crate) fn billing_headers(cred: &WorkBuddyCredential) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        (
            "Accept".to_string(),
            "application/json, text/plain, */*".to_string(),
        ),
        ("X-Requested-With".to_string(), "XMLHttpRequest".to_string()),
        ("Origin".to_string(), WORKBUDDY_BILLING_HOST.to_string()),
        (
            "Referer".to_string(),
            format!("{}/", WORKBUDDY_BILLING_HOST),
        ),
        (
            "Authorization".to_string(),
            format!("Bearer {}", cred.access_token),
        ),
        ("X-CodeBuddy-Request".to_string(), "1".to_string()),
        ("Accept-Language".to_string(), "zh-CN".to_string()),
    ];

    // Per-account fingerprint, identical to the chat path's: X-User-Id,
    // X-Machine-Id, X-Domain and the enterprise/tenant pair. The billing
    // gateway answers 401 without them even though the token itself is valid —
    // verified against the live endpoint (401 bare → 200 with headers).
    for (name, value) in upstream_headers(cred) {
        headers.push((name, value));
    }

    // A header the endpoint accepts in place of a tenant id.
    if cred.account.enterprise_id.is_empty() {
        headers.push(("X-No-Enterprise-Id".to_string(), "1".to_string()));
    }

    headers
}

/// Query the remaining points (积分) for one WorkBuddy credential.
///
/// Hits the same billing resource endpoint `credits.rs` uses, but authorised
/// with the credential's own access token and its account fingerprint, so each
/// account reports its own balance. Returns `None` when the upstream cannot
/// answer — the UI shows "—" rather than a misleading zero.
pub async fn fetch_points(client: &reqwest::Client, cred: &WorkBuddyCredential) -> Option<f64> {
    if cred.access_token.is_empty() {
        return None;
    }
    let url = format!(
        "{}{}",
        WORKBUDDY_BILLING_HOST,
        crate::credits::WORKBUDDY_RESOURCE_PATH
    );

    let mut req = client
        .post(&url)
        .timeout(std::time::Duration::from_secs(20));
    for (name, value) in billing_headers(cred) {
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(&value),
        ) {
            req = req.header(n, v);
        }
    }

    let resp = req
        .json(&serde_json::json!({
            "PageNumber": 1,
            "PageSize": 100,
            "ProductCode": crate::credits::WORKBUDDY_RESOURCE_PRODUCT_CODE,
            "Status": [0, 3],
        }))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        tracing::warn!(
            "points query for {} returned {}: balance shown as unknown",
            cred.id,
            resp.status()
        );
        return None;
    }
    let body = resp.text().await.ok()?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;

    // A business error is not a balance; reporting 0 would read as exhausted.
    if let Some(code) = value.get("code").and_then(|c| c.as_i64()) {
        if code != 0 {
            tracing::warn!(
                "points query for {} returned business code {}: balance shown as unknown",
                cred.id,
                code
            );
            return None;
        }
    }
    Some(crate::credits::remaining_from_response(&value))
}

/// The WorkBuddy billing host, shared with `credits.rs`.
pub(crate) const WORKBUDDY_BILLING_HOST: &str = "https://www.codebuddy.cn";

/// Refresh stored points for every enabled credential **and every enabled API
/// key**, returning a report covering both pools.
///
/// Each identity is queried once and persisted, so the GUI can show a balance
/// immediately on load and so a failover decision can prefer the identity with
/// the most remaining points without a network call on the request path.
///
/// Keys were previously skipped entirely — `set_api_key_points` had no caller —
/// which is why the overview's 剩余积分 card showed "—" whenever the identity in
/// force was a key: the value was never fetched, not genuinely unknown.
pub async fn refresh_all_points(client: &reqwest::Client) -> PointsReport {
    let mut results = Vec::new();

    for cred in load_credentials().into_iter().filter(|c| c.enabled) {
        results.push(refresh_one_credential_points(client, &cred).await);
    }

    // API keys hit the same billing endpoint with the raw key material, so their
    // balances are readable too — previously they were skipped entirely, which is
    // why the overview card read "—" whenever the identity in force was a key.
    // Same persist-on-failure rule: a dead key clears its figure.
    for key in load_api_keys().into_iter().filter(|k| k.enabled) {
        results.push(refresh_one_key_points(client, &key).await);
    }

    PointsReport { results }
}

/// Query and persist one credential's balance.
async fn refresh_one_credential_points(
    client: &reqwest::Client,
    cred: &WorkBuddyCredential,
) -> PointsResult {
    let points = fetch_points(client, cred).await;
    // Persist even on failure: a failed refresh clears a stale figure rather than
    // leaving a number the account no longer has.
    let _ = set_points(&cred.id, points);
    PointsResult {
        id: cred.id.clone(),
        label: display_label(cred),
        points,
    }
}

/// One account's refreshed point balance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PointsResult {
    pub id: String,
    pub label: String,
    /// `None` when the upstream could not be read.
    pub points: Option<f64>,
}

/// Refresh the point balance of a **single** identity, by id.
///
/// The per-row 刷新积分 button: without this it called the pool-wide refresh and
/// silently updated every account, so the button did not do what its own tooltip
/// said — and a single unreachable account could not be retried on its own.
///
/// Resolves the id against the credential pool first, then the key pool: the two
/// pools are one list in the GUI, and their ids are already disjoint (`wb-…` vs
/// `k-…`). Same persist-on-failure rule as the pool-wide path: a failed read
/// clears the stale figure rather than leaving a number the account no longer
/// has.
///
/// Returns `None` when no identity with that id exists, so the caller can say
/// "账号不存在" instead of reporting a fictional success.
pub async fn refresh_one_points(client: &reqwest::Client, id: &str) -> Option<PointsResult> {
    if let Some(cred) = load_credentials().into_iter().find(|c| c.id == id) {
        return Some(refresh_one_credential_points(client, &cred).await);
    }

    if let Some(key) = load_api_keys().into_iter().find(|k| k.id == id) {
        return Some(refresh_one_key_points(client, &key).await);
    }

    None
}

/// Result of a pool-wide points refresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PointsReport {
    pub results: Vec<PointsResult>,
}

/// The name to show for a credential: explicit label, then nickname, then id.
pub fn display_label(cred: &WorkBuddyCredential) -> String {
    if !cred.label.is_empty() {
        cred.label.clone()
    } else if !cred.account.nickname.is_empty() {
        cred.account.nickname.clone()
    } else {
        cred.id.clone()
    }
}

// ── Daily Check-in & Points Claiming ───────────────────────────────────────
