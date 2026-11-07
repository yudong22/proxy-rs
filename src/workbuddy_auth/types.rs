//! See the module table in `workbuddy_auth/mod.rs` for what lives here.

use super::*;

/// One WorkBuddy login-state credential.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// When a token refresh was **rejected** (not merely failed transiently),
    /// epoch millis. `None` means no rejection is on record.
    ///
    /// Needed because "does this account need me to log in again?" cannot be
    /// answered from `last_error` alone: a network blip and a revoked refresh
    /// token both write one. Only the rejection case cannot self-heal, and it
    /// must survive a restart — otherwise the GUI badge would forget an account
    /// that is permanently stranded, and the user would only discover it when a
    /// request failed.
    ///
    /// Cleared by a successful refresh or a re-login, so a recovered account
    /// stops asking for attention.
    #[serde(default)]
    pub relogin_required_since_ms: Option<i64>,
    /// Remaining points (积分) as last queried. `None` until a refresh reads
    /// them, so the UI can distinguish "unknown" from a real zero balance.
    #[serde(default)]
    pub points: Option<f64>,
    /// When `points` was last fetched, milliseconds since the epoch.
    #[serde(default)]
    pub points_fetched_at_ms: Option<i64>,
    /// Local calendar day (`YYYY-MM-DD`) this credential last claimed its daily
    /// check-in. Drives both the "already checked in" display and the scheduler's
    /// once-a-day guard.
    #[serde(default)]
    pub last_checkin_date: Option<String>,
    /// When this credential's access token was last renewed, epoch millis.
    ///
    /// Needed because most imported login states carry **no `expiresAt`**: the
    /// desktop file often omits it, so [`Self::needs_refresh`] has nothing to
    /// compare against and proactive renewal can never fire for exactly the
    /// accounts that need it most. This is the timestamp the conservative
    /// [`UNKNOWN_EXPIRY_REFRESH_AFTER_MS`] backstop measures from.
    #[serde(default)]
    pub last_refresh_at_ms: Option<i64>,
}

/// The account block of a login state. Only the fields the upstream headers
/// need are kept; everything else in the desktop file is ignored. The desktop
/// file spells the fields camelCase (`enterpriseId`), so the aliases matter.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkBuddyAccount {
    #[serde(default)]
    pub uid: String,
    #[serde(default, alias = "enterprise_id")]
    pub enterprise_id: String,
    #[serde(default)]
    pub nickname: String,
}

/// How long an access token with **unknown** expiry is trusted before the
/// proactive path renews it anyway.
///
/// Chosen from the observed shape of real logins rather than from the vendor's
/// documented lifetime: WorkBuddy login states are issued for hours, not
/// minutes, so six hours is comfortably inside a token's life and still makes a
/// long-running app renew well before it would otherwise eat a 401. Raising this
/// reduces refresh traffic at the cost of a later first-401; lowering it does the
/// reverse. It never overrides a *known* `expiresAt`.
pub const UNKNOWN_EXPIRY_REFRESH_AFTER_MS: i64 = 6 * 60 * 60 * 1000;

impl WorkBuddyCredential {
    /// Whether the credential is in a rate-limit cooldown right now.
    pub fn is_cooling_down(&self, now_ms: i64) -> bool {
        matches!(self.cooldown_until_ms, Some(until) if now_ms < until)
    }

    /// Whether the access token should be renewed *before* spending a request on
    /// it.
    ///
    /// Two independent triggers:
    ///
    /// 1. **Known expiry** — within the 60 s renewal margin, matching the
    ///    reference client's threshold. Exact, so it costs nothing extra.
    /// 2. **Unknown expiry backstop** — most imported login states carry no
    ///    `expiresAt` at all, and a token whose expiry we cannot see is not a
    ///    token that never expires. Without this branch those accounts could only
    ///    ever be renewed *after* a 401 had already failed a user request.
    ///    [`UNKNOWN_EXPIRY_REFRESH_AFTER_MS`] is deliberately conservative (only
    ///    once the last renewal is old enough that the token is very likely
    ///    stale), because guessing too eagerly would spend a refresh round-trip
    ///    per request.
    ///
    /// A credential that has never been refreshed and has no known expiry is
    /// *not* refreshed here: nothing says a freshly imported token is stale, and
    /// the reactive 401 path already covers it.
    pub fn needs_refresh(&self, now_ms: i64) -> bool {
        if !self.enabled || self.refresh_token.is_none() {
            return false;
        }
        match self.expires_at_ms {
            Some(exp) => now_ms >= exp - 60_000,
            None => match self.last_refresh_at_ms {
                Some(last) => now_ms.saturating_sub(last) >= UNKNOWN_EXPIRY_REFRESH_AFTER_MS,
                None => false,
            },
        }
    }

    /// Whether the access token is still believed valid: not expired (with a
    /// 60-second margin, matching the reference client's renewal threshold)
    /// and not in a rate-limit cooldown.
    pub fn is_usable(&self, now_ms: i64) -> bool {
        if !self.enabled {
            return false;
        }
        if self.is_cooling_down(now_ms) {
            return false;
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
        mask_secret(&self.access_token)
    }
}

/// Masked display form shared by credentials and API keys (`sk-a…wxyz`).
///
/// Slicing is done on *character* boundaries rather than byte offsets: the
/// obvious `&t[..4]` / `&t[len-4..]` panics as soon as a multi-byte character
/// straddles byte 4 or `len - 4`, and the release profile is
/// `panic = "abort"` — one non-ASCII token in the list would take the whole
/// desktop app down rather than merely failing to render. The length guard is
/// in characters for the same reason.
pub(crate) fn mask_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 8 {
        return "••••".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}••••{tail}")
}

/// Derive a stable short credential id from the token material.
///
/// **Superseded by [`credential_id_for`]** and kept only as the fallback for a
/// login state that carries no account identity (see below). Prefer
/// [`identity_key`] for anything that must survive a token refresh.
///
/// The desktop token is opaque, so the id is a content hash: the same login
/// state imported twice collapses to one id, and the id stays stable across
/// restarts without a counter file. Six hex characters is enough to avoid
/// collisions at any realistic pool size and short enough for log lines.
///
/// The flaw this cannot avoid: the *token* rotates on every refresh, so a hash
/// of it is not a stable identity. A credential that has been refreshed once no
/// longer hashes to its stored id, which is why re-logging in the same account
/// used to add a second row instead of replacing the first.
pub fn credential_id(access_token: &str) -> String {
    short_id("wb", access_token.as_bytes())
}

/// The stable identity of an account: `uid` + `enterpriseId`.
///
/// This — not the access token — is what identifies a WorkBuddy account across
/// its lifetime. A token refresh rotates `accessToken` but leaves the account
/// untouched, so an id derived from this pair survives every renewal and makes
/// re-login an *update* of the existing entry rather than a new one.
///
/// `enterpriseId` is part of the key because the same `uid` can exist in
/// different enterprises (a personal and a company login), and collapsing those
/// onto one id would silently merge two accounts.
///
/// Returns `None` for a login state with no `uid` — some imported fixtures carry
/// none — and the caller falls back to the token hash.
pub fn identity_key(account: &WorkBuddyAccount) -> Option<String> {
    let uid = account.uid.trim();
    if uid.is_empty() {
        return None;
    }
    let enterprise = account.enterprise_id.trim();
    Some(format!("{uid}\u{1f}{enterprise}"))
}

/// The id for a login state, preferring the stable account identity and falling
/// back to the token hash only when the account carries no `uid`.
pub fn credential_id_for(account: &WorkBuddyAccount, access_token: &str) -> String {
    match identity_key(account) {
        Some(key) => short_id("wb", key.as_bytes()),
        None => credential_id(access_token),
    }
}

/// `prefix-<6 hex>` over arbitrary bytes, the one place the id shape is defined.
fn short_id(prefix: &str, material: &[u8]) -> String {
    use std::fmt::Write;
    let digest = md5_hex(material);
    let mut out = String::with_capacity(prefix.len() + 7);
    out.push_str(prefix);
    out.push('-');
    for byte in &digest[..3] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// MD5 hex digest without pulling a crypto dependency: this is a *fingerprint*,
/// not a secret derivation — the token material is already a secret on disk.
///
/// `pub(crate)` because [`crate::pool_transfer`] reuses the same digest for the
/// export bundle's integrity checksum: that checksum exists to catch a
/// hand-edited or truncated file, not to resist an attacker, so MD5 is the
/// right tool and a second hash implementation would be dead weight.
pub(crate) fn md5_hex(data: &[u8]) -> [u8; 16] {
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

    // `msg` was padded above to a whole number of 64-byte blocks, so the
    // remainder is always empty; `as_chunks` states that fixed block size in the
    // type instead of leaving it as a runtime invariant. (Clippy 1.98's
    // `chunks_exact_to_as_chunks` prefers this over `chunks_exact`.)
    let (blocks, remainder) = msg.as_chunks::<64>();
    debug_assert!(
        remainder.is_empty(),
        "msg must be padded to a multiple of 64 bytes"
    );
    for chunk in blocks {
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
        id: credential_id_for(&account, access),
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
        // A freshly imported login state has not been rejected yet.
        relogin_required_since_ms: None,
        points: None,
        points_fetched_at_ms: None,
        last_checkin_date: None,
        // A freshly imported token has just been minted by the client, so the
        // unknown-expiry backstop starts counting now rather than from zero.
        last_refresh_at_ms: Some(crate::util::unix_millis()),
    })
}

/// The auth endpoint to refresh this credential against.
///
/// The stored `domain` is the authority the login state was issued for, so the
/// token must be renewed there; an empty/unknown domain falls back to the
/// domestic default. Accepts a bare host or a full URL.
pub fn auth_endpoint_for(credential: &WorkBuddyCredential) -> String {
    let domain = credential.domain.trim();
    if domain.is_empty() {
        DEFAULT_WORKBUDDY_ENDPOINT.to_string()
    } else if domain.starts_with("http://") || domain.starts_with("https://") {
        domain.trim_end_matches('/').to_string()
    } else {
        format!("https://{}", domain.trim_end_matches('/'))
    }
}
