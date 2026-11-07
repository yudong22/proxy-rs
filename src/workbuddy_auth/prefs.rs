//! See the module table in `workbuddy_auth/mod.rs` for what lives here.

use super::*;

/// Persisted pool-wide preferences: which credential is the default and the
/// daily check-in schedule.
///
/// Kept in its own file rather than on each credential so "the default" is a
/// single value that cannot disagree with itself, and so the scheduler settings
/// survive a credential being deleted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolPreferences {
    /// Id of the credential every healthy session uses. Empty means "first
    /// usable in store order", the pre-1.8.2 behaviour.
    #[serde(default)]
    pub default_credential_id: String,
    /// Local wall-clock time to run the daily check-in, `HH:MM` (24h).
    #[serde(default = "default_checkin_time")]
    pub daily_checkin_time: String,
    /// Whether the backend scheduler runs the check-in at all.
    #[serde(default)]
    pub daily_checkin_enabled: bool,
    /// Local calendar day the scheduler last ran, so a restart inside the same
    /// day does not claim twice.
    #[serde(default)]
    pub last_checkin_run_date: Option<String>,
    /// Id of the default upstream API key in the key pool. Empty means the
    /// single `GuiSettings::api_key` field is used.
    #[serde(default)]
    pub default_key_id: String,
    /// Unified default identity: either a credential id (`wb-…`) or a key id
    /// (`k-…`). Empty means "first usable credential, else first enabled key,
    /// else the single `GuiSettings::api_key`", the pre-1.8.5 behaviour. A
    /// literal `"__none__"` disables the account pool and forces the key/static
    /// path. This field is what the unified 身份池 selector writes; the two
    /// legacy `*_id` fields are retained only for first-run migration.
    #[serde(default)]
    pub default_identity_id: String,
    /// The GUI's full identity order, credentials and API keys interleaved.
    ///
    /// The two stores live in separate files, so neither one's array order can
    /// express "account A, then key K, then account B" — the single ordering the
    /// pool GUI shows and the user drags. This list is that one order; it is
    /// consulted by [`ordered_identities`].
    ///
    /// Deliberately advisory: an id that no longer exists is skipped, and any
    /// identity missing from the list is appended in store order. A stale list
    /// therefore degrades to "the old ordering for the entries it still names"
    /// rather than hiding accounts.
    #[serde(default)]
    pub identity_order: Vec<String>,
    /// Whether the scheduler runs the growth-task automation at all.
    ///
    /// **Off by default.** The automation calls the growth endpoints
    /// repeatedly for every account; enabling it is an explicit opt-in rather
    /// than something a fresh install does behind the user's back.
    #[serde(default)]
    pub growth_enabled: bool,
    /// Local calendar day (`YYYY-MM-DD`) the growth automation last ran, so a
    /// restart inside the same day does not run it twice.
    #[serde(default)]
    pub last_growth_run_date: Option<String>,
    /// Local calendar day the *night-only* pass (`black_cat`) last ran.
    ///
    /// Tracked separately from [`Self::last_growth_run_date`] because those are
    /// two different windows: the day pass must not consume the night pass's
    /// slot, and the night pass only applies inside 23:00–08:00.
    #[serde(default)]
    pub last_night_growth_date: Option<String>,
}

fn default_checkin_time() -> String {
    "09:00".to_string()
}

/// The default preferences, matching what an empty `workbuddy-pool.json` decodes
/// to.
///
/// Hand-written rather than derived, and the difference is a real bug rather
/// than tidiness: `#[serde(default = "default_checkin_time")]` applies only to
/// *deserialization*, so a derived `Default` left `daily_checkin_time` as `""`.
/// [`load_preferences`] returns `Default::default()` whenever the file is absent
/// — i.e. on every fresh install — so the scheduler ran with an empty time
/// (harmless, because `normalize_checkin_time` falls back to 09:00) while
/// `/api/scheduler` published `"cadence": {"at": ""}`, an empty wall-clock time
/// that no consumer can render. Both the behaviour and the reported fact must
/// come from the same default.
impl Default for PoolPreferences {
    fn default() -> Self {
        Self {
            default_credential_id: String::new(),
            daily_checkin_time: default_checkin_time(),
            daily_checkin_enabled: false,
            last_checkin_run_date: None,
            default_key_id: String::new(),
            default_identity_id: String::new(),
            identity_order: Vec::new(),
            growth_enabled: false,
            last_growth_run_date: None,
            last_night_growth_date: None,
        }
    }
}

/// Pool preferences file path: `~/.proxy-rs/workbuddy-pool.json`.
pub fn preferences_path() -> PathBuf {
    crate::settings::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("workbuddy-pool.json")
}

/// Load the pool preferences. A missing or unreadable file is the default, not
/// an error — every field has a sane fallback.
pub fn load_preferences() -> PoolPreferences {
    let Ok(text) = std::fs::read_to_string(preferences_path()) else {
        return PoolPreferences::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// What one on-disk pool file looks like right now.
///
/// The `load_*` accessors deliberately degrade to "empty" on any failure — a
/// corrupt file must not stop the proxy from serving. That is right for the
/// request path, but it makes "the file is broken" and "the pool is empty"
/// indistinguishable to a caller, and the account-pool export is exactly the
/// caller that must not confuse them: it would otherwise write a valid-looking
/// bundle containing nothing and let the user carry it to another machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreState {
    /// No file on disk. Reading it as empty is correct, not a failure.
    Missing,
    /// Present and parseable.
    Ok,
    /// Present but unreadable or unparseable; the store currently reads as
    /// empty, and anything derived from it is silently incomplete.
    Corrupt(String),
}

impl StoreState {
    /// Whether this state means data is being lost on read.
    pub fn is_corrupt(&self) -> bool {
        matches!(self, StoreState::Corrupt(_))
    }
}

/// Inspect one store file without changing it.
///
/// `pub(crate)` because `client_accounts.rs` describes its own file through the
/// same rule: the export path iterates one health list, and a second copy of
/// this logic would let "corrupt" mean two different things in two places.
pub(crate) fn inspect_store_for<T: serde::de::DeserializeOwned>(
    path: std::path::PathBuf,
) -> StoreState {
    inspect_store::<T>(path)
}

/// Inspect one store file without changing it.
fn inspect_store<T: serde::de::DeserializeOwned>(path: std::path::PathBuf) -> StoreState {
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => StoreState::Missing,
        Err(e) => StoreState::Corrupt(format!("读取失败: {e}")),
        Ok(text) => match serde_json::from_str::<T>(&text) {
            Ok(_) => StoreState::Ok,
            Err(e) => StoreState::Corrupt(format!("解析失败: {e}")),
        },
    }
}

/// Health of every file that makes up the identity pool, as `(名称, 状态)`.
///
/// Read-only and side-effect free: it exists so a caller can tell the user that
/// a store is unreadable *before* acting as if it were empty.
pub fn store_health() -> Vec<(&'static str, StoreState)> {
    vec![
        (
            "账号池",
            inspect_store::<Vec<WorkBuddyCredential>>(credentials_path()),
        ),
        ("密钥池", inspect_store::<Vec<ApiKeyEntry>>(api_keys_path())),
        (
            "偏好设置",
            inspect_store::<PoolPreferences>(preferences_path()),
        ),
    ]
}

/// Persist the pool preferences.
pub fn save_preferences(prefs: &PoolPreferences) -> Result<()> {
    let path = preferences_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(prefs)?)?;
    Ok(())
}

/// Rewrite just the scheduling fields, leaving the selection fields alone.
///
/// The GUI saves the schedule from one place and the default selection from
/// another; going through the whole struct each time would let a stale copy of
/// the other half clobber it.
pub fn save_schedule(enabled: bool, time: &str) -> Result<PoolPreferences> {
    let mut prefs = load_preferences();
    prefs.daily_checkin_enabled = enabled;
    prefs.daily_checkin_time = normalize_checkin_time(time);
    save_preferences(&prefs)?;
    Ok(prefs)
}

/// Clamp a user-entered `HH:MM` to a valid wall-clock time.
///
/// Anything unparseable falls back to the 09:00 default rather than being
/// stored: an invalid time would either disable the scheduler silently or panic
/// the timer's duration arithmetic.
pub fn normalize_checkin_time(raw: &str) -> String {
    let raw = raw.trim();
    let Some((h, m)) = raw.split_once(':') else {
        return default_checkin_time();
    };
    let Ok(h) = h.trim().parse::<u32>() else {
        return default_checkin_time();
    };
    // Trailing seconds ("09:00:00") are accepted and ignored.
    let m = m.split(':').next().unwrap_or("").trim();
    let Ok(m) = m.parse::<u32>() else {
        return default_checkin_time();
    };
    if h > 23 || m > 59 {
        return default_checkin_time();
    }
    format!("{:02}:{:02}", h, m)
}

/// Local calendar day string (`YYYY-MM-DD`) for `now_ms`.
///
/// The check-in is a *daily* allowance in the user's own timezone, so the day
/// boundary must be local — a UTC day would let an evening claim count as the
/// next morning's.
pub fn local_day(now_ms: i64) -> String {
    let secs = now_ms / 1000;
    let offset = crate::util::local_utc_offset_secs();
    let local = secs + offset;
    let days = local.div_euclid(86_400);
    let (y, mo, d) = crate::util::civil_from_days(days);
    format!("{:04}-{:02}-{:02}", y, mo, d)
}

/// Milliseconds from now until the next local wall-clock `HH:MM`.
///
/// Always strictly positive: a time that has just passed schedules for
/// tomorrow, so a check-in set to the current minute fires once rather than
/// spinning.
pub fn millis_until_daily_time(now_ms: i64, time: &str) -> i64 {
    let time = normalize_checkin_time(time);
    let (h, m) = time.split_once(':').unwrap_or(("9", "0"));
    let h: i64 = h.parse().unwrap_or(9);
    let m: i64 = m.parse().unwrap_or(0);
    let target_of_day = h * 3_600_000 + m * 60_000;

    let offset = crate::util::local_utc_offset_secs() * 1000;
    let local_ms = now_ms + offset;
    let day_start = local_ms.div_euclid(86_400_000) * 86_400_000;
    let mut target_local = day_start + target_of_day;
    if target_local <= local_ms {
        target_local += 86_400_000;
    }
    // Back to epoch milliseconds; the result is a duration, so the offset
    // cancels out.
    let delta = target_local - local_ms;
    delta.max(1_000)
}

pub(crate) fn default_true() -> bool {
    true
}

/// Set the default credential id, validating it exists.
pub fn set_default_credential(id: &str) -> Result<PoolPreferences> {
    let mut prefs = load_preferences();
    prefs.default_credential_id = if id.is_empty() {
        String::new()
    } else {
        let items = load_credentials();
        items
            .iter()
            .find(|c| c.id == id)
            .map(|_| id.to_string())
            .ok_or_else(|| anyhow::anyhow!("账号不存在: {}", id))?
    };
    save_preferences(&prefs)?;
    Ok(prefs)
}

/// Set the default API key id, validating it exists.
pub fn set_default_api_key(id: &str) -> Result<PoolPreferences> {
    let mut prefs = load_preferences();
    prefs.default_key_id = if id.is_empty() {
        String::new()
    } else {
        let items = load_api_keys();
        items
            .iter()
            .find(|k| k.id == id)
            .map(|_| id.to_string())
            .ok_or_else(|| anyhow::anyhow!("密钥不存在: {}", id))?
    };
    save_preferences(&prefs)?;
    Ok(prefs)
}

/// The literal value the unified selector stores to mean "no account pool; use
/// the key/static path". Credentials always start with `wb-` and keys with `k-`,
/// so this sentinel can never collide with a real id.
pub const NO_ACCOUNT_POOL_SENTINEL: &str = "__none__";

/// Resolve the unified default identity: which credential id (or key id, or
/// "none") the unified selector last chose. Falls back to the legacy separate
/// fields when the unified field has not been set yet, so an upgraded install
/// keeps behaving as before until the user touches the new control.
pub fn resolve_default_identity() -> String {
    let prefs = load_preferences();
    if !prefs.default_identity_id.is_empty() {
        return prefs.default_identity_id.clone();
    }
    // Migration: honour the old separate defaults. A selected account wins over
    // a selected key, matching the 1.8.4 behaviour where a pool default account
    // and a key default could both be set independently. Once the user picks in
    // the new UI this branch is never taken again.
    if !prefs.default_credential_id.is_empty() {
        return prefs.default_credential_id.clone();
    }
    if !prefs.default_key_id.is_empty() {
        return prefs.default_key_id.clone();
    }
    String::new()
}

/// The identity actually in force right now, including first-usable fallbacks.
///
/// Returns `"__none__"` when the account pool is explicitly disabled, a
/// credential id (`wb-…`) when an account is the default, a key id (`k-…`) when
/// a key is, or `""` when nothing in the pools is used (the static
/// `GuiSettings::api_key` path). The unified 身份池 selector uses this to mark
/// the "default" badge, mirroring exactly the resolution the proxy request path
/// applies.
pub fn effective_default_identity() -> String {
    let prefs = load_preferences();
    if !prefs.default_identity_id.is_empty() {
        return prefs.default_identity_id.clone();
    }
    let creds = load_credentials();
    if let Some(c) = creds
        .iter()
        .find(|c| c.is_usable(crate::util::unix_millis()))
    {
        return c.id.clone();
    }
    let keys = load_api_keys();
    if let Some(k) = keys.iter().find(|k| k.enabled && !k.key.trim().is_empty()) {
        return k.id.clone();
    }
    String::new()
}

/// Set the unified default identity id. Validates that the id names a real
/// credential or key (or the explicit "none" sentinel); an empty string clears
/// it back to the first-usable fallback.
pub fn set_default_identity(id: &str) -> Result<PoolPreferences> {
    let mut prefs = load_preferences();
    prefs.default_identity_id = if id.is_empty() || id == NO_ACCOUNT_POOL_SENTINEL {
        id.to_string()
    } else if id.starts_with("wb-") {
        let items = load_credentials();
        items
            .iter()
            .find(|c| c.id == id)
            .map(|_| id.to_string())
            .ok_or_else(|| anyhow::anyhow!("账号不存在: {}", id))?;
        id.to_string()
    } else if id.starts_with("k-") {
        let items = load_api_keys();
        items
            .iter()
            .find(|k| k.id == id)
            .map(|_| id.to_string())
            .ok_or_else(|| anyhow::anyhow!("密钥不存在: {}", id))?
    } else {
        anyhow::bail!("未知的默认身份类型: {}", id)
    };
    save_preferences(&prefs)?;
    Ok(prefs)
}

/// Reorder `items` to follow [`PoolPreferences::identity_order`].
///
/// Ids named in the order keep their listed position; anything not listed is
/// appended in its original (store) order. Stale ids — an account deleted since
/// the last drag — are simply absent and nothing else shifts, so a stale order
/// degrades to "the remembered prefix, then the rest as before" rather than
/// dropping entries from the pool.
pub(crate) fn apply_identity_order<T, F>(items: Vec<T>, order: &[String], id_of: F) -> Vec<T>
where
    F: Fn(&T) -> &str,
{
    if order.is_empty() {
        return items;
    }
    let rank = |item: &T| -> usize {
        let id = id_of(item);
        order.iter().position(|x| x == id).unwrap_or(usize::MAX)
    };
    // `sort_by_key` is stable, so everything not named in `order` keeps its
    // store order at the tail instead of shuffling unpredictably.
    let mut ranked: Vec<(usize, T)> = items.into_iter().map(|i| (rank(&i), i)).collect();
    ranked.sort_by_key(|(r, _)| *r);
    ranked.into_iter().map(|(_, i)| i).collect()
}

/// Persist the growth-automation switch, leaving every other preference alone.
///
/// Separate from [`save_schedule`] because the two switches are independent: a
/// user may want automatic check-in but not the growth automation (it calls the
/// upstream far more often). Returns the stored preferences so the caller can
/// echo them back to the window.
pub fn save_growth_enabled(enabled: bool) -> Result<PoolPreferences> {
    let mut prefs = load_preferences();
    prefs.growth_enabled = enabled;
    save_preferences(&prefs)?;
    Ok(prefs)
}

/// Credentials in the order the pool should try them.
///
/// Order = the GUI's identity order first (which is what a drag writes), then
/// the pinned default hoisted to the front, then store order.
///
/// Ordering *is* the default mechanism ([`crate::session_pool::CredentialPool`]
/// picks the first usable entry), so selecting a default is a reorder at load
/// time rather than a new field the request path must check.
pub fn ordered_credentials() -> Vec<WorkBuddyCredential> {
    let prefs = load_preferences();
    // The GUI's interleaved order is the primary statement of intent; it is what
    // a drag writes and what the user sees.
    let mut items =
        apply_identity_order(load_credentials(), &prefs.identity_order, |c| c.id.as_str());
    // The unified 身份池 selector is the source of truth when it pins a
    // credential; the legacy `default_credential_id` is only consulted for
    // installs that predate it. A key/`__none__` selection must not reorder the
    // credential pool at all.
    let target = if prefs.default_identity_id.starts_with("wb-") {
        prefs.default_identity_id.clone()
    } else {
        prefs.default_credential_id.clone()
    };
    if target.is_empty() {
        return items;
    }
    let Some(pos) = items.iter().position(|c| c.id == target) else {
        return items;
    };
    let chosen = items.remove(pos);
    items.insert(0, chosen);
    items
}

/// After a drag reorder, the identity the default should follow to, or `None`
/// if the gesture should leave the default alone.
///
/// `ordered_credentials` hoists the pinned default to the front, so a drag that
/// puts another identity at the top would be silently reverted on reload. The
/// one unambiguous intent to change the default is dragging a row to the top,
/// so this returns `top` only when:
///
/// * the current default is a credential pinned in the credential list, and
/// * the drag actually put a *different* identity first — otherwise the default
///   is already where it belongs and an unrelated drag must not move it.
///
/// A key default (`k-…`, or a legacy `default_key_id`) is deliberately excluded:
/// keys are never hoisted into [`ordered_credentials`], so an account drag
/// cannot revert it, and the key's position among *keys* comes from
/// [`apply_identity_order`](crate::workbuddy_auth::apply_identity_order)
/// independently.
///
/// The legacy `default_credential_id` counts as "current" only when the unified
/// selector has never been set: it is what `ordered_credentials` hoists on such
/// installs. A key (`k-…`) or `__none__` selection is authoritative and must not
/// be shadowed by a stale legacy field — that is exactly how a key default would
/// otherwise be silently replaced by whichever account was dragged to the top.
pub fn reorder_pin_target(prefs: &PoolPreferences, ids: &[String]) -> Option<String> {
    let top = ids.first()?;
    let current = if !prefs.default_identity_id.is_empty() {
        prefs.default_identity_id.as_str()
    } else {
        prefs.default_credential_id.as_str()
    };
    if !current.starts_with("wb-") || current == top {
        None
    } else {
        Some(top.clone())
    }
}
