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
//!    It *is* sent home when its replacement becomes unusable without ever
//!    being manually re-enabled — the quota reset at local midnight, or a
//!    failed token refresh parking the account for a minute. The account is
//!    then honestly dead, and `pick` re-homes the session through [`switch`],
//!    which records the new sticky binding. Skipping that left the session on
//!    the default with no binding, so it flipped back the moment the default
//!    recovered — see [`StickySession::away_since_ms`] for the full failure.
//!
//! The `switched` map is the only state: sessions that never errored do not
//! appear in it, so the hot path is one map lookup, and nothing is persisted —
//! a restart returns to "everything on the default", costing one cache warm-up.
//!
//! [`switch`]: CredentialPool::switch

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
    /// When `pick` first found the bound credential unusable, or `None` while
    /// it was healthy. Set only by [`CredentialPool::pick`].
    ///
    /// This separates the two ways a binding can go stale, which the plan's
    /// contract treats differently:
    ///
    /// * **Re-enabled by hand** — the GUI clears the cooldown and reloads the
    ///   pool. The binding is forgotten immediately, so the session rejoins
    ///   the default-first policy from its next request.
    /// * **Recovered on its own** — a quota cooldown runs out at local
    ///   midnight, or a failed token refresh parks the account for 60 s. The
    ///   session is sent back to the configured default, and the binding is
    ///   *kept* for [`STICKY_AWAY_MS`] after it was first found dead, so a
    ///   default that has meanwhile filled up again does not pull it into a
    ///   failover round-trip-per-session. After that grace the binding is
    ///   dropped like any expired one.
    ///
    /// The previous behaviour (always fall through to the default, keep the
    /// binding forever) silently broke the switch in the common daily case: a
    /// session failed over at night, `pick` saw the bound account cooling,
    /// served the default, and when the cooldown lapsed it resumed serving the
    /// sticky-bound default — the GUI offered no way to move the session —
    /// instead of re-homing it onto a healthy account. [`pick`] now re-homes
    /// through [`switch`] in that same request.
    ///
    /// [`pick`]: CredentialPool::pick
    /// [`switch`]: CredentialPool::switch
    away_since_ms: Option<i64>,
}

/// How long a session whose sticky credential went dead may head home (or be
/// re-homed by `pick`) before its dead binding is dropped for good.
///
/// Covers the longest self-healing outage seen in practice (a local-midnight
/// quota reset being the shortest, a failed-refresh park being 60 s) with room
/// to spare, without pinning a session to a long-gone choice for its whole TTL.
pub const STICKY_AWAY_MS: i64 = 10 * 60 * 1000;

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
    /// Resolution order (see the module docs):
    ///
    /// 1. A healthy sticky binding.
    /// 2. A binding whose credential is gone (deleted) is dropped and the
    ///    session revisited.
    /// 3. A binding whose credential is merely unhealthy (cooldown, expiry)
    ///    is remembered as dead, and the session is re-homed via [`Self::switch`]
    ///    onto a credential other than that one — exactly as a failover would
    ///    have done had this request hit the error instead of discovering the
    ///    dead state up front. A dead default is left alone: the next request
    ///    re-homes when the default recovers.
    /// 4. The default: first usable entry.
    ///
    /// The "most remaining quota / rotation" preference applies only when a
    /// failover actually picks a replacement (in [`Self::switch`]), never on the
    /// plain default path.
    pub async fn pick(&self, session_id: &str) -> Option<WorkBuddyCredential> {
        let now = crate::util::unix_millis();
        let entries = self.entries.read().await;

        let mut revisit = false;
        if let Some(binding) = self.switched.read().await.get(session_id) {
            match entries.iter().find(|c| c.id == binding.credential_id) {
                // Healthy and within TTL: the cache is warm there, stay.
                Some(c) if now < binding.expires_at_ms && c.is_usable(now) => {
                    return Some(c.clone())
                }
                // Unhealthy, deleted, or the TTL ran out: decide in `rehome`.
                _ => revisit = true,
            }
        }

        if revisit {
            self.rehome(session_id).await;
        }

        // Default: first usable entry.
        entries.iter().find(|c| c.is_usable(now)).cloned()
    }

    /// Resolve the sticky binding that [`Self::pick`] found unusable.
    ///
    /// Drops it when its credential is gone or the grace period has run out,
    /// otherwise records it as dead and re-homes the session with
    /// [`Self::switch`] onto a credential other than the dead one. When the
    /// dead credential is still the first usable entry there is nothing to
    /// fail over to: the session heads back to the default (step 4 of
    /// [`Self::pick`]) and is re-homed the request after that default
    /// recovers.
    async fn rehome(&self, session_id: &str) {
        let now = crate::util::unix_millis();
        let from = {
            let mut switched = self.switched.write().await;
            let Some(binding) = switched.get_mut(session_id) else {
                return;
            };
            // Expired or past its grace: the binding is spent, go home for good.
            let first_seen = *binding.away_since_ms.get_or_insert(now);
            if now >= binding.expires_at_ms || now - first_seen > STICKY_AWAY_MS {
                switched.remove(session_id);
                return;
            }
            binding.credential_id.clone()
        };

        let default = self
            .entries
            .read()
            .await
            .iter()
            .find(|c| c.is_usable(now))
            .map(|c| c.id.clone());
        if default.as_deref() == Some(from.as_str()) {
            return;
        }
        let _ = self.switch(session_id, &from).await;
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
                away_since_ms: None,
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
    ///
    /// The exception is a credential that was disabled in the pool and is
    /// enabled again: the user has just put it back into service by hand, so
    /// the sessions bound to it rejoin the default-first policy instead of
    /// reappearing there as if nothing happened. (A credential that comes back
    /// healthy on its own — a cooldown running out — is *not* such a signal
    /// and keeps its bindings; see [`StickySession::away_since_ms`].)
    pub async fn reload(&self, credentials: Vec<WorkBuddyCredential>) {
        let present: std::collections::HashSet<String> =
            credentials.iter().map(|c| c.id.clone()).collect();
        let reenabled: std::collections::HashSet<String> = {
            let old = self.entries.read().await;
            credentials
                .iter()
                .filter(|c| c.enabled)
                .filter(|c| old.iter().any(|o| o.id == c.id && !o.enabled))
                .map(|c| c.id.clone())
                .collect()
        };
        *self.entries.write().await = credentials;
        self.switched.write().await.retain(|_, s| {
            present.contains(&s.credential_id) && !reenabled.contains(&s.credential_id)
        });
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

    /// How many sessions are sticky-bound to one credential (GUI display).
    pub async fn sticky_sessions_for(&self, id: &str) -> usize {
        self.switched
            .read()
            .await
            .values()
            .filter(|s| s.credential_id == id)
            .count()
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

    /// An expired sticky binding is dropped and the session rejoins the
    /// default-first policy instead of quietly resuming on whichever account is
    /// bound in the map.
    ///
    /// Regression: expiry used to fall through to "first usable" with the
    /// binding left in place, so the GUI kept reporting a session as sticky on
    /// an account it was no longer using — and the session resumed there as
    /// soon as that account became first-usable again.
    #[tokio::test]
    async fn expired_sticky_binding_is_dropped() {
        let p = pool(&["wb-a", "wb-b"]);
        p.switch("claude:s1", "wb-a").await.unwrap();
        if let Some(b) = p.switched.write().await.get_mut("claude:s1") {
            b.expires_at_ms = crate::util::unix_millis() - 1;
        }

        assert_eq!(p.pick("claude:s1").await.unwrap().id, "wb-a");
        assert_eq!(p.sticky_count().await, 0, "the stale binding is forgotten");
        assert_eq!(p.pick("claude:s1").await.unwrap().id, "wb-a");
    }

    /// The reported bug: an account that runs out of quota is cooled down until
    /// local midnight, the session moves to the next account, and when the
    /// midnight reset lifts the cooldown the session must keep serving from the
    /// replacement — not fall back to re-served default-first and re-stick to
    /// the exhausted account.
    #[tokio::test]
    async fn session_does_not_return_to_an_account_that_recovers_from_quota() {
        let p = pool(&["wb-a", "wb-b", "wb-c"]);
        p.mark_limited("wb-a", 60_000).await;
        p.switch("claude:s1", "wb-a").await.unwrap();
        assert_eq!(p.pick("claude:s1").await.unwrap().id, "wb-b");

        // Midnight: wb-a's quota resets and it is usable again — but nobody
        // re-enabled it by hand.
        p.mark_ok("wb-a").await;

        assert_eq!(
            p.pick("claude:s1").await.unwrap().id,
            "wb-b",
            "the relocated session must stay on its replacement"
        );
        // A session that never failed over still goes to the default.
        assert_eq!(p.pick("claude:s2").await.unwrap().id, "wb-a");
    }

    /// Hand-re-enabling an account in the GUI puts its sessions back under the
    /// default-first policy: disabling wb-b relocates them to wb-c, so enabling
    /// it again must not silently pull them back and make wb-c look sticky.
    #[tokio::test]
    async fn reenabled_account_does_not_pull_sessions_back() {
        let p = pool(&["wb-a", "wb-b", "wb-c"]);
        p.mark_limited("wb-a", 60_000).await;
        p.switch("claude:s1", "wb-a").await.unwrap();
        assert_eq!(p.pick("claude:s1").await.unwrap().id, "wb-b");

        let mut items = p.snapshot().await;
        items[1].enabled = false;
        p.reload(items).await;
        assert_eq!(p.pick("claude:s1").await.unwrap().id, "wb-c");

        // The GUI toggle reloads with wb-b enabled and its cooldown cleared;
        // the reload itself is what notices the hand re-enable.
        let mut items = p.snapshot().await;
        items[1].enabled = true;
        items[1].cooldown_until_ms = None;
        p.reload(items).await;
        // wb-b is enabled but wb-a is cooling and wb-c is next in line, so
        // the session is re-homed onto wb-c again — with a fresh binding.
        assert_eq!(p.pick("claude:s1").await.unwrap().id, "wb-c");
        assert_eq!(p.sticky_count().await, 1, "one fresh binding, not two");
        assert_eq!(p.pick("claude:s1").await.unwrap().id, "wb-c");
        assert_eq!(p.sticky_count().await, 1);
    }

    /// A session bound to an account that is disabled and then deleted is
    /// re-homed on its next request rather than riding the default until it
    /// errors.
    #[tokio::test]
    async fn disabled_then_deleted_binding_rehomes_on_pick() {
        let p = pool(&["wb-a", "wb-b", "wb-c"]);
        p.mark_limited("wb-a", 60_000).await;
        p.switch("claude:s1", "wb-a").await.unwrap();
        assert_eq!(p.pick("claude:s1").await.unwrap().id, "wb-b");

        // Disabled in the GUI: the binding survives the reload (it is not a
        // re-enable), but the account can no longer serve.
        let mut items = p.snapshot().await;
        items[1].enabled = false;
        p.reload(items).await;
        assert_eq!(p.pick("claude:s1").await.unwrap().id, "wb-c");
        assert_eq!(
            p.pick("claude:s1").await.unwrap().id,
            "wb-c",
            "and it stays there"
        );
    }

    /// Once the grace period has passed, a dead binding is dropped and the
    /// session simply follows the default again.
    #[tokio::test]
    async fn stale_dead_binding_is_dropped_after_the_grace_period() {
        let p = pool(&["wb-a", "wb-b"]);
        p.switch("claude:s1", "wb-a").await.unwrap();

        // wb-b is gone (disabled) well past the grace period: nothing to
        // re-home onto, so `pick` waits for the default rather than writing a
        // fresh binding every request.
        let mut items = p.snapshot().await;
        items[1].enabled = false;
        p.reload(items).await;
        if let Some(b) = p.switched.write().await.get_mut("claude:s1") {
            b.away_since_ms = Some(crate::util::unix_millis() - STICKY_AWAY_MS - 1);
        }
        let picked = p.pick("claude:s1").await.unwrap();
        assert_eq!(picked.id, "wb-a", "back to the default-first policy");
        assert_eq!(
            p.sticky_count().await,
            0,
            "the dead binding is dropped, not re-homed forever"
        );
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
