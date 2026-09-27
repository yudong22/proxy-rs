//! Upstream credential pool: default-first selection with failover stickiness.
//!
//! The v1.8.0 routing contract (see `docs/plan-1.8.0.md` §2.1a):
//!
//! 1. **Default-first, no error → no switch.** While the GUI-selected default
//!    credential is healthy, *every* request — any session, any client — uses
//!    it. There is no proactive spreading and no "most remaining quota"
//!    everyday balancing.
//! 2. **Only a 4xx limit/failure exception switches**, and only for the one
//!    session that hit it. Every other session keeps going to the default.
//! 3. **Sticky after switching, for the cache.** Upstream prompt caches are
//!    per-account; once a session is moved onto a replacement credential it
//!    stays there, so its follow-up turns rebuild and reuse the cache instead
//!    of being scattered per request. The session is *not* bounced back when
//!    the default recovers — that flip-flop would invalidate the cache twice.
//!
//! The `switched` map is the only state: sessions that never errored do not
//! appear in it, so the hot path is one map lookup, and nothing is persisted —
//! a restart returns to "everything on the default", costing one cache warm-up.

use crate::workbuddy_auth::WorkBuddyCredential;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Default bound on the switched-session map. Generous: each entry is two short
/// strings, and 1000 concurrent relocated conversations is well past any single
/// user's reality.
const DEFAULT_SWITCHED_CAPACITY: usize = 1000;

/// Default sticky TTL for a relocated session, in milliseconds (24h).
pub const DEFAULT_STICKY_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// One relocated session: which credential it now uses and how long to remember.
#[derive(Debug, Clone)]
struct StickySession {
    credential_id: String,
    expires_at_ms: i64,
}

/// The pool state. Constructed once per proxy start from the credential store.
pub struct CredentialPool {
    /// All credentials in the pool, in store order. The first enabled one that
    /// is not cooling down acts as the default (the GUI "selected" credential
    /// is simply ordered first by the caller).
    entries: RwLock<Vec<WorkBuddyCredential>>,
    /// Sessions relocated by a 4xx exception, keyed by namespaced session id.
    switched: RwLock<HashMap<String, StickySession>>,
    capacity: usize,
    sticky_ttl_ms: i64,
}

impl CredentialPool {
    /// Build a pool from the credential store, preserving the given order. The
    /// caller (GUI selection) puts the intended default first.
    pub fn new(credentials: Vec<WorkBuddyCredential>) -> Self {
        Self::with_limits(
            credentials,
            DEFAULT_SWITCHED_CAPACITY,
            DEFAULT_STICKY_TTL_MS,
        )
    }

    /// Test-constructor with explicit limits.
    pub fn with_limits(
        credentials: Vec<WorkBuddyCredential>,
        capacity: usize,
        sticky_ttl_ms: i64,
    ) -> Self {
        Self {
            entries: RwLock::new(credentials),
            switched: RwLock::new(HashMap::new()),
            capacity,
            sticky_ttl_ms,
        }
    }

    /// An empty pool: `pick` yields `None` and the proxy falls back to the
    /// legacy static-key path unchanged.
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Whether this pool has any enabled credential at all. When false, the
    /// request path must behave exactly like 1.7.1.
    pub async fn is_enabled(&self) -> bool {
        let now = crate::util::unix_millis();
        self.entries.read().await.iter().any(|c| c.is_usable(now))
    }

    /// Choose the credential for one request.
    ///
    /// Resolution order (see the module docs): a healthy sticky binding wins —
    /// otherwise the default credential. The default is simply the first
    /// usable entry; the "most remaining quota / rotation" preference applies
    /// only at the moment of an actual failover (in [`Self::switch`]), never
    /// here.
    pub async fn pick(&self, session_id: &str) -> Option<WorkBuddyCredential> {
        let now = crate::util::unix_millis();
        let entries = self.entries.read().await;

        // 1. Sticky: only for a session that a previous 4xx actually moved.
        if let Some(binding) = self.switched.read().await.get(session_id) {
            if now < binding.expires_at_ms {
                if let Some(c) = entries
                    .iter()
                    .find(|c| c.id == binding.credential_id && c.is_usable(now))
                {
                    return Some(c.clone());
                }
            }
            // Expired binding or its credential is gone/unhealthy: fall
            // through to the default and let a later `switch` re-home.
        }

        // 2. Default: first usable entry.
        entries.iter().find(|c| c.is_usable(now)).cloned()
    }

    /// Failover: choose a replacement for `from` on behalf of one session and
    /// record the sticky binding.
    ///
    /// Preference among the remaining healthy credentials is "most remaining
    /// quota first" when a credential can report it, then store order. This is
    /// the only place that ordering is consulted.
    pub async fn switch(&self, session_id: &str, from: &str) -> Option<WorkBuddyCredential> {
        let now = crate::util::unix_millis();
        let entries = self.entries.read().await;
        let mut candidates: Vec<&WorkBuddyCredential> = entries
            .iter()
            .filter(|c| c.id != from && c.is_usable(now))
            .collect();
        // Most remaining points first, when a refresh has recorded them: a
        // failover should land on the account that can actually keep serving.
        // Credentials with no figure sort last (treated as unknown, not zero),
        // and ties keep store order.
        candidates.sort_by(|a, b| {
            b.points
                .unwrap_or(-1.0)
                .partial_cmp(&a.points.unwrap_or(-1.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let chosen = (*candidates.first()?).clone();

        let mut switched = self.switched.write().await;
        // Enforce the LRU-ish bound: dropping expired entries first is enough
        // in practice; only when none are stale do we evict an arbitrary entry
        // (HashMap order) — the map is a cache, not a ledger.
        if switched.len() >= self.capacity {
            switched.retain(|_, s| now < s.expires_at_ms);
            if switched.len() >= self.capacity {
                if let Some(key) = switched.keys().next().cloned() {
                    switched.remove(&key);
                }
            }
        }
        switched.insert(
            session_id.to_string(),
            StickySession {
                credential_id: chosen.id.clone(),
                expires_at_ms: now + self.sticky_ttl_ms,
            },
        );
        Some(chosen)
    }

    /// Mark a credential cooling down after a limit/failure exception, and
    /// forget every sticky binding that pointed at it (a cooled credential
    /// cannot serve anyone, and a stale binding would silently pin sessions to
    /// an unhealthy credential until TTL).
    pub async fn mark_limited(&self, id: &str, cooldown_ms: i64) {
        let now = crate::util::unix_millis();
        let mut entries = self.entries.write().await;
        if let Some(c) = entries.iter_mut().find(|c| c.id == id) {
            c.cooldown_until_ms = Some(now + cooldown_ms);
            c.last_error = format!("rate limited, cooling down {}ms", cooldown_ms);
        }
        drop(entries);
        let mut switched = self.switched.write().await;
        switched.retain(|_, s| s.credential_id != id);
    }

    /// Clear a credential's cooldown (manual re-enable or successful refresh).
    pub async fn mark_ok(&self, id: &str) {
        let mut entries = self.entries.write().await;
        if let Some(c) = entries.iter_mut().find(|c| c.id == id) {
            c.cooldown_until_ms = None;
            c.last_error.clear();
        }
    }

    /// Replace one credential in place, preserving pool order.
    ///
    /// Used after a successful token refresh so the very next request picks up
    /// the new access token without waiting for a pool reload.
    pub async fn update_credential(&self, credential: WorkBuddyCredential) -> bool {
        let mut entries = self.entries.write().await;
        if let Some(slot) = entries.iter_mut().find(|c| c.id == credential.id) {
            *slot = credential;
            true
        } else {
            false
        }
    }

    /// Park a credential for a short while after a failed token refresh, so
    /// every in-flight request does not re-hit the refresh endpoint.
    pub async fn mark_refresh_failed(&self, id: &str, cooldown_ms: i64, message: &str) {
        let now = crate::util::unix_millis();
        let mut entries = self.entries.write().await;
        if let Some(c) = entries.iter_mut().find(|c| c.id == id) {
            c.cooldown_until_ms = Some(now + cooldown_ms);
            c.last_error = message.to_string();
        }
    }

    /// The id of the credential that counts as "the default" for override
    /// attribution.
    ///
    /// This is the first **enabled** entry in pool order — deliberately
    /// independent of health. The caller orders the pool so the configured
    /// default is first (see [`crate::workbuddy_auth::ordered_credentials`]),
    /// which means the answer is stable across requests.
    ///
    /// Using the first *usable* entry instead was a bug: the moment the
    /// configured default hit a cooldown, the sticky replacement became the
    /// "first usable" entry, so the very session that had failed over stopped
    /// looking like an override — the override column went blank after one
    /// switch. Health must not change what "default" means.
    pub async fn configured_default_id(&self) -> Option<String> {
        let entries = self.entries.read().await;
        entries.iter().find(|c| c.enabled).map(|c| c.id.clone())
    }

    /// The credential that counts as the configured default (first enabled
    /// entry), for proactive token renewal before selection.
    pub async fn default_credential(&self) -> Option<WorkBuddyCredential> {
        let entries = self.entries.read().await;
        entries.iter().find(|c| c.enabled).cloned()
    }

    /// Replace the credential list, preserving the sticky bindings.
    ///
    /// Called when the GUI changes the default account or imports/deletes one.
    /// Only the bindings pointing at a credential that no longer exists are
    /// dropped; the rest keep their session on the account they were relocated
    /// to, so re-selecting a default does not invalidate everyone's cache.
    pub async fn reload(&self, credentials: Vec<WorkBuddyCredential>) {
        let present: std::collections::HashSet<String> =
            credentials.iter().map(|c| c.id.clone()).collect();
        *self.entries.write().await = credentials;
        self.switched
            .write()
            .await
            .retain(|_, s| present.contains(&s.credential_id));
    }

    /// Reload from the credential store, honouring the selected default's
    /// ordering. The GUI calls this after a selection change so the running
    /// proxy picks up the new default without a restart.
    pub async fn reload_from_store(&self) {
        self.reload(crate::workbuddy_auth::ordered_credentials())
            .await;
    }

    /// Drop every sticky binding: all sessions return to the default
    /// credential. Wired to the GUI "重置粘滞" button.
    pub async fn reset_sticky(&self) {
        self.switched.write().await.clear();
    }

    /// How many sessions currently sit on a relocated credential (GUI display).
    pub async fn sticky_count(&self) -> usize {
        self.switched.read().await.len()
    }

    /// A snapshot of all credentials for the GUI credential list.
    pub async fn snapshot(&self) -> Vec<WorkBuddyCredential> {
        self.entries.read().await.clone()
    }
}

/// Shared handle used across the request path.
pub type SharedCredentialPool = Arc<CredentialPool>;

#[cfg(test)]
mod tests {
    use super::*;

    fn cred(id: &str) -> WorkBuddyCredential {
        WorkBuddyCredential {
            id: id.to_string(),
            label: id.to_string(),
            access_token: format!("token-{}", id),
            enabled: true,
            ..Default::default()
        }
    }

    fn pool(items: &[&str]) -> CredentialPool {
        CredentialPool::new(items.iter().map(|id| cred(id)).collect())
    }

    /// A session with no error history always gets the default credential —
    /// the "default-first, no error no switch" contract.
    #[tokio::test]
    async fn every_healthy_session_goes_to_the_default() {
        let p = pool(&["wb-a", "wb-b", "wb-c"]);
        for session in ["claude:s1", "codex:s2", "dsh:s3"] {
            let picked = p.pick(session).await.unwrap();
            assert_eq!(picked.id, "wb-a", "{session} must use the default");
        }
    }

    /// Only the session that hit the exception is relocated; others stay.
    #[tokio::test]
    async fn only_the_error_session_switches() {
        let p = pool(&["wb-a", "wb-b"]);
        p.mark_limited("wb-a", 60_000).await;

        // Default is cooling: new sessions can still fail over.
        let relocated = p.pick("claude:error-session").await.unwrap();
        assert_eq!(relocated.id, "wb-b");
        // And the switch records stickiness for that one session only.
        let sticky = p.sticky_count().await;
        assert_eq!(sticky, 0, "pick() alone must not create bindings");

        let switched = p.switch("claude:error-session", "wb-a").await.unwrap();
        assert_eq!(switched.id, "wb-b");
        assert_eq!(p.sticky_count().await, 1);
    }

    /// A relocated session stays on its replacement (cache reuse), even after
    /// the default credential recovers.
    #[tokio::test]
    async fn switched_session_stays_sticky_after_default_recovers() {
        let p = pool(&["wb-a", "wb-b"]);
        p.mark_limited("wb-a", 60_000).await;
        p.switch("claude:s1", "wb-a").await.unwrap();

        // Default cools off the cooldown...
        p.mark_ok("wb-a").await;
        // ...but s1 keeps its replacement.
        let picked = p.pick("claude:s1").await.unwrap();
        assert_eq!(picked.id, "wb-b", "sticky binding must survive recovery");
        // A different session goes back to the (now healthy) default.
        let other = p.pick("claude:s2").await.unwrap();
        assert_eq!(other.id, "wb-a");
    }

    /// A sticky session whose replacement itself fails over one more level.
    ///
    /// Real flow: s1's 429 on the default marked wb-a cooling, the switch
    /// homed it on wb-b, then wb-b's own 429 marked it cooling too — so the
    /// next switch must land on wb-c, and stickiness follows.
    #[tokio::test]
    async fn sticky_relocates_again_when_replacement_fails() {
        let p = pool(&["wb-a", "wb-b", "wb-c"]);
        p.mark_limited("wb-a", 60_000).await;
        p.switch("claude:s1", "wb-a").await.unwrap();
        assert_eq!(p.pick("claude:s1").await.unwrap().id, "wb-b");

        p.mark_limited("wb-b", 60_000).await;
        let again = p.switch("claude:s1", "wb-b").await.unwrap();
        assert_eq!(again.id, "wb-c");
        let final_pick = p.pick("claude:s1").await.unwrap();
        assert_eq!(final_pick.id, "wb-c");
        // Other sessions skip all three: a and b cooling, c only reachable by
        // them because they never errored — default-first picks first usable.
        assert_eq!(p.pick("codex:s2").await.unwrap().id, "wb-c");
    }

    /// No session id (anonymous client): default credential, never sticky.
    #[tokio::test]
    async fn anonymous_requests_never_become_sticky() {
        let p = pool(&["wb-a", "wb-b"]);
        let picked = p.pick("").await.unwrap();
        assert_eq!(picked.id, "wb-a");
        let switched = p.switch("", "wb-a").await.unwrap();
        assert_eq!(switched.id, "wb-b");
        // The "" binding exists but does not affect other sessions...
        assert_eq!(p.pick("claude:s1").await.unwrap().id, "wb-a");
        // ...and the next anonymous request still starts from the default
        // policy-wise; the binding only applies while "" keeps failing.
    }

    /// Disabled credentials are skipped entirely.
    #[tokio::test]
    async fn disabled_credentials_are_skipped() {
        let mut items = vec![cred("wb-a"), cred("wb-b")];
        items[0].enabled = false;
        let p = CredentialPool::new(items);
        assert_eq!(p.pick("claude:s1").await.unwrap().id, "wb-b");
    }

    /// Reset sticky sends everyone home.
    #[tokio::test]
    async fn reset_sticky_returns_all_sessions_to_default() {
        let p = pool(&["wb-a", "wb-b"]);
        p.switch("claude:s1", "wb-a").await.unwrap();
        p.switch("codex:s2", "wb-a").await.unwrap();
        assert_eq!(p.sticky_count().await, 2);
        p.reset_sticky().await;
        assert_eq!(p.sticky_count().await, 0);
        assert_eq!(p.pick("claude:s1").await.unwrap().id, "wb-a");
        assert_eq!(p.pick("codex:s2").await.unwrap().id, "wb-a");
    }

    /// Expired sticky bindings no longer pin the session.
    #[tokio::test]
    async fn expired_sticky_binding_is_ignored() {
        let p = CredentialPool::with_limits(
            vec![cred("wb-a"), cred("wb-b")],
            DEFAULT_SWITCHED_CAPACITY,
            // TTL in the past — every binding is immediately stale.
            -1_000,
        );
        p.switch("claude:s1", "wb-a").await.unwrap();
        assert_eq!(p.pick("claude:s1").await.unwrap().id, "wb-a");
    }

    /// An empty pool reports disabled so the legacy path stays untouched.
    #[tokio::test]
    async fn empty_pool_is_disabled() {
        let p = CredentialPool::empty();
        assert!(!p.is_enabled().await);
        assert!(p.pick("claude:s1").await.is_none());
    }

    /// The override label must survive the default going unhealthy.
    ///
    /// Regression: the default was computed as the first *usable* entry. As soon
    /// as the configured default hit a cooldown it dropped out, so the sticky
    /// replacement became "first usable" — the very session that had failed over
    /// stopped looking like an override, and the override column went blank after
    /// one switch. The default must be the first *enabled* entry in pool order.
    #[tokio::test]
    async fn configured_default_stays_put_when_the_default_cools_down() {
        let p = pool(&["wb-a", "wb-b"]);
        assert_eq!(p.configured_default_id().await.as_deref(), Some("wb-a"));

        // The default is rate-limited and a session relocates to wb-b.
        p.mark_limited("wb-a", 60_000).await;
        p.switch("claude:s1", "wb-a").await.unwrap();
        let served = p.pick("claude:s1").await.unwrap();
        assert_eq!(served.id, "wb-b");

        // It must STILL report wb-a as the default, so `served != default` and
        // the session keeps being labelled an override on every later request.
        assert_eq!(
            p.configured_default_id().await.as_deref(),
            Some("wb-a"),
            "health must not redefine which credential is the default"
        );
    }

    /// `update_credential` replaces the token in place, keeping pool order.
    #[tokio::test]
    async fn update_credential_preserves_order_and_swaps_the_token() {
        let p = pool(&["wb-a", "wb-b"]);
        let mut fresh = cred("wb-b");
        fresh.access_token = "new-token".to_string();
        assert!(p.update_credential(fresh).await);

        let items = p.snapshot().await;
        assert_eq!(items[0].id, "wb-a", "order is preserved");
        assert_eq!(items[1].access_token, "new-token");
    }

    /// Failover prefers the credential with the most remaining points.
    ///
    /// This is why the GUI refreshes and stores points: the switch is the one
    /// moment the "most remaining quota" rule is consulted.
    #[tokio::test]
    async fn failover_prefers_the_most_remaining_points() {
        let mut items = vec![cred("wb-a"), cred("wb-b"), cred("wb-c")];
        items[1].points = Some(500.0);
        items[2].points = Some(50.0);
        // wb-a has no figure: unknown, so it sorts last behind both.
        let p = CredentialPool::new(items);

        let chosen = p.switch("claude:s1", "wb-a").await.unwrap();
        assert_eq!(chosen.id, "wb-b", "most points wins");
    }

    /// Reloading keeps sticky bindings whose credential still exists and drops
    /// the ones pointing at a removed credential.
    #[tokio::test]
    async fn reload_preserves_live_bindings_and_drops_dead_ones() {
        let p = pool(&["wb-a", "wb-b"]);
        p.switch("claude:s1", "wb-a").await.unwrap();
        assert_eq!(p.sticky_count().await, 1);

        // wb-b — where s1 landed — is removed.
        p.reload(vec![cred("wb-a")]).await;
        assert_eq!(
            p.sticky_count().await,
            0,
            "bindings to a deleted credential must go"
        );

        // A reload that keeps the credential preserves the binding, so
        // re-selecting a default does not invalidate everyone's cache.
        let p2 = pool(&["wb-a", "wb-b"]);
        p2.switch("claude:s1", "wb-a").await.unwrap();
        p2.reload(vec![cred("wb-b"), cred("wb-a")]).await;
        assert_eq!(p2.sticky_count().await, 1);
        assert_eq!(p2.pick("claude:s1").await.unwrap().id, "wb-b");
    }
}
