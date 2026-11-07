//! See the module table in `workbuddy_auth/mod.rs` for what lives here.

use super::*;

/// The default WorkBuddy auth endpoint prefix (`/v2/plugin` on the domestic
/// backend; workbuddy2api uses the same prefix for both regions).
pub const AUTH_PREFIX: &str = "/v2/plugin";

/// Per-account refresh locks.
///
/// The upstream **rotates** the refresh token on every renewal, so two
/// concurrent refreshes of one account are not idempotent: the first consumes
/// `refreshToken A` and receives `B`, and the second still presents `A`, which
/// is now dead. The loser sees `12153 refresh token is invalid` and — because a
/// rejected refresh is exactly what marks an account "needs re-login" — a
/// perfectly healthy account gets flagged. That is the shape of the
/// libai/S.D incident: flagged as rejected, then healthy again later.
///
/// Serialising per credential id (not globally) keeps different accounts
/// refreshing in parallel, which matters because the pool refreshes several at
/// once on a schedule.
fn refresh_locks() -> &'static std::sync::Mutex<
    std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>,
> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    LOCKS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// The lock guarding refreshes of one credential.
pub(crate) fn refresh_lock_for(id: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    let mut map = refresh_locks()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    map.entry(id.to_string())
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// The `X-Auth-Refresh-Source` value for an endpoint.
///
/// The CN deployment tags desktop-issued renewals `workbuddy` and only the
/// plugin/IDE path uses `plugin`; the international deployment uses `plugin`.
/// Our code sent `plugin` everywhere. Both are accepted today — verified against
/// the live endpoint 2026-10-25 — so this is not a fix for a current failure,
/// but the channel marker is what the gateway would key a future tightening on,
/// and being wrong there would fail the same way the chat channel did.
pub(crate) fn refresh_source_for(endpoint: &str) -> &'static str {
    let host = endpoint
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = host.split('/').next().unwrap_or(host);
    if host.ends_with("workbuddy.ai") {
        "plugin"
    } else {
        "workbuddy"
    }
}

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
    // Serialise per account: the upstream rotates the refresh token, so two
    // concurrent renewals make the second present a token the first consumed.
    // Re-acquiring the guard here also re-reads the freshest token from disk
    // below, which is what makes the second caller a no-op instead of a failure.
    let lock = refresh_lock_for(&credential.id);
    let _guard = lock.lock().await;

    // Another task may have refreshed this credential while we waited. Adopt
    // its result rather than spending the rotated token a second time.
    //
    // The test is "the store holds a *different* access token than the caller
    // passed in", not "the stored one is usable": usability folds in `enabled`
    // and cooldown, and a credential that is cooling down (or was just disabled)
    // still has a perfectly fresh token that must not be refreshed again. Using
    // `is_usable` here would re-spend the rotated token in exactly the failover
    // scenario this lock exists to protect.
    if let Some(stored) = load_credentials()
        .into_iter()
        .find(|c| c.id == credential.id)
    {
        if stored.access_token != credential.access_token
            && !stored.access_token.is_empty()
            && stored.refresh_token == credential.refresh_token
        {
            *credential = stored;
            return Ok(());
        }
    }

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
        .header("X-Auth-Refresh-Source", refresh_source_for(endpoint))
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
    // Both field shapes, same helper the QR-login poll uses: the refresh
    // endpoint answers `expiresIn` (seconds, e.g. 96492) and never `expiresAt`,
    // so reading only the absolute field discarded the expiry on every single
    // renewal and left a token that lives ~27 hours looking unknown-lived.
    if let Some(exp) = expires_at_ms_from(&unwrapped, crate::util::unix_millis()) {
        credential.expires_at_ms = Some(exp);
    }
    if let Some(domain) = unwrapped.get("domain").and_then(serde_json::Value::as_str) {
        credential.domain = domain.to_string();
    }
    credential.last_error.clear();
    // Stamp the renewal: this is what the unknown-expiry backstop measures from,
    // so a credential whose response carries no usable expiry still gets renewed
    // again later instead of looking permanently fresh.
    credential.last_refresh_at_ms = Some(crate::util::unix_millis());
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

/// One account's token refresh attempt, as reported to the GUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshResult {
    pub id: String,
    pub label: String,
    /// True when the access token was renewed and written back.
    pub refreshed: bool,
    /// Why it failed, when it did. Empty on success.
    #[serde(default)]
    pub error: String,
    /// True when the account cannot self-heal and needs a fresh login: no
    /// refresh token at all, or the refresh endpoint rejected it.
    ///
    /// This is what drives the GUI's "需要重新绑定" marker, so the user is told
    /// *which* account to re-scan instead of being left to guess.
    #[serde(default)]
    pub needs_relogin: bool,
}

/// Result of a pool-wide login-state refresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshReport {
    pub total: usize,
    pub refreshed: usize,
    pub failed: usize,
    /// Accounts that need the user to log in again.
    pub needs_relogin: usize,
    pub details: Vec<RefreshResult>,
}

/// Renew the access token of one credential and describe the outcome.
///
/// The single place the per-account refresh is performed, shared by the manual
/// pool-wide refresh and the background scanner. They differ only in *which*
/// credentials they consider, and keeping the outcome logic (including the
/// `needs_relogin` classification, which the GUI shows to the user) in one
/// function is what stops the two paths from disagreeing about which accounts
/// need a fresh login.
///
/// On success the renewed credential has already been written back to the store
/// by [`refresh_credential`]. On failure the reason is persisted so the list can
/// show it without re-probing.
/// Whether a refresh failure can only be fixed by a new login.
///
/// The one place this classification lives, so the request path, the background
/// scanner and the GUI badge cannot disagree about which accounts are stranded:
///
///   * **no refresh token at all** — there is nothing to renew with;
///   * **the endpoint refused ours** (401/403, or an explicit complaint about
///     the refresh token) — the credential is revoked.
///
/// Anything else (connection reset, 5xx, timeout) is retryable and must *not* be
/// labelled relogin; otherwise a flaky network would light up every account with
/// "需重新绑定" and train the user to ignore it.
pub fn refresh_failure_needs_relogin(cred: &WorkBuddyCredential, error: &str) -> bool {
    cred.refresh_token.is_none()
        || error.contains("refreshToken")
        || error.contains("401")
        || error.contains("403")
}

/// Record a failed refresh on disk, distinguishing "retryable" from "log in again".
///
/// Both cases write `last_error` so the list can explain itself; only the
/// un-self-healing case sets [`WorkBuddyCredential::relogin_required_since_ms`],
/// which is what the GUI badge keys off and what must survive a restart.
pub fn mark_credential_refresh_failure(
    cred: &WorkBuddyCredential,
    error: &str,
    needs_relogin: bool,
) {
    let mut marked = cred.clone();
    marked.last_error = error.to_string();
    if needs_relogin {
        marked.relogin_required_since_ms = Some(crate::util::unix_millis());
    }
    let _ = upsert_credential(marked);
}

async fn refresh_one_credential(
    client: &reqwest::Client,
    cred: &WorkBuddyCredential,
) -> RefreshResult {
    let endpoint = auth_endpoint_for(cred);
    let mut fresh = cred.clone();
    match refresh_credential(client, &endpoint, &mut fresh).await {
        Ok(()) => {
            // A successful renewal clears any previous "log in again" verdict:
            // the account demonstrably heals itself now.
            if cred.relogin_required_since_ms.is_some() {
                let mut cleared = fresh.clone();
                cleared.relogin_required_since_ms = None;
                let _ = upsert_credential(cleared);
            }
            RefreshResult {
                id: fresh.id.clone(),
                label: display_label(&fresh),
                refreshed: true,
                error: String::new(),
                needs_relogin: false,
            }
        }
        Err(e) => {
            let msg = e.to_string();
            let needs_relogin = refresh_failure_needs_relogin(cred, &msg);
            mark_credential_refresh_failure(cred, &msg, needs_relogin);
            RefreshResult {
                id: cred.id.clone(),
                label: display_label(cred),
                refreshed: false,
                error: msg,
                needs_relogin,
            }
        }
    }
}

/// Renew every credential in `candidates`, sequentially, and summarise.
///
/// Sequential on purpose: a handful of accounts, and hammering the refresh
/// endpoint in parallel is exactly the pattern that gets a session flagged. A
/// failure never aborts the run — each account is independent, and one dead
/// refresh token must not stop the others from being renewed.
async fn refresh_candidates(
    client: &reqwest::Client,
    candidates: Vec<WorkBuddyCredential>,
) -> RefreshReport {
    let mut details = Vec::with_capacity(candidates.len());
    for cred in &candidates {
        details.push(refresh_one_credential(client, cred).await);
    }

    let refreshed = details.iter().filter(|d| d.refreshed).count();
    let needs_relogin = details.iter().filter(|d| d.needs_relogin).count();
    RefreshReport {
        total: details.len(),
        refreshed,
        failed: details.len() - refreshed,
        needs_relogin,
        details,
    }
}

/// Renew only the accounts whose token is due, and report what happened.
///
/// Used by the background scanner. Performs no requests at all when nothing is
/// due, which is what keeps an idle app silent.
pub async fn refresh_due_credentials(client: &reqwest::Client) -> RefreshReport {
    let now = crate::util::unix_millis();
    let candidates: Vec<WorkBuddyCredential> = load_credentials()
        .into_iter()
        .filter(|c| c.enabled && c.needs_refresh(now))
        .collect();
    refresh_candidates(client, candidates).await
}
