//! See the module table in `workbuddy_auth/mod.rs` for what lives here.

use super::*;

/// Default domestic WorkBuddy endpoint authority.
pub const DEFAULT_WORKBUDDY_ENDPOINT: &str = "https://copilot.tencent.com";

/// Pending response code returned by `/v2/plugin/auth/token` while user has not yet authorized.
pub const LOGIN_TOKEN_PENDING_CODE: i64 = 11217;

/// Initial state returned by `start_oauth_flow`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthStateResponse {
    pub state: String,
    pub auth_url: String,
    pub qr_svg: String,
}

/// Status of an OAuth polling check.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum OAuthPollResult {
    Pending,
    // Boxed: `WorkBuddyCredential` dwarfs the other variants, and this enum is
    // carried across an await per poll, so the size difference is real churn.
    Success {
        credential: Box<WorkBuddyCredential>,
    },
    Failed {
        error: String,
    },
}

/// Generate SVG vector string for any URL / text string.
pub fn generate_qr_svg(content: &str) -> Result<String> {
    use qrcode::render::svg;
    use qrcode::QrCode;

    let code =
        QrCode::new(content.as_bytes()).map_err(|e| anyhow::anyhow!("生成二维码失败: {}", e))?;
    let svg = code
        .render::<svg::Color>()
        .min_dimensions(220, 220)
        .dark_color(svg::Color("#18181b"))
        .light_color(svg::Color("#ffffff"))
        .build();
    Ok(svg)
}

/// Start an OAuth authorization session, generating state and QR code SVG.
pub async fn start_oauth_flow(
    client: &reqwest::Client,
    endpoint: Option<&str>,
) -> Result<OAuthStateResponse> {
    let base = endpoint
        .unwrap_or(DEFAULT_WORKBUDDY_ENDPOINT)
        .trim_end_matches('/');
    let url = format!("{}/v2/plugin/auth/state?platform=desktop", base);

    let resp = client
        .post(&url)
        .timeout(std::time::Duration::from_secs(15))
        .header("User-Agent", crate::providers::WORKBUDDY_USER_AGENT)
        .header("X-Product-Code", "codebuddy")
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("请求授权服务失败: {}", e))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "授权端点返回 {}: {}",
            status,
            crate::util::truncate(&body, 300)
        );
    }

    let val: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("解析响应失败: {}", e))?;
    let code = val.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = val
            .get("msg")
            .and_then(|m| m.as_str())
            .unwrap_or("未知错误");
        anyhow::bail!("获取授权状态失败: {}", msg);
    }

    let data = val
        .get("data")
        .ok_or_else(|| anyhow::anyhow!("响应缺少 data 字段"))?;
    let state = data
        .get("state")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    let auth_url = data
        .get("authUrl")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();

    if state.is_empty() || auth_url.is_empty() {
        anyhow::bail!("授权返回缺少 state 或 authUrl");
    }

    let qr_svg = generate_qr_svg(&auth_url)?;
    Ok(OAuthStateResponse {
        state,
        auth_url,
        qr_svg,
    })
}

/// Poll the OAuth token endpoint. When authorized, creates and persists the credential.
pub async fn poll_oauth_token(
    client: &reqwest::Client,
    endpoint: Option<&str>,
    state: &str,
) -> Result<OAuthPollResult> {
    let base = endpoint
        .unwrap_or(DEFAULT_WORKBUDDY_ENDPOINT)
        .trim_end_matches('/');
    let url = format!("{}/v2/plugin/auth/token?state={}", base, state);

    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .header("User-Agent", crate::providers::WORKBUDDY_USER_AGENT)
        .header("X-Product-Code", "codebuddy")
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("轮询授权状态失败: {}", e))?;

    let body = resp.text().await.unwrap_or_default();

    match parse_oauth_token_response(&body, crate::util::unix_millis()) {
        OAuthTokenGrant::Pending => Ok(OAuthPollResult::Pending),
        OAuthTokenGrant::Failed(error) => Ok(OAuthPollResult::Failed { error }),
        OAuthTokenGrant::Granted(grant) => finish_oauth_login(client, base, grant).await,
    }
}

/// One poll's decoded outcome, before any further network or disk side effect.
///
/// Split out of [`poll_oauth_token`] so the envelope rules — which code means
/// "keep waiting", how a rejection is described, and how expiry is expressed —
/// are testable without an HTTP client or a data directory.
pub(crate) enum OAuthTokenGrant {
    Pending,
    Granted(TokenGrant),
    Failed(String),
}

/// The token fields a successful poll yields.
pub(crate) struct TokenGrant {
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
    pub(crate) expires_at_ms: Option<i64>,
    pub(crate) domain: String,
}

/// Decode the `/v2/plugin/auth/token` envelope.
///
/// `now_ms` is passed in rather than read here so the relative `expiresIn`
/// conversion is deterministic under test.
///
/// Every rejection carries the upstream's own `code` and `msg`. Reporting a
/// bare sentence instead is what made a stuck QR login impossible to diagnose
/// after the fact: the GUI only ever showed "授权失败: 授权失败".
pub(crate) fn parse_oauth_token_response(body: &str, now_ms: i64) -> OAuthTokenGrant {
    let val: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            return OAuthTokenGrant::Failed(format!(
                "授权响应不是有效 JSON ({}): {}",
                e,
                crate::util::truncate(body, 200)
            ))
        }
    };

    let code = val.get("code").and_then(serde_json::Value::as_i64);
    if code == Some(LOGIN_TOKEN_PENDING_CODE) {
        return OAuthTokenGrant::Pending;
    }
    if code != Some(0) {
        // A missing `code` stays distinct from `code == 0`: treating an absent
        // field as success would report a malformed body as an authorized
        // login and then fail later on the missing token.
        return OAuthTokenGrant::Failed(format!(
            "上游拒绝授权 (code={}): {}",
            code.map(|c| c.to_string()).unwrap_or_else(|| "缺失".into()),
            val.get("msg")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("无 msg")
        ));
    }

    let data = match val.get("data") {
        Some(d) if d.is_object() => d,
        _ => {
            return OAuthTokenGrant::Failed(format!(
                "授权响应缺少 data: {}",
                crate::util::truncate(body, 200)
            ))
        }
    };

    let access_token = data
        .get("accessToken")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    if access_token.is_empty() {
        return OAuthTokenGrant::Failed(format!(
            "授权响应缺少 accessToken: {}",
            crate::util::truncate(body, 200)
        ));
    }

    OAuthTokenGrant::Granted(TokenGrant {
        access_token,
        refresh_token: data
            .get("refreshToken")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        expires_at_ms: expires_at_ms_from(data, now_ms),
        domain: data
            .get("domain")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("copilot.tencent.com")
            .to_string(),
    })
}

/// Absolute access-token expiry in epoch millis from either field shape.
///
/// The OAuth endpoints answer with **`expiresIn` in seconds**
/// (`"expiresIn": 2419198`), while the desktop client's persisted login state
/// uses `expiresAt` in millis. Reading only `expiresAt` made every scanned
/// account look like it had no known expiry, which silently demoted it to the
/// conservative unknown-expiry backstop ([`UNKNOWN_EXPIRY_REFRESH_AFTER_MS`]):
/// a token that really lives ~28 days was treated as unrenewable-until-6h and
/// its true `exp` was never recorded. Preferring the absolute field keeps
/// imported states exact; the relative one covers the OAuth answer.
pub(crate) fn expires_at_ms_from(data: &serde_json::Value, now_ms: i64) -> Option<i64> {
    if let Some(exp) = data.get("expiresAt").and_then(serde_json::Value::as_i64) {
        return Some(exp);
    }
    let secs = data
        .get("expiresIn")
        .and_then(serde_json::Value::as_i64)
        .filter(|s| *s > 0)?;
    Some(now_ms.saturating_add(secs.saturating_mul(1000)))
}

/// Turn a granted token into a stored credential, fetching its profile first.
async fn finish_oauth_login(
    client: &reqwest::Client,
    base: &str,
    grant: TokenGrant,
) -> Result<OAuthPollResult> {
    let TokenGrant {
        access_token,
        refresh_token,
        expires_at_ms,
        domain,
    } = grant;

    // Fetch account profile for nickname / uid / enterpriseId
    let account = fetch_account(client, base, &access_token)
        .await
        .unwrap_or_default();
    let label = if !account.nickname.is_empty() {
        account.nickname.clone()
    } else {
        "扫码账号".to_string()
    };

    let cred = WorkBuddyCredential {
        id: credential_id_for(&account, &access_token),
        label,
        access_token,
        refresh_token,
        expires_at_ms,
        domain,
        account,
        machine_id: String::new(),
        enabled: true,
        cooldown_until_ms: None,
        last_error: String::new(),
        // A brand-new login is by definition not rejected.
        relogin_required_since_ms: None,
        points: None,
        points_fetched_at_ms: None,
        last_checkin_date: None,
        // Just minted by the OAuth flow, so the unknown-expiry backstop starts
        // counting from now.
        last_refresh_at_ms: Some(crate::util::unix_millis()),
    };

    upsert_credential(cred.clone())?;
    Ok(OAuthPollResult::Success {
        credential: Box::new(cred),
    })
}
