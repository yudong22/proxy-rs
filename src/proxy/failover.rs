//! Credential relocation: when to leave the configured default, and the short
//! memory that keeps a proven failover from being rediscovered per request.
//!
//! Split out of `proxy.rs`. The memory is deliberately process-global (see
//! `FAILOVER_MEMORY`), and the predicates here decide whether an upstream failure
//! is even eligible to switch identities.

/// How long a proven failover target is reused without re-probing the failing
/// credential, in milliseconds (3 min).
///
/// Long enough that a broken upstream path (the `401 not_found` routing fault)
/// costs one discovery per three minutes instead of one per request; short
/// enough that a transient blip does not pin traffic away from the configured
/// default for long.
pub(crate) const FAILOVER_MEMORY_TTL_MS: i64 = 3 * 60 * 1000;

/// One remembered "credential X is failing → go straight to Y" decision.
#[derive(Debug, Clone)]
pub(crate) struct FailoverMemory {
    /// The credential/key id that failed.
    from: String,
    /// What to serve instead: an account-pool credential, or an API key.
    to: FailoverTarget,
    /// When the memory stops being consulted.
    pub(crate) expires_at_ms: i64,
}

/// Where a remembered failover lands.
#[derive(Debug, Clone)]
pub(crate) enum FailoverTarget {
    /// An account-pool credential id — fed back through `pool.pick`-style
    /// lookup so the freshest token for that account is used.
    Credential(String),
    /// An API-key-pool entry id.
    ApiKey(String),
}

/// Skip the failing credential entirely when a recent request already proved it
/// broken and found a working replacement.
///
/// Without this, every single request re-discovers the same failure from
/// scratch: it pays the upstream round-trip, the 3 s `not_found` sleep, the
/// second round-trip, and only then fails over. In the field that showed up as
/// *every* request costing ~5 s and two logged upstream errors before it was
/// served — the log filled with
/// `401 not_found … → 账号池无可用替换账号 … → ok override=k-…`
/// repeated verbatim for request after request.
///
/// Half-open semantics. While the memory is fresh the request is relocated
/// straight onto the remembered replacement — no probe of the broken identity.
/// Once the TTL expires the memory is cleared *here* and `None` is returned, so
/// the request falls through to the ordinary default-first path: that attempt
/// is the half-open probe. It either succeeds (normal service, no override) or
/// fails (the failover path re-records the memory, restarting the window).
///
/// The memory is a hint, never the only path: it is only consulted on the first
/// credential attempt, and only when its replacement is still present and
/// enabled. Anything that makes it inapplicable falls through to the existing
/// discovery path unchanged.
pub(crate) async fn remembered_failover(
    pool: &crate::session_pool::SharedCredentialPool,
    credential: Option<&crate::workbuddy_auth::WorkBuddyCredential>,
) -> Option<FailoverTarget> {
    let from = credential.map(|c| c.id.clone())?;
    let now = crate::util::unix_millis();

    let memory = {
        let guard = failover_memory().read().ok()?;
        match guard.as_ref() {
            Some(m) if m.from == from && now < m.expires_at_ms => m.to.clone(),
            // Expired, or about a different credential: nothing to reuse. An
            // expired entry for *this* credential is cleared so the ordinary
            // default-first path gets to probe it once — that is the half-open
            // step, and it is what lets a recovered default drop its override.
            _ => return None,
        }
    };

    // The replacement must still exist: a deleted or disabled account must send
    // us back through discovery rather than pinning every request to a ghost.
    match &memory {
        FailoverTarget::Credential(id) => {
            let still_there = pool
                .snapshot()
                .await
                .iter()
                .any(|c| c.id == *id && c.enabled);
            if !still_there {
                failover_memory().write().ok().and_then(|mut g| g.take());
                return None;
            }
        }
        FailoverTarget::ApiKey(id) => {
            let still_there = crate::workbuddy_auth::enabled_api_keys()
                .iter()
                .any(|k| k.id == *id);
            if !still_there {
                failover_memory().write().ok().and_then(|mut g| g.take());
                return None;
            }
        }
    }

    // Debug, not WARN: this fires on *every* request while the window is open,
    // so a WARN here made the log read like a series of failures even though
    // each request was being served fine on the first attempt. The override
    // columns and the per-request `ok` line still record it.
    tracing::debug!(
        "credential {} is in failover memory; reusing {} without probing",
        from,
        match &memory {
            FailoverTarget::Credential(id) => id.clone(),
            FailoverTarget::ApiKey(id) => id.clone(),
        }
    );
    Some(memory)
}

/// The one most recent failover decision, process-wide.
///
/// Deliberately global rather than per-session: the observed failures are
/// upstream-path faults (a gateway that cannot route), which hit every session
/// at once, so the point of remembering is that *other* sessions benefit too.
/// A per-session map would let each new session rediscover the same broken path.
pub(crate) static FAILOVER_MEMORY: std::sync::OnceLock<std::sync::RwLock<Option<FailoverMemory>>> =
    std::sync::OnceLock::new();

/// The shared memory slot, initializing it on first use.
pub(crate) fn failover_memory() -> &'static std::sync::RwLock<Option<FailoverMemory>> {
    FAILOVER_MEMORY.get_or_init(|| std::sync::RwLock::new(None))
}

/// Whether a recent request already proved `from` broken.
///
/// Used to skip the exploratory retry: once the failure is known, waiting 3 s to
/// re-probe the same broken path is pure latency on the client's critical path.
pub(crate) fn failover_is_known(from: &str) -> bool {
    let now = crate::util::unix_millis();
    failover_memory()
        .read()
        .ok()
        .and_then(|g| g.as_ref().map(|m| m.from == from && now < m.expires_at_ms))
        .unwrap_or(false)
}

/// Record that `from` failed and `to` served the request instead.
pub(crate) fn remember_failover(from: &str, to: FailoverTarget) {
    let lock = failover_memory();
    if let Ok(mut guard) = lock.write() {
        *guard = Some(FailoverMemory {
            from: from.to_string(),
            to,
            expires_at_ms: crate::util::unix_millis() + FAILOVER_MEMORY_TTL_MS,
        });
    }
}

/// Forget the memory, if any.
///
/// Called when the identity that previously failed serves a request
/// successfully: the half-open probe has passed, so later requests must go back
/// to the default-first path instead of being pinned to the override forever.
pub(crate) fn clear_failover() {
    failover_memory().write().ok().and_then(|mut g| g.take());
}

/// Whether an upstream failure should relocate the session onto another
/// credential: the plan's limit/failure classes — 429 rate limit, 402 out of
/// credit, 403 banned/permission denied, and WorkBuddy quota business codes
/// (11105/11106 family). The 11128 content-policy code is deliberately absent:
/// switching accounts cannot fix a content-policy rejection.
pub(crate) fn is_switchable_error(status: u16, body: &str) -> bool {
    // 401: stale/revoked token (handled first with a refresh, then failover)
    // 402: payment required / quota exceeded
    // 403: account banned / permission denied
    // 429: rate limited
    // WorkBuddy quota business codes always switch regardless of HTTP status.
    matches!(status, 401 | 402 | 403 | 429) || is_quota_business_code(body)
}

/// WorkBuddy business codes that signal quota/rate exhaustion for this account.
pub(crate) fn is_quota_business_code(body: &str) -> bool {
    // 11105: quota exhausted; 11106: rate/frequency limited. Kept alongside
    // the status classes so a 200-wrapped business error still switches.
    body.contains("11105") || body.contains("11106")
}

pub(crate) fn is_retriable_status(status: u16) -> bool {
    matches!(status, 429 | 500..=599)
}
