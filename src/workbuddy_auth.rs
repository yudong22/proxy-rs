//! WorkBuddy / CodeBuddy login-state credential store.
//!
//! A "credential" here is the desktop client's login state — the same JSON the
//! WorkBuddy desktop app (or a workbuddy2api session file) persists:
//! `{auth: {accessToken, refreshToken, expiresAt, domain}, account: {uid,
//! enterpriseId, …}}`. Presenting it reproduces the genuine client's upstream
//! fingerprint, which the plain `x-api-key` mode cannot: the gateway answers
//! `11128 Illegal API invocation from an unapproved channel` to requests that
//! lack the session-bound headers (`X-User-Id`, `X-Enterprise-Id`, …).
//!
//! The store lives in `~/.proxy-rs/workbuddy-credentials.json` (mode 0600, the
//! same protection workbuddy2api gives its session file) and follows the
//! [`crate::settings::data_dir`] relocation, so a dev instance never touches the
//! installed app's credentials.
//!
//! Layer note: like `credits.rs`, this is Layer 3 with I/O — it is *not* part of
//! the pure-function translate core.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One WorkBuddy login-state credential.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkBuddyCredential {
    /// Stable short id used in logs and the request-log `override_key` column
    /// (`wb-<6 hex>`). Assigned once at creation, never regenerated.
    pub id: String,
    /// User-facing label ("工作号"). Free-form.
    #[serde(default)]
    pub label: String,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Access-token expiry, milliseconds since the Unix epoch, as the desktop
    /// client stores it. `None` means "no known expiry" — treat as valid.
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
    /// `X-Domain` header value (the token's `domain` field, or the endpoint
    /// authority). Empty when unknown.
    #[serde(default)]
    pub domain: String,
    /// Account info extracted from the login state (`uid`, `enterpriseId`, …).
    #[serde(default)]
    pub account: WorkBuddyAccount,
    /// Stable `X-Machine-Id`; kept from an imported session when present so the
    /// upstream sees the same device it issued the token to.
    #[serde(default)]
    pub machine_id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Milliseconds-since-epoch until which this credential is cooling down
    /// after a rate/limit exception. Empty when healthy.
    #[serde(default)]
    pub cooldown_until_ms: Option<i64>,
    /// Last failure description, shown in the GUI credential list.
    #[serde(default)]
    pub last_error: String,
}

fn default_true() -> bool {
    true
}

/// The account block of a login state. Only the fields the upstream headers
/// need are kept; everything else in the desktop file is ignored. The desktop
/// file spells the fields camelCase (`enterpriseId`), so the aliases matter.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkBuddyAccount {
    #[serde(default)]
    pub uid: String,
    #[serde(default, alias = "enterprise_id")]
    pub enterprise_id: String,
    #[serde(default)]
    pub nickname: String,
}

impl WorkBuddyCredential {
    /// Whether the access token is still believed valid: not expired (with a
    /// 60-second margin, matching the reference client's renewal threshold)
    /// and not in a rate-limit cooldown.
    pub fn is_usable(&self, now_ms: i64) -> bool {
        if !self.enabled {
            return false;
        }
        if let Some(until) = self.cooldown_until_ms {
            if now_ms < until {
                return false;
            }
        }
        match self.expires_at_ms {
            Some(exp) => now_ms < exp - 60_000,
            None => true,
        }
    }

    /// The Authorization bearer value. Only the access token ever appears on
    /// the wire; refresh tokens are used exclusively against the refresh
    /// endpoint.
    pub fn bearer(&self) -> &str {
        &self.access_token
    }

    /// Masked display form for the GUI (`sk-…abcd` style): never emit the full
    /// token back to the UI layer.
    pub fn masked_token(&self) -> String {
        let t = &self.access_token;
        if t.len() <= 8 {
            "••••".to_string()
        } else {
            format!("{}••••{}", &t[..4], &t[t.len() - 4..])
        }
    }
}

/// Derive a stable short credential id from the token material.
///
/// The desktop token is opaque, so the id is a content hash: the same login
/// state imported twice collapses to one id, and the id stays stable across
/// restarts without a counter file. Six hex characters is enough to avoid
/// collisions at any realistic pool size and short enough for log lines.
pub fn credential_id(access_token: &str) -> String {
    use std::fmt::Write;
    let digest = md5_hex(access_token.as_bytes());
    let mut out = String::from("wb-");
    for byte in &digest[..3] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// MD5 hex digest without pulling a crypto dependency: this is a *fingerprint*,
/// not a secret derivation — the token material is already a secret on disk.
fn md5_hex(data: &[u8]) -> [u8; 16] {
    // Minimal MD5 implementation (RFC 1321) to avoid a new dependency for what
    // is only an id fingerprint.
    let mut h: [u32; 4] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476];
    let mut msg = data.to_vec();
    let original_len_bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&original_len_bits.to_le_bytes());

    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    // The RFC 1321 K table: floor(|sin(i+1)| * 2^32). Computed here with a
    // const-compatible expression (f64::sin is not const-callable), so the
    // values are the standard constants spelled out via the reference series.
    const K: [u32; 64] = MD5_K;

    for chunk in msg.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            *word = u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f2 = f.wrapping_add(a).wrapping_add(K[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f2.rotate_left(S[i]));
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
    }
    let mut out = [0u8; 16];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

/// The RFC 1321 round-constant table (`floor(abs(sin(i+1)) * 2^32)`), spelled
/// out because `f64::sin` cannot run in a const context.
const MD5_K: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

/// Parse a pasted login state into a credential.
///
/// Accepts the shapes seen in the wild:
/// * the desktop app's auth file: `{auth: {...}, account: {...}, machineId}`
/// * workbuddy2api's session file: the same, plus `backend`/`endpoint` keys
/// * a bare `{accessToken, refreshToken, expiresAt, domain}` token object
/// * the desktop file's `{data: {...}}` wrapper (the client unwraps one level)
///
/// Anything else is rejected with a readable error rather than silently
/// producing an unusable credential.
pub fn parse_login_state(raw: &str) -> Result<WorkBuddyCredential> {
    let value: serde_json::Value =
        serde_json::from_str(raw.trim()).map_err(|e| anyhow::anyhow!("不是有效的 JSON: {}", e))?;

    // Unwrap the `{data: {...}}` envelope the desktop endpoints return.
    let value = match value.get("data") {
        Some(inner) if inner.is_object() && value.as_object().is_some_and(|o| o.len() == 1) => {
            inner.clone()
        }
        _ => value,
    };

    let auth = value.get("auth").cloned().unwrap_or(value.clone());
    let access = auth
        .get("accessToken")
        .or_else(|| value.get("accessToken"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("登录态缺少 accessToken 字段"))?;

    let account = value
        .get("account")
        .cloned()
        .unwrap_or_else(|| auth.get("account").cloned().unwrap_or_default());
    let account: WorkBuddyAccount = serde_json::from_value(account).unwrap_or_default();

    let expires_at_ms = auth
        .get("expiresAt")
        .or_else(|| value.get("expiresAt"))
        .and_then(serde_json::Value::as_i64);

    let machine_id = value
        .get("machineId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();

    Ok(WorkBuddyCredential {
        id: credential_id(access),
        label: account.nickname.clone(),
        access_token: access.to_string(),
        refresh_token: auth
            .get("refreshToken")
            .or_else(|| value.get("refreshToken"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        expires_at_ms,
        domain: auth
            .get("domain")
            .or_else(|| value.get("domain"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        account,
        machine_id,
        enabled: true,
        cooldown_until_ms: None,
        last_error: String::new(),
    })
}

/// Credentials file path: `~/.proxy-rs/workbuddy-credentials.json` (relocated by
/// `PROXY_DATA_DIR` like every other state file).
pub fn credentials_path() -> PathBuf {
    crate::settings::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("workbuddy-credentials.json")
}

/// Load every stored credential. A missing file is an empty store, not an error.
pub fn load_credentials() -> Vec<WorkBuddyCredential> {
    let Ok(text) = std::fs::read_to_string(credentials_path()) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Persist every credential. The file holds live tokens, so it is written
/// 0600 — the same protection workbuddy2api applies to its session file.
pub fn save_credentials(items: &[WorkBuddyCredential]) -> Result<()> {
    let path = credentials_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(items)?;
    std::fs::write(&path, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Upsert one credential (matched by [`WorkBuddyCredential::id`]).
pub fn upsert_credential(item: WorkBuddyCredential) -> Result<WorkBuddyCredential> {
    let mut items = load_credentials();
    let id = item.id.clone();
    if let Some(slot) = items.iter_mut().find(|c| c.id == id) {
        *slot = item.clone();
    } else {
        items.push(item.clone());
    }
    save_credentials(&items)?;
    Ok(item)
}

/// The default WorkBuddy auth endpoint prefix (`/v2/plugin` on the domestic
/// backend; workbuddy2api uses the same prefix for both regions).
pub const AUTH_PREFIX: &str = "/v2/plugin";

/// Refresh an access token against the desktop client's refresh endpoint.
///
/// Mirrors workbuddy2api's `refresh()`: `POST /v2/plugin/auth/token/refresh`
/// with `Authorization` omitted, the `X-Refresh-Token` header carrying the
/// refresh token, and `X-Auth-Refresh-Source: plugin` marking the channel.
/// On success the new token pair is persisted and returned.
pub async fn refresh_credential(
    client: &reqwest::Client,
    endpoint: &str,
    credential: &mut WorkBuddyCredential,
) -> Result<()> {
    let Some(refresh_token) = credential.refresh_token.clone() else {
        anyhow::bail!("凭据没有 refreshToken，无法刷新；请重新导入登录态");
    };
    let url = format!(
        "{}/{}/auth/token/refresh",
        endpoint.trim_end_matches('/'),
        AUTH_PREFIX
    );

    let resp = client
        .post(&url)
        .timeout(std::time::Duration::from_secs(30))
        .header("User-Agent", crate::providers::WORKBUDDY_USER_AGENT)
        .header("X-Product-Code", "codebuddy")
        .header("X-Refresh-Token", refresh_token)
        .header("X-Auth-Refresh-Source", "plugin")
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("刷新请求失败: {}", e))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "刷新端点返回 {}: {}",
            status,
            crate::util::truncate(&body, 300)
        );
    }

    // The desktop client unwraps `{data: {data: …}}` / `{data: …}` envelopes.
    let value: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("刷新响应不是有效 JSON: {}", e))?;
    let unwrapped = match value.get("data") {
        Some(inner) => inner.clone(),
        None => value,
    };
    let access = unwrapped
        .get("accessToken")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "刷新响应缺少 accessToken: {}",
                crate::util::truncate(&body, 200)
            )
        })?
        .to_string();

    credential.access_token = access;
    if let Some(rt) = unwrapped
        .get("refreshToken")
        .and_then(serde_json::Value::as_str)
    {
        credential.refresh_token = Some(rt.to_string());
    }
    if let Some(exp) = unwrapped
        .get("expiresAt")
        .and_then(serde_json::Value::as_i64)
    {
        credential.expires_at_ms = Some(exp);
    }
    if let Some(domain) = unwrapped.get("domain").and_then(serde_json::Value::as_str) {
        credential.domain = domain.to_string();
    }
    credential.last_error.clear();
    upsert_credential(credential.clone())?;
    Ok(())
}

/// Build the full upstream fingerprint header set for one credential.
///
/// Order and naming mirror the reference client (`auth_headers()` in
/// workbuddy2api): product/IDE identification on every request, then the
/// session-bound headers only when the login state provides them. This is the
/// set the gateway checks when it rejects "unapproved channel" invocations.
pub fn upstream_headers(credential: &WorkBuddyCredential) -> Vec<(String, String)> {
    let mut headers = vec![
        ("X-Product-Code".to_string(), "codebuddy".to_string()),
        ("X-IDE-Type".to_string(), "vscode".to_string()),
        ("X-IDE-Name".to_string(), "Visual Studio Code".to_string()),
        ("X-IDE-Version".to_string(), "1.70.2".to_string()),
        ("X-Product-Version".to_string(), "4.10.33259736".to_string()),
        (
            "X-Machine-Id".to_string(),
            if credential.machine_id.is_empty() {
                credential.id.clone()
            } else {
                credential.machine_id.clone()
            },
        ),
    ];
    if !credential.account.uid.is_empty() {
        headers.push(("X-User-Id".to_string(), credential.account.uid.clone()));
    }
    if !credential.account.enterprise_id.is_empty() {
        headers.push((
            "X-Enterprise-Id".to_string(),
            credential.account.enterprise_id.clone(),
        ));
        headers.push((
            "X-Tenant-Id".to_string(),
            credential.account.enterprise_id.clone(),
        ));
    }
    if !credential.domain.is_empty() {
        headers.push(("X-Domain".to_string(), credential.domain.clone()));
    }
    headers
}

// ── OAuth Device Flow & QR Code Authorization ──────────────────────────────

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
    Success {
        credential: WorkBuddyCredential,
    },
    Failed {
        error: String,
    },
}

/// Generate SVG vector string for any URL / text string.
pub fn generate_qr_svg(content: &str) -> Result<String> {
    use qrcode::render::svg;
    use qrcode::QrCode;

    let code = QrCode::new(content.as_bytes())
        .map_err(|e| anyhow::anyhow!("生成二维码失败: {}", e))?;
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
    let base = endpoint.unwrap_or(DEFAULT_WORKBUDDY_ENDPOINT).trim_end_matches('/');
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
        anyhow::bail!("授权端点返回 {}: {}", status, crate::util::truncate(&body, 300));
    }

    let val: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("解析响应失败: {}", e))?;
    let code = val.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = val.get("msg").and_then(|m| m.as_str()).unwrap_or("未知错误");
        anyhow::bail!("获取授权状态失败: {}", msg);
    }

    let data = val.get("data").ok_or_else(|| anyhow::anyhow!("响应缺少 data 字段"))?;
    let state = data.get("state").and_then(|s| s.as_str()).unwrap_or_default().to_string();
    let auth_url = data.get("authUrl").and_then(|s| s.as_str()).unwrap_or_default().to_string();

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
    let base = endpoint.unwrap_or(DEFAULT_WORKBUDDY_ENDPOINT).trim_end_matches('/');
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
    let val: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return Ok(OAuthPollResult::Failed { error: format!("无效响应: {}", e) }),
    };

    let code = val.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code == LOGIN_TOKEN_PENDING_CODE {
        return Ok(OAuthPollResult::Pending);
    }

    if code != 0 {
        let msg = val.get("msg").and_then(|m| m.as_str()).unwrap_or("授权失败");
        return Ok(OAuthPollResult::Failed { error: msg.to_string() });
    }

    let data = match val.get("data") {
        Some(d) if d.is_object() => d,
        _ => return Ok(OAuthPollResult::Failed { error: "缺少 token 数据".to_string() }),
    };

    let access_token = data.get("accessToken").and_then(|s| s.as_str()).unwrap_or_default().to_string();
    if access_token.is_empty() {
        return Ok(OAuthPollResult::Failed { error: "缺少 accessToken".to_string() });
    }

    let refresh_token = data.get("refreshToken").and_then(|s| s.as_str()).map(|s| s.to_string());
    let expires_at_ms = data.get("expiresAt").and_then(|e| e.as_i64());
    let domain = data.get("domain").and_then(|d| d.as_str()).unwrap_or("copilot.tencent.com").to_string();

    // Fetch account profile for nickname / uid / enterpriseId
    let account = fetch_account(client, base, &access_token).await.unwrap_or_default();
    let label = if !account.nickname.is_empty() {
        account.nickname.clone()
    } else {
        "扫码账号".to_string()
    };

    let cred = WorkBuddyCredential {
        id: credential_id(&access_token),
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
    };

    upsert_credential(cred.clone())?;
    Ok(OAuthPollResult::Success { credential: cred })
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

// ── Daily Check-in & Points Claiming ───────────────────────────────────────

/// Single account daily checkin result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckinResult {
    pub id: String,
    pub label: String,
    pub status: CheckinStatus,
    pub message: String,
    pub raw: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckinStatus {
    Success,
    AlreadyCheckedIn,
    Failed,
}

/// Batch check-in report across all credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchCheckinReport {
    pub total: usize,
    pub success: usize,
    pub already_checked_in: usize,
    pub failed: usize,
    pub details: Vec<CheckinResult>,
}

/// Execute daily checkin for a single WorkBuddy credential.
pub async fn claim_daily_checkin(
    client: &reqwest::Client,
    cred: &WorkBuddyCredential,
) -> CheckinResult {
    let id = cred.id.clone();
    let label = if !cred.label.is_empty() {
        cred.label.clone()
    } else if !cred.account.nickname.is_empty() {
        cred.account.nickname.clone()
    } else {
        id.clone()
    };

    if cred.access_token.is_empty() {
        return CheckinResult {
            id,
            label,
            status: CheckinStatus::Failed,
            message: "凭据缺少 access token".to_string(),
            raw: None,
        };
    }

    // Try copilot.tencent.com first, then fallback to www.codebuddy.cn
    let endpoints = [
        "https://copilot.tencent.com/v2/billing/meter/daily-checkin",
        "https://www.codebuddy.cn/v2/billing/meter/daily-checkin",
        "https://www.codebuddy.cn/billing/meter/daily-checkin",
    ];

    let mut last_err = String::new();
    for endpoint in endpoints {
        let mut req = client
            .post(endpoint)
            .timeout(std::time::Duration::from_secs(15))
            .header("Authorization", format!("Bearer {}", cred.access_token))
            .header("X-Product-Code", "codebuddy")
            .header(
                "User-Agent",
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36",
            )
            .header("Content-Type", "application/json")
            .header("X-Client-Platform", "web");

        if endpoint.contains("codebuddy.cn") {
            req = req
                .header("Origin", "https://www.codebuddy.cn")
                .header("Referer", "https://www.codebuddy.cn/profile/growth-center");
        }

        if !cred.account.uid.is_empty() {
            req = req.header("X-User-Id", &cred.account.uid);
        }
        if !cred.account.enterprise_id.is_empty() {
            req = req.header("X-Enterprise-Id", &cred.account.enterprise_id);
        }

        let resp = match req.json(&serde_json::json!({})).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("网络请求失败: {}", e);
                continue;
            }
        };

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            last_err = format!("HTTP {}: {}", status, crate::util::truncate(&body, 200));
            continue;
        }

        let val: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                last_err = format!("解析响应失败: {}", e);
                continue;
            }
        };

        let code = val.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        let msg = val.get("msg").and_then(|m| m.as_str()).unwrap_or("");

        if code == 0 {
            let reward_msg = if let Some(data) = val.get("data") {
                if let Some(pts) = data
                    .get("rewardPoint")
                    .or_else(|| data.get("points"))
                    .or_else(|| data.get("score"))
                {
                    format!("打卡成功，获得 +{} 积分", pts)
                } else {
                    "打卡成功，积分已领取".to_string()
                }
            } else {
                "打卡成功".to_string()
            };
            return CheckinResult {
                id,
                label,
                status: CheckinStatus::Success,
                message: reward_msg,
                raw: Some(val),
            };
        } else {
            let lower_msg = msg.to_lowercase();
            if lower_msg.contains("已签到")
                || lower_msg.contains("已经签到")
                || lower_msg.contains("repeat")
                || lower_msg.contains("already")
                || code == 10001
                || code == 10002
            {
                return CheckinResult {
                    id,
                    label,
                    status: CheckinStatus::AlreadyCheckedIn,
                    message: if msg.is_empty() {
                        "今日已完成打卡".to_string()
                    } else {
                        msg.to_string()
                    },
                    raw: Some(val),
                };
            } else {
                return CheckinResult {
                    id,
                    label,
                    status: CheckinStatus::Failed,
                    message: if msg.is_empty() {
                        format!("打卡失败 (code {})", code)
                    } else {
                        msg.to_string()
                    },
                    raw: Some(val),
                };
            }
        }
    }

    CheckinResult {
        id,
        label,
        status: CheckinStatus::Failed,
        message: if last_err.is_empty() {
            "打卡端点无响应".to_string()
        } else {
            last_err
        },
        raw: None,
    }
}

/// Run daily checkin across all enabled credentials in the pool.
pub async fn batch_claim_daily_checkin(client: &reqwest::Client) -> BatchCheckinReport {
    let credentials = load_credentials();
    let enabled: Vec<_> = credentials.into_iter().filter(|c| c.enabled).collect();
    let total = enabled.len();
    let mut details = Vec::with_capacity(total);
    let mut success = 0;
    let mut already_checked_in = 0;
    let mut failed = 0;

    for cred in &enabled {
        let res = claim_daily_checkin(client, cred).await;
        match res.status {
            CheckinStatus::Success => success += 1,
            CheckinStatus::AlreadyCheckedIn => already_checked_in += 1,
            CheckinStatus::Failed => failed += 1,
        }
        details.push(res);
    }

    BatchCheckinReport {
        total,
        success,
        already_checked_in,
        failed,
        details,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_desktop_login_state() {
        let raw = r#"{
            "auth": {
                "accessToken": "acc-1234567890",
                "refreshToken": "ref-9876543210",
                "expiresAt": 1893456000000,
                "domain": "copilot.tencent.com"
            },
            "account": { "uid": "u1", "enterpriseId": "e1", "nickname": "孙东" },
            "machineId": "machine-abc"
        }"#;
        let c = parse_login_state(raw).unwrap();
        assert_eq!(c.access_token, "acc-1234567890");
        assert_eq!(c.refresh_token.as_deref(), Some("ref-9876543210"));
        assert_eq!(c.account.uid, "u1");
        assert_eq!(c.account.enterprise_id, "e1");
        assert_eq!(c.domain, "copilot.tencent.com");
        assert_eq!(c.machine_id, "machine-abc");
        assert!(c.id.starts_with("wb-"));
        assert_eq!(c.label, "孙东");
    }

    #[test]
    fn parses_bare_token_object() {
        let c = parse_login_state(r#"{"accessToken":"tok-abcdef", "expiresAt": 1893456000000}"#)
            .unwrap();
        assert_eq!(c.access_token, "tok-abcdef");
        assert_eq!(c.refresh_token, None);
        assert_eq!(c.account.uid, "");
    }

    #[test]
    fn unwraps_data_envelope() {
        let raw =
            r#"{"data": {"auth": {"accessToken": "inner-token-123"}, "account": {"uid": "u9"}}}"#;
        let c = parse_login_state(raw).unwrap();
        assert_eq!(c.access_token, "inner-token-123");
        assert_eq!(c.account.uid, "u9");
    }

    #[test]
    fn rejects_login_state_without_access_token() {
        assert!(parse_login_state(r#"{"user": "x"}"#).is_err());
        assert!(parse_login_state("not json").is_err());
        assert!(parse_login_state(r#"{"auth": {"refreshToken": "only"}}"#).is_err());
    }

    #[test]
    fn same_login_state_yields_the_same_id() {
        let raw = r#"{"accessToken":"stable-token-value"}"#;
        assert_eq!(
            parse_login_state(raw).unwrap().id,
            parse_login_state(raw).unwrap().id
        );
        let other = parse_login_state(r#"{"accessToken":"different-token"}"#).unwrap();
        assert_ne!(parse_login_state(raw).unwrap().id, other.id);
    }

    #[test]
    fn usability_follows_expiry_margin() {
        let mut c = parse_login_state(r#"{"accessToken":"t"}"#).unwrap();
        assert!(c.is_usable(1_000));
        c.expires_at_ms = Some(1_000_000);
        // Valid until 60s before expiry: exp - 60_000 = 940_000.
        assert!(c.is_usable(939_999));
        assert!(!c.is_usable(940_000));
        assert!(!c.is_usable(1_000_000));
        // No expiry at all means always usable.
        c.expires_at_ms = None;
        assert!(c.is_usable(9_999_999_999));
    }

    #[test]
    fn usability_follows_cooldown_and_enabled() {
        let mut c = parse_login_state(r#"{"accessToken":"t"}"#).unwrap();
        c.cooldown_until_ms = Some(5_000);
        assert!(!c.is_usable(4_999));
        assert!(c.is_usable(5_000));
        c.cooldown_until_ms = None;
        c.enabled = false;
        assert!(!c.is_usable(0));
    }

    #[test]
    fn masked_token_never_shows_the_whole_secret() {
        let c = parse_login_state(r#"{"accessToken":"0123456789abcdef"}"#).unwrap();
        let m = c.masked_token();
        assert!(!m.contains("6789abcdef"));
        assert!(m.starts_with("0123"));
        // Short tokens degrade to full masking rather than leaking.
        let short = parse_login_state(r#"{"accessToken":"abc"}"#).unwrap();
        assert_eq!(short.masked_token(), "••••");
    }

    #[test]
    fn upstream_headers_carry_the_fingerprint() {
        let raw = r#"{
            "auth": {"accessToken": "a", "domain": "copilot.tencent.com"},
            "account": {"uid": "u1", "enterpriseId": "e1"},
            "machineId": "m1"
        }"#;
        let c = parse_login_state(raw).unwrap();
        let headers = upstream_headers(&c);
        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("X-Product-Code").as_deref(), Some("codebuddy"));
        assert_eq!(get("X-User-Id").as_deref(), Some("u1"));
        assert_eq!(get("X-Enterprise-Id").as_deref(), Some("e1"));
        assert_eq!(get("X-Tenant-Id").as_deref(), Some("e1"));
        assert_eq!(get("X-Domain").as_deref(), Some("copilot.tencent.com"));
        assert_eq!(get("X-Machine-Id").as_deref(), Some("m1"));
    }

    #[test]
    fn credential_id_is_stable_and_short() {
        let id = credential_id("some-long-token-material");
        assert_eq!(id.len(), 3 + 6);
        assert!(id.starts_with("wb-"));
        assert_eq!(id, credential_id("some-long-token-material"));
    }

    #[test]
    fn generates_valid_qr_svg() {
        let svg = generate_qr_svg("https://copilot.tencent.com/login?state=test").unwrap();
        assert!(svg.contains("<svg"));
        assert!(svg.contains("</svg>"));
    }
}

