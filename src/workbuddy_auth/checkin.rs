//! See the module table in `workbuddy_auth/mod.rs` for what lives here.

use super::*;

/// Record a successful check-in for one credential on today's local day.
pub fn mark_checkin_done(id: &str) -> Result<()> {
    let mut items = load_credentials();
    let Some(c) = items.iter_mut().find(|c| c.id == id) else {
        return Ok(());
    };
    c.last_checkin_date = Some(local_day(crate::util::unix_millis()));
    save_credentials(&items)
}

/// Whether this credential already claimed today's check-in.
pub fn checked_in_today(c: &WorkBuddyCredential, today: &str) -> bool {
    c.last_checkin_date.as_deref() == Some(today)
}

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

/// Business codes the check-in endpoint returns for "already claimed today".
///
/// The upstream does not agree on one code across deployments, and `code == 0`
/// is *not* proof of a fresh claim: it also comes back for a repeat call that
/// granted nothing. So these are checked independently of `code`.
const ALREADY_CHECKIN_CODES: &[i64] = &[10001, 10002, 11217, 11101];

/// Substrings that mark "already checked in" in a message, ASCII-lowercased
/// before comparison so mixed-case English and Chinese both match (Chinese is
/// unaffected by the lowering).
const ALREADY_CHECKIN_WORDS: &[&str] = &[
    "already",
    "repeat",
    "duplicate",
    "已签到",
    "已经签到",
    "已打卡",
    "已经打卡",
    "今日已打卡",
    "今日已完成",
    "重复打卡",
    "重复签到",
    "已领取",
    "已经领取",
    "今日已领",
    "重复领取",
];

/// Whether `msg`/`code` mean this account already claimed today's check-in.
///
/// Exposed as a pure predicate so the classification is unit-testable without
/// a network call — the counting bug this fixes was precisely a misread of the
/// response, and a heuristic this wide needs tests more than the I/O does.
pub fn is_already_checked_in(code: Option<i64>, msg: &str) -> bool {
    if let Some(code) = code {
        if ALREADY_CHECKIN_CODES.contains(&code) {
            return true;
        }
    }
    // Compare case-insensitively; the Chinese entries pass through unchanged.
    let lower = msg.to_lowercase();
    ALREADY_CHECKIN_WORDS.iter().any(|w| lower.contains(w))
}

/// Whether a `code == 0` response actually granted points.
///
/// A repeat check-in returns success with no reward payload, so "success" is
/// only claimed when a reward figure is present — otherwise the run is counted
/// as already-checked-in rather than inflating the success counter.
pub fn has_reward(val: &serde_json::Value) -> bool {
    reward_points(val).is_some()
}

/// The reward point figure in a check-in response, if any.
pub(crate) fn reward_points(val: &serde_json::Value) -> Option<f64> {
    let data = val.get("data")?;
    for key in ["rewardPoint", "rewardPoints", "points", "score", "point"] {
        if let Some(n) = data.get(key).and_then(serde_json::Value::as_f64) {
            return Some(n);
        }
        // Some deployments spell the reward as a string ("100").
        if let Some(s) = data.get(key).and_then(serde_json::Value::as_str) {
            if let Ok(n) = s.trim().parse::<f64>() {
                return Some(n);
            }
        }
    }
    None
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
///
/// `force` skips the "already claimed today" short-circuit: the GUI's manual
/// button uses it so a user can re-run a claim the local record thinks is done
/// (e.g. after the upstream reset at midnight in another timezone).
pub async fn claim_daily_checkin(
    client: &reqwest::Client,
    cred: &WorkBuddyCredential,
) -> CheckinResult {
    claim_daily_checkin_inner(client, cred, false).await
}

pub async fn claim_daily_checkin_forced(
    client: &reqwest::Client,
    cred: &WorkBuddyCredential,
) -> CheckinResult {
    claim_daily_checkin_inner(client, cred, true).await
}

async fn claim_daily_checkin_inner(
    client: &reqwest::Client,
    cred: &WorkBuddyCredential,
    force: bool,
) -> CheckinResult {
    let id = cred.id.clone();
    let label = display_label(cred);

    if cred.access_token.is_empty() {
        return CheckinResult {
            id,
            label,
            status: CheckinStatus::Failed,
            message: "凭据缺少 access token".to_string(),
            raw: None,
        };
    }

    // Local-day guard: the scheduler runs unattended, so it must not re-claim
    // an account that already got today's allowance.
    let today = local_day(crate::util::unix_millis());
    if !force && checked_in_today(cred, &today) {
        return CheckinResult {
            id,
            label,
            status: CheckinStatus::AlreadyCheckedIn,
            message: "今日已打卡".to_string(),
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
        // Some deployments spell the message field `message`, and some nest it
        // under `data`. Both feed the already-checked-in classification, so all
        // spellings are read before a fresh success is claimed.
        let msg = val
            .get("msg")
            .or_else(|| val.get("message"))
            .or_else(|| {
                val.get("data")
                    .and_then(|d| d.get("msg").or_else(|| d.get("message")))
            })
            .and_then(|m| m.as_str())
            .unwrap_or("");

        if code == 0 {
            // A repeat claim also answers `code == 0`, just without a reward
            // payload — and sometimes with an "already" message. Counting those
            // as fresh successes is what made 今日已打卡 read 0, so the reward
            // is what decides success.
            if is_already_checked_in(Some(code), msg) || !has_reward(&val) {
                let _ = mark_checkin_done(&id);
                return CheckinResult {
                    id,
                    label,
                    status: CheckinStatus::AlreadyCheckedIn,
                    message: "今日已打卡".to_string(),
                    raw: Some(val),
                };
            }
            let pts = reward_points(&val).unwrap_or_default();
            // Remember the local day so the scheduler does not claim twice.
            let _ = mark_checkin_done(&id);
            return CheckinResult {
                id,
                label,
                status: CheckinStatus::Success,
                message: format!("打卡成功，获得 +{} 积分", pts),
                raw: Some(val),
            };
        }

        if is_already_checked_in(Some(code), msg) {
            let _ = mark_checkin_done(&id);
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
        }

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
///
/// `force` is the manual button: it ignores the local "already claimed today"
/// record, so a user who knows the upstream reset can re-claim deliberately.
pub async fn batch_claim_daily_checkin(client: &reqwest::Client) -> BatchCheckinReport {
    batch_claim_daily_checkin_inner(client, false).await
}

pub async fn batch_claim_daily_checkin_forced(client: &reqwest::Client) -> BatchCheckinReport {
    batch_claim_daily_checkin_inner(client, true).await
}

async fn batch_claim_daily_checkin_inner(
    client: &reqwest::Client,
    force: bool,
) -> BatchCheckinReport {
    let credentials = load_credentials();
    let enabled: Vec<_> = credentials.into_iter().filter(|c| c.enabled).collect();
    let total = enabled.len();
    let mut details = Vec::with_capacity(total);
    let mut success = 0;
    let mut already_checked_in = 0;
    let mut failed = 0;

    for cred in &enabled {
        let res = claim_daily_checkin_inner(client, cred, force).await;
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
