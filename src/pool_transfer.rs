//! Account-pool transfer: a self-contained bundle that carries the identity
//! pool to another machine.
//!
//! What travels
//! ------------
//! Exactly the three files that make up the 身份池 — `workbuddy-credentials.json`
//! (login states), `api-keys.json` (upstream keys) and the selection/schedule
//! half of `workbuddy-pool.json` — and nothing else. `gui-settings.json` and
//! `.env` are deliberately excluded: the port, bind address, upstream URL and
//! model mapping are *machine* facts, and importing them into a second machine
//! is as likely to break it (port already taken, upstream pointed at a host that
//! does not resolve there) as to help. `stats.db` is excluded too: it is
//! history, not identity, and can be large.
//!
//! The three stores are not rewritten in a new shape — the bundle embeds
//! `WorkBuddyCredential` / `ApiKeyEntry` verbatim, so a field added later with
//! `#[serde(default)]` is automatically carried by an older build's bundle and
//! tolerated by a newer one.
//!
//! Plain or encrypted
//! ------------------
//! The default is a plain JSON document, written 0600. A bundle is a password
//! book — the login states in it are immediately usable — so encryption is
//! offered, but it is opt-in: a passphrase the user forgets turns the file into
//! rubbish, which is a worse failure than a file only they can read. When asked
//! for, the whole payload (tokens, keys, preferences) goes into one AES-256-GCM
//! ciphertext under an Argon2id-derived key, and the envelope keeps only counts
//! and KDF parameters — never a token.
//!
//! Import is non-destructive by construction
//! ----------------------------------------
//! [`preview_import`] is read-only and can be called repeatedly; [`apply_import`]
//! re-decodes the text rather than trusting a previous preview, backs up every
//! file it is about to replace, and writes through a temp file + rename so a
//! failure cannot leave half a pool behind.
//!
//! Layer note: like `credits.rs` and `workbuddy_auth.rs`, this is Layer 3 with
//! I/O — not part of the pure translation core.

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use anyhow::{anyhow, bail, Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::workbuddy_auth::{
    self, api_key_id, credential_id, md5_hex, ApiKeyEntry, PoolPreferences, WorkBuddyCredential,
};

/// Magic value in every bundle. Checked on import so a random JSON file fails
/// with "这不是账号池导出文件" rather than a confusing serde error.
pub const BUNDLE_FORMAT: &str = "proxy-rs-pool";

/// Transfer-format version, deliberately independent of the app version: the
/// bundle layout changes far less often than the app, and a version that moved
/// with the release number would make every upgrade look incompatible.
pub const BUNDLE_VERSION: u32 = 1;

/// Import ceiling. The pools are kilobytes at any realistic size, so anything
/// larger is a mistake or a hostile input, and refusing early keeps a stray
/// multi-megabyte paste from being buffered and parsed.
pub const MAX_BUNDLE_BYTES: usize = 1024 * 1024;

const KIND_PLAIN: &str = "plain";
const KIND_ENCRYPTED: &str = "encrypted";

const KDF_ALGO: &str = "argon2id";
const CIPHER_ALGO: &str = "aes-256-gcm";

/// Argon2id cost: 19 MiB, 2 passes, 1 lane (the OWASP-recommended minimum).
/// Written into the bundle, so raising it later does not invalidate old files.
const KDF_M_COST: u32 = 19_456;
const KDF_T_COST: u32 = 2;
const KDF_P_COST: u32 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

// ── Wire format ────────────────────────────────────────────────────────────

/// How an import treats what is already on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    /// Upsert by id; everything else on this machine is left alone.
    Merge,
    /// The bundle becomes the whole pool; existing entries are dropped.
    Replace,
}

impl ImportMode {
    fn as_str(self) -> &'static str {
        match self {
            ImportMode::Merge => "merge",
            ImportMode::Replace => "replace",
        }
    }

    /// Parse the GUI's `mode` argument. Unknown values are rejected rather than
    /// silently treated as "merge": an import that does the wrong thing to a
    /// credential pool is not something to guess about.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim() {
            "merge" | "" => Ok(ImportMode::Merge),
            "replace" => Ok(ImportMode::Replace),
            other => bail!("未知的导入模式: {other}"),
        }
    }
}

/// The preferences a bundle carries. Every field is `Option` so "the bundle did
/// not say" is distinguishable from "the bundle says: no default (fallback)".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BundlePreferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_identity_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_checkin_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_checkin_time: Option<String>,
}

/// Everything sensitive, in one object: this is what gets encrypted as a unit,
/// so an encrypted bundle's envelope cannot leak a token by accident.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BundlePayload {
    #[serde(default)]
    preferences: BundlePreferences,
    #[serde(default)]
    credentials: Vec<WorkBuddyCredential>,
    #[serde(default)]
    api_keys: Vec<ApiKeyEntry>,
}

/// Plaintext counts, kept outside the ciphertext so an encrypted bundle can be
/// previewed ("2 个账号 / 1 个密钥") before the passphrase is known.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BundleCounts {
    pub credentials: usize,
    pub api_keys: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KdfParams {
    algo: String,
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
    salt_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CipherBlob {
    algo: String,
    nonce_b64: String,
    ct_b64: String,
}

/// The on-disk document. One struct covers both kinds: the plain kind fills
/// `preferences`/`credentials`/`api_keys`, the encrypted kind fills `kdf` and
/// `cipher`, and `kind` says which. Two separate structs would mean two serde
/// definitions that could drift apart.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BundleFile {
    format: String,
    version: u32,
    kind: String,
    #[serde(default)]
    exported_at_ms: i64,
    #[serde(default)]
    app_version: String,
    #[serde(default)]
    counts: BundleCounts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checksum: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preferences: Option<BundlePreferences>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    credentials: Vec<WorkBuddyCredential>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    api_keys: Vec<ApiKeyEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kdf: Option<KdfParams>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cipher: Option<CipherBlob>,
}

// ── Public API types ───────────────────────────────────────────────────────

/// Export knobs. `passphrase: None` (or an empty string) means a plain bundle.
#[derive(Debug, Clone, Default)]
pub struct ExportOptions {
    pub passphrase: Option<String>,
}

/// What an export produced, for the GUI to report.
#[derive(Debug, Clone, Serialize)]
pub struct ExportOutcome {
    pub encrypted: bool,
    pub counts: BundleCounts,
    /// Anything worth telling the user about what was (not) included.
    ///
    /// The important one is "the pool is empty": an export that writes a valid
    /// but empty bundle must not read as success, because the user's next action
    /// is to trust that file on a new machine. The file is still written (it is
    /// a legitimate, importable bundle), but the caller has to be able to say
    /// what happened.
    pub warnings: Vec<String>,
}

impl ExportOutcome {
    /// Whether the bundle carries no identity at all.
    pub fn is_empty(&self) -> bool {
        self.counts.credentials == 0 && self.counts.api_keys == 0
    }
}

/// Read-only summary of what applying a bundle would do. Deliberately contains
/// no token, no key and no label beyond what the user already sees in the pool
/// list — it is rendered in the GUI and, on a bad day, pasted into an issue.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportPreview {
    pub mode: String,
    pub encrypted: bool,
    pub credentials_total: usize,
    pub credentials_added: usize,
    pub credentials_updated: usize,
    pub credentials_unchanged: usize,
    /// Replace mode only: local entries the bundle does not mention.
    pub credentials_removed: usize,
    pub keys_total: usize,
    pub keys_added: usize,
    pub keys_updated: usize,
    pub keys_unchanged: usize,
    pub keys_removed: usize,
    /// The default identity that will be in force after the import, or empty
    /// for "no explicit default (first usable wins)".
    pub default_identity_applied: String,
    /// True when a bundle default was dropped because it names nothing.
    pub default_identity_dropped: bool,
    pub checkin_enabled: Option<bool>,
    pub checkin_time: Option<String>,
    /// Non-fatal findings: checksum mismatch, id recomputed, unknown fields.
    pub warnings: Vec<String>,
}

/// The three stores as they will be written. Built by `plan`, committed by
/// `commit`; `preview_import` stops after building it.
struct PlannedState {
    credentials: Vec<WorkBuddyCredential>,
    api_keys: Vec<ApiKeyEntry>,
    prefs: PoolPreferences,
    /// Explicit legacy pointers (`default_credential_id` / `default_key_id`).
    /// Cleared on replace so a stale pointer cannot survive its target.
    clear_legacy_defaults: bool,
}

/// A decoded bundle, plus anything worth telling the user about the decoding.
struct Decoded {
    payload: BundlePayload,
    encrypted: bool,
    checksum: Option<String>,
    warnings: Vec<String>,
}

// ── Export ─────────────────────────────────────────────────────────────────

/// Normalize one credential for travel: drop the state that describes *this*
/// machine's recent trouble, keep everything that describes the identity.
///
/// A cooldown is a rate-limit memory from the exporting machine's network, and
/// `last_error` is the text of that failure — carrying either to a fresh machine
/// would make a healthy account look broken there for no reason.
fn normalize_credential(c: &WorkBuddyCredential) -> WorkBuddyCredential {
    let mut out = c.clone();
    out.cooldown_until_ms = None;
    out.last_error.clear();
    out
}

/// Build the payload from the local stores, normalized. Also returns warnings
/// describing anything wrong or dropped, so the GUI can say so.
fn collect_payload() -> (BundlePayload, Vec<String>) {
    let mut warnings = Vec::new();

    // Check the files before reading them through the `load_*` accessors.
    // Those degrade to "empty" on any parse failure (correct for the request
    // path, which must keep serving), so without this the export cannot tell a
    // broken file from an empty pool and would happily write a bundle that
    // restores nothing.
    for (name, state) in workbuddy_auth::store_health() {
        if let workbuddy_auth::StoreState::Corrupt(detail) = state {
            warnings.push(format!(
                "{name}文件已损坏，本次导出会缺少这部分内容（{detail}）。修复或删除该文件后重试：{}",
                match name {
                    "账号池" => workbuddy_auth::credentials_path(),
                    "密钥池" => workbuddy_auth::api_keys_path(),
                    _ => workbuddy_auth::preferences_path(),
                }
                .display()
            ));
        }
    }

    let credentials: Vec<WorkBuddyCredential> = workbuddy_auth::load_credentials()
        .iter()
        .map(normalize_credential)
        .collect();
    let api_keys = workbuddy_auth::load_api_keys();
    let prefs = workbuddy_auth::load_preferences();

    if credentials.is_empty() && api_keys.is_empty() && warnings.is_empty() {
        warnings.push("本机身份池为空，导出的文件不包含任何账号或密钥".to_string());
    }

    let payload = BundlePayload {
        preferences: BundlePreferences {
            default_identity_id: Some(prefs.default_identity_id.clone()),
            daily_checkin_enabled: Some(prefs.daily_checkin_enabled),
            daily_checkin_time: Some(prefs.daily_checkin_time.clone()),
        },
        credentials,
        api_keys,
    };
    (payload, warnings)
}

/// Checksum over the payload's canonical JSON.
///
/// This is a *tamper detector*, not a signature: it catches a hand-edited or
/// truncated file and tells the user their edits were seen. MD5 is therefore the
/// right tool, and reusing [`md5_hex`] avoids a second hash implementation.
fn payload_checksum(payload: &BundlePayload) -> Result<String> {
    let bytes = serde_json::to_vec(payload)?;
    let digest = md5_hex(&bytes);
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Encode a payload into a bundle document.
///
/// An empty pool still produces a valid bundle — refusing to write would be
/// surprising, and the file is legitimately importable (it can carry preferences
/// alone). But the emptiness is reported through [`ExportOutcome::warnings`] so
/// the caller can stop the user from carrying away a file that looks like a
/// backup and restores nothing.
pub fn export_text(opts: &ExportOptions) -> Result<(String, ExportOutcome)> {
    let (payload, warnings) = collect_payload();
    let counts = BundleCounts {
        credentials: payload.credentials.len(),
        api_keys: payload.api_keys.len(),
    };
    let checksum = payload_checksum(&payload)?;
    let passphrase = opts
        .passphrase
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());

    let mut bundle = BundleFile {
        format: BUNDLE_FORMAT.to_string(),
        version: BUNDLE_VERSION,
        kind: KIND_PLAIN.to_string(),
        exported_at_ms: crate::util::unix_millis(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        counts: counts.clone(),
        checksum: Some(checksum),
        preferences: Some(payload.preferences.clone()),
        credentials: payload.credentials.clone(),
        api_keys: payload.api_keys.clone(),
        kdf: None,
        cipher: None,
    };

    let encrypted = passphrase.is_some();
    if let Some(passphrase) = passphrase {
        let (kdf, cipher) = encrypt_payload(passphrase, &payload)?;
        // The token-bearing fields must not survive into the envelope.
        bundle.kind = KIND_ENCRYPTED.to_string();
        bundle.preferences = None;
        bundle.credentials = Vec::new();
        bundle.api_keys = Vec::new();
        bundle.kdf = Some(kdf);
        bundle.cipher = Some(cipher);
    }

    let text = serde_json::to_string_pretty(&bundle)?;
    Ok((
        text,
        ExportOutcome {
            encrypted,
            counts,
            warnings,
        },
    ))
}

/// Default destination: `~/Downloads/proxy-rs-pool-YYYYMMDD.json`, falling back
/// to the data directory when the machine has no Downloads folder.
///
/// The date is the *local* one (the same offset `util::local_datetime_millis`
/// applies), so a file exported at 23:30 in UTC+8 is named for that day rather
/// than for the previous UTC day.
pub fn default_export_path() -> PathBuf {
    let stamp = export_stamp(
        crate::util::unix_millis(),
        crate::util::local_utc_offset_secs(),
    );
    let name = format!("proxy-rs-pool-{stamp}.json");
    if let Some(home) = std::env::var_os("HOME") {
        let downloads = PathBuf::from(home).join("Downloads");
        if downloads.is_dir() {
            return downloads.join(name);
        }
    }
    crate::settings::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(name)
}

/// `YYYYMMDD` in a zone offset, split out so the timezone arithmetic is testable
/// without touching the process-global `PROXY_TZ_OFFSET_HOURS`.
///
/// The offset is added in signed arithmetic *before* clamping: a
/// western-hemisphere offset is negative, and clamping it first would silently
/// name the file for the wrong day there.
fn export_stamp(now_ms: i64, offset_secs: i64) -> String {
    let local_secs = now_ms / 1000 + offset_secs;
    let formatted = crate::util::format_epoch_secs(local_secs.max(0) as u64);
    formatted[..10].replace('-', "")
}

/// Strip anything path-like from a user-supplied filename.
///
/// The GUI lets the user name the file, and that name arrives as an IPC string;
/// without this, `../../.ssh/authorized_keys` would be a write primitive. Only
/// a base name survives: directory separators are dropped by taking the last
/// path component, and every character outside letters/digits (any script, so
/// a Chinese filename works), `-`, `_` and `.` becomes `-`. An empty result
/// falls back to the default name.
pub fn sanitize_filename(raw: &str) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .trim_start_matches('.');
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(['-', '.']).to_string();
    if cleaned.is_empty() {
        "proxy-rs-pool.json".to_string()
    } else if cleaned.to_ascii_lowercase().ends_with(".json") {
        cleaned
    } else {
        format!("{cleaned}.json")
    }
}

/// Compose the path to *suggest* for an export: a sanitized user filename inside
/// the given directory.
///
/// Since the export grew a native "save as" panel, nothing writes to this path
/// unseen — it only seeds the panel's starting directory and name field, and the
/// user can override both. It is still built by sanitizing the name rather than
/// joining it raw, because a suggested name is what the panel opens with and
/// `sanitize_filename` is what keeps a pasted `../../foo` from appearing as a
/// directory traversal in the name field. `write_bundle` is never called with a
/// path that skipped the panel.
pub fn export_path_in(dir: &Path, filename: Option<&str>) -> PathBuf {
    match filename.map(str::trim).filter(|f| !f.is_empty()) {
        Some(name) => dir.join(sanitize_filename(name)),
        None => {
            let default = default_export_path();
            let name = default
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "proxy-rs-pool.json".to_string());
            dir.join(sanitize_filename(&name))
        }
    }
}

// ── Encryption ─────────────────────────────────────────────────────────────

/// Derive the key and seal the payload.
///
/// Argon2id's memory cost is what makes a weak passphrase expensive to attack
/// offline; the salt is random per export, so two bundles of the same pool do
/// not share a key.
fn encrypt_payload(passphrase: &str, payload: &BundlePayload) -> Result<(KdfParams, CipherBlob)> {
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce);

    let key = derive_key(passphrase, &salt, KDF_M_COST, KDF_T_COST, KDF_P_COST)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow!("加密器初始化失败"))?;
    let plaintext = serde_json::to_vec(payload)?;
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext.as_slice())
        .map_err(|_| anyhow!("加密失败"))?;

    Ok((
        KdfParams {
            algo: KDF_ALGO.to_string(),
            m_cost: KDF_M_COST,
            t_cost: KDF_T_COST,
            p_cost: KDF_P_COST,
            salt_b64: B64.encode(salt),
        },
        CipherBlob {
            algo: CIPHER_ALGO.to_string(),
            nonce_b64: B64.encode(nonce),
            ct_b64: B64.encode(ct),
        },
    ))
}

/// Run Argon2id with the parameters stored in the bundle.
fn derive_key(
    passphrase: &str,
    salt: &[u8],
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<[u8; KEY_LEN]> {
    let params = Params::new(m_cost, t_cost, p_cost, Some(KEY_LEN))
        .map_err(|e| anyhow!("Argon2 参数无效: {e}"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; KEY_LEN];
    argon
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow!("口令派生失败: {e}"))?;
    Ok(key)
}

/// Open an encrypted envelope.
///
/// A failure here is reported as one message covering wrong passphrase and
/// corrupted file, because AES-GCM cannot tell them apart — claiming to know
/// which it was would be a lie, and telling the user "wrong passphrase" when the
/// file is truncated sends them hunting for a password that was never wrong.
fn decrypt_payload(passphrase: &str, kdf: &KdfParams, blob: &CipherBlob) -> Result<BundlePayload> {
    if kdf.algo != KDF_ALGO {
        bail!("不支持的口令派生算法: {}", kdf.algo);
    }
    if blob.algo != CIPHER_ALGO {
        bail!("不支持的加密算法: {}", blob.algo);
    }
    let salt = B64.decode(&kdf.salt_b64).context("口令派生参数损坏")?;
    let nonce = B64.decode(&blob.nonce_b64).context("加密参数损坏")?;
    if nonce.len() != NONCE_LEN {
        bail!("口令错误或文件已损坏");
    }
    let ct = B64.decode(&blob.ct_b64).context("密文损坏")?;

    let key = derive_key(passphrase, &salt, kdf.m_cost, kdf.t_cost, kdf.p_cost)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow!("加密器初始化失败"))?;
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&nonce), ct.as_slice())
        .map_err(|_| anyhow!("口令错误或文件已损坏"))?;
    serde_json::from_slice(&plaintext).context("解密后的内容不是有效的账号池数据")
}

// ── Decoding ───────────────────────────────────────────────────────────────

/// Parse (and if needed decrypt) a bundle, checking format-level invariants.
/// Does not touch the local stores.
fn decode(text: &str, passphrase: Option<&str>) -> Result<Decoded> {
    if text.len() > MAX_BUNDLE_BYTES {
        bail!(
            "文件过大（{} 字节，上限 {} 字节），这不是账号池导出文件",
            text.len(),
            MAX_BUNDLE_BYTES
        );
    }
    let bundle: BundleFile =
        serde_json::from_str(text).context("无法解析导出文件：不是有效的 JSON")?;

    if bundle.format != BUNDLE_FORMAT {
        bail!(
            "这不是 Proxy RS 账号池导出文件（format = \"{}\"）",
            bundle.format
        );
    }
    let mut warnings = Vec::new();
    if bundle.version > BUNDLE_VERSION {
        // A newer writer may have added fields this build ignores. Importing the
        // part it does understand is more useful than refusing outright, but the
        // user has to know the result may be incomplete.
        warnings.push(format!(
            "该文件由更新版本的 Proxy RS 导出 (格式版本 {})，本版本按格式版本 {} 读取，可能丢失部分内容",
            bundle.version, BUNDLE_VERSION
        ));
    }

    let (payload, encrypted) = match bundle.kind.as_str() {
        KIND_PLAIN => (
            BundlePayload {
                preferences: bundle.preferences.clone().unwrap_or_default(),
                credentials: bundle.credentials.clone(),
                api_keys: bundle.api_keys.clone(),
            },
            false,
        ),
        KIND_ENCRYPTED => {
            let Some(passphrase) = passphrase.map(str::trim).filter(|p| !p.is_empty()) else {
                bail!("该导出文件已加密，请输入口令");
            };
            let kdf = bundle.kdf.as_ref().context("加密文件缺少口令派生参数")?;
            let blob = bundle.cipher.as_ref().context("加密文件缺少密文")?;
            (decrypt_payload(passphrase, kdf, blob)?, true)
        }
        other => bail!("未知的导出文件类型: {other}"),
    };

    Ok(Decoded {
        payload,
        encrypted,
        checksum: bundle.checksum.clone(),
        warnings,
    })
}

/// Enforce the invariant the id derivation promises: an id *is* the content
/// hash of the secret it names.
///
/// A mismatch means the file was edited by hand (or by a tool that does not know
/// the rule). Recomputing rather than rejecting keeps a usable account usable,
/// while the warning makes the edit visible; silently trusting the stored id
/// would be worse, because two different tokens could then collide onto one
/// entry and one of them would vanish.
fn reconcile_ids(payload: &mut BundlePayload, warnings: &mut Vec<String>) {
    for c in payload.credentials.iter_mut() {
        let expected = credential_id(&c.access_token);
        if c.id != expected {
            warnings.push(format!(
                "账号 {} 的 id 与 token 不一致，已按 token 重新计算为 {}",
                if c.id.is_empty() { "(空)" } else { &c.id },
                expected
            ));
            c.id = expected;
        }
    }
    for k in payload.api_keys.iter_mut() {
        let expected = api_key_id(&k.key);
        if k.id != expected {
            warnings.push(format!(
                "密钥 {} 的 id 与内容不一致，已重新计算为 {}",
                if k.id.is_empty() { "(空)" } else { &k.id },
                expected
            ));
            k.id = expected;
        }
    }
    // A duplicate id inside one bundle collapses to the last occurrence, which
    // is exactly what a set of upserts would do anyway.
    let mut seen = std::collections::HashSet::new();
    if payload
        .credentials
        .iter()
        .any(|c| !seen.insert(c.id.clone()))
    {
        let mut deduped: Vec<WorkBuddyCredential> = Vec::new();
        for c in payload.credentials.drain(..) {
            match deduped.iter_mut().find(|e| e.id == c.id) {
                Some(slot) => *slot = c,
                None => deduped.push(c),
            }
        }
        warnings.push("导出文件内存在重复账号，已按 id 去重".to_string());
        payload.credentials = deduped;
    }
    let mut seen = std::collections::HashSet::new();
    if payload.api_keys.iter().any(|k| !seen.insert(k.id.clone())) {
        let mut deduped: Vec<ApiKeyEntry> = Vec::new();
        for k in payload.api_keys.drain(..) {
            match deduped.iter_mut().find(|e| e.id == k.id) {
                Some(slot) => *slot = k,
                None => deduped.push(k),
            }
        }
        warnings.push("导出文件内存在重复密钥，已按 id 去重".to_string());
        payload.api_keys = deduped;
    }
}

/// Drop entries that cannot authenticate: a credential with an empty token or a
/// key with an empty key string would be written to the pool and then never
/// usable, and would keep showing up in the list as a mysterious failure.
fn drop_empty(payload: &mut BundlePayload, warnings: &mut Vec<String>) {
    let before = payload.credentials.len();
    payload
        .credentials
        .retain(|c| !c.access_token.trim().is_empty());
    if before != payload.credentials.len() {
        warnings.push(format!(
            "已跳过 {} 个缺少 access token 的账号",
            before - payload.credentials.len()
        ));
    }
    let before = payload.api_keys.len();
    payload.api_keys.retain(|k| !k.key.trim().is_empty());
    if before != payload.api_keys.len() {
        warnings.push(format!(
            "已跳过 {} 个内容为空的密钥",
            before - payload.api_keys.len()
        ));
    }
}

// ── Planning ───────────────────────────────────────────────────────────────

/// Fold the bundle into the local stores, purely (no writes).
fn plan(decoded: &Decoded, mode: ImportMode) -> Result<(ImportPreview, PlannedState)> {
    let mut payload = decoded.payload.clone();
    let mut warnings = decoded.warnings.clone();

    // Verify the checksum against the payload as it arrived, before any
    // normalization rewrites a field. A mismatch means the file was edited by
    // hand after export; that is not a reason to refuse a usable account, but it
    // is a reason to say so, because the edit is exactly what the user needs to
    // know about when the import result surprises them.
    if let Some(expected) = decoded.checksum.as_deref() {
        match payload_checksum(&payload) {
            Ok(actual) if actual != expected => warnings.push(
                "文件校验和与内容不一致：导出文件可能被手工修改过，请确认内容无误".to_string(),
            ),
            Err(e) => warnings.push(format!("无法校验文件完整性: {e}")),
            _ => {}
        }
    }

    reconcile_ids(&mut payload, &mut warnings);
    drop_empty(&mut payload, &mut warnings);

    let local_creds = workbuddy_auth::load_credentials();
    let local_keys = workbuddy_auth::load_api_keys();
    let local_prefs = workbuddy_auth::load_preferences();

    let mut credentials: Vec<WorkBuddyCredential> = match mode {
        ImportMode::Merge => local_creds.clone(),
        ImportMode::Replace => Vec::new(),
    };
    let mut api_keys: Vec<ApiKeyEntry> = match mode {
        ImportMode::Merge => local_keys.clone(),
        ImportMode::Replace => Vec::new(),
    };

    let mut credentials_added = 0;
    let mut credentials_updated = 0;
    let mut credentials_unchanged = 0;
    for incoming in &payload.credentials {
        // Existing entries keep their refresh material when the bundle does not
        // carry any: this machine may hold a fresher refresh token than the
        // bundle, and dropping it would force a re-login for no reason.
        let merged = match credentials.iter().find(|c| c.id == incoming.id) {
            Some(local) => preserve_local_secrets(incoming, local),
            None => incoming.clone(),
        };
        match credentials.iter_mut().find(|c| c.id == merged.id) {
            Some(slot) => {
                if records_equal(slot, &merged) {
                    credentials_unchanged += 1;
                } else {
                    credentials_updated += 1;
                }
                *slot = merged;
            }
            None => {
                credentials.push(merged);
                credentials_added += 1;
            }
        }
    }

    let mut keys_added = 0;
    let mut keys_updated = 0;
    let mut keys_unchanged = 0;
    for incoming in &payload.api_keys {
        match api_keys.iter_mut().find(|k| k.id == incoming.id) {
            Some(slot) => {
                if records_equal(slot, incoming) {
                    keys_unchanged += 1;
                } else {
                    keys_updated += 1;
                }
                *slot = incoming.clone();
            }
            None => {
                api_keys.push(incoming.clone());
                keys_added += 1;
            }
        }
    }

    let credentials_removed = match mode {
        ImportMode::Replace => local_creds.len(),
        ImportMode::Merge => 0,
    };
    let keys_removed = match mode {
        ImportMode::Replace => local_keys.len(),
        ImportMode::Merge => 0,
    };

    // ── Preferences ──
    let mut prefs = local_prefs.clone();
    prefs.daily_checkin_enabled = payload
        .preferences
        .daily_checkin_enabled
        .unwrap_or(local_prefs.daily_checkin_enabled);
    prefs.daily_checkin_time = payload
        .preferences
        .daily_checkin_time
        .as_ref()
        .map(|t| workbuddy_auth::normalize_checkin_time(t))
        .unwrap_or_else(|| local_prefs.daily_checkin_time.clone());

    let known = |id: &str, creds: &[WorkBuddyCredential], keys: &[ApiKeyEntry]| {
        id.is_empty()
            || id == workbuddy_auth::NO_ACCOUNT_POOL_SENTINEL
            || creds.iter().any(|c| c.id == id)
            || keys.iter().any(|k| k.id == id)
    };

    let mut default_identity_dropped = false;
    // `None` means the bundle had no opinion, so the local default stands —
    // unless replace mode just deleted its target, in which case a dangling
    // pointer would silently fall through to the first usable entry anyway.
    // Clearing it keeps the GUI honest about what is in force.
    let candidate = payload
        .preferences
        .default_identity_id
        .as_deref()
        .map(str::to_string);
    match candidate {
        Some(id) if known(&id, &credentials, &api_keys) => {
            prefs.default_identity_id = id;
        }
        Some(id) => {
            warnings.push(format!(
                "导出文件中的默认身份 {id} 不在导入内容中，已清空默认身份"
            ));
            prefs.default_identity_id.clear();
            default_identity_dropped = true;
        }
        None => {
            if !prefs.default_identity_id.is_empty()
                && !known(&prefs.default_identity_id, &credentials, &api_keys)
            {
                warnings.push(format!(
                    "本机默认身份 {} 不在导入后的身份池中，已清空默认身份",
                    prefs.default_identity_id
                ));
                prefs.default_identity_id.clear();
                default_identity_dropped = true;
            }
        }
    }
    if mode == ImportMode::Replace
        && (!prefs.default_credential_id.is_empty() || !prefs.default_key_id.is_empty())
    {
        warnings.push("替换导入已清除旧版默认账号/密钥指针".to_string());
    }

    let preview = ImportPreview {
        mode: mode.as_str().to_string(),
        encrypted: decoded.encrypted,
        credentials_total: payload.credentials.len(),
        credentials_added,
        credentials_updated,
        credentials_unchanged,
        credentials_removed,
        keys_total: payload.api_keys.len(),
        keys_added,
        keys_updated,
        keys_unchanged,
        keys_removed,
        default_identity_applied: prefs.default_identity_id.clone(),
        default_identity_dropped,
        checkin_enabled: payload.preferences.daily_checkin_enabled,
        checkin_time: payload.preferences.daily_checkin_time.clone(),
        warnings,
    };

    let state = PlannedState {
        credentials,
        api_keys,
        prefs,
        clear_legacy_defaults: mode == ImportMode::Replace,
    };
    Ok((preview, state))
}

/// Keep this machine's fresher login material when the bundle has none.
fn preserve_local_secrets(
    incoming: &WorkBuddyCredential,
    local: &WorkBuddyCredential,
) -> WorkBuddyCredential {
    let mut merged = incoming.clone();
    if merged.refresh_token.is_none() {
        merged.refresh_token = local.refresh_token.clone();
    }
    if merged.expires_at_ms.is_none() {
        merged.expires_at_ms = local.expires_at_ms;
    }
    if merged.machine_id.trim().is_empty() {
        merged.machine_id = local.machine_id.clone();
    }
    merged
}

/// Structural equality, used only to distinguish "updated" from "unchanged" in
/// the preview. Serializing is both simpler and stricter than a field-by-field
/// comparison that a future field would silently escape.
fn records_equal<T: Serialize>(a: &T, b: &T) -> bool {
    match (serde_json::to_value(a), serde_json::to_value(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

// ── Public entry points ────────────────────────────────────────────────────

/// Read-only: decode the bundle and report what importing it would change.
pub fn preview_import(
    text: &str,
    passphrase: Option<&str>,
    mode: ImportMode,
) -> Result<ImportPreview> {
    let decoded = decode(text, passphrase)?;
    let (preview, _) = plan(&decoded, mode)?;
    Ok(preview)
}

/// Decode, plan, then write. Re-decodes instead of accepting a previous
/// preview's result: the file could have been re-exported (or edited) between
/// the two calls, and the write must reflect what is on screen *now*.
pub fn apply_import(
    text: &str,
    passphrase: Option<&str>,
    mode: ImportMode,
) -> Result<ImportPreview> {
    let decoded = decode(text, passphrase)?;
    let (preview, state) = plan(&decoded, mode)?;
    commit(&state)?;
    Ok(preview)
}

/// Write the three stores, backing each one up first.
fn commit(state: &PlannedState) -> Result<()> {
    let mut prefs = state.prefs.clone();
    if state.clear_legacy_defaults {
        prefs.default_credential_id.clear();
        prefs.default_key_id.clear();
    }

    // Credentials first: if a later write fails, the pool is at least consistent
    // about which accounts exist, and the failure is reported rather than
    // swallowed.
    write_json_atomic(
        &workbuddy_auth::credentials_path(),
        &state.credentials,
        true,
    )?;
    write_json_atomic(&workbuddy_auth::api_keys_path(), &state.api_keys, true)?;
    write_json_atomic(&workbuddy_auth::preferences_path(), &prefs, false)?;
    Ok(())
}

/// Back up `path` (if present) and replace it atomically.
///
/// The temp-file + rename dance is what makes an interrupted import recoverable:
/// a reader either sees the old file or the new one, never a half-written JSON
/// document. `secret` applies 0600, which the two token-bearing stores need and
/// the preferences file does not.
fn write_json_atomic<T: Serialize>(path: &Path, value: &T, secret: bool) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("无法创建目录 {}", dir.display()))?;
    }
    if path.exists() {
        let backup = backup_path(path);
        std::fs::copy(path, &backup)
            .with_context(|| format!("无法备份 {} 到 {}", path.display(), backup.display()))?;
    }
    let text = serde_json::to_string_pretty(value)?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("无法写入 {}", tmp.display()))?;
    if secret {
        set_private_permissions(&tmp);
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow!("无法替换 {}: {e}", path.display())
    })?;
    if secret {
        set_private_permissions(path);
    }
    Ok(())
}

/// `<file>.proxy-rs-backup-<epoch>` beside the original, matching the naming the
/// DSH writer already uses so backups are recognisable at a glance.
fn backup_path(path: &Path) -> PathBuf {
    let secs = (crate::util::unix_millis() / 1000).max(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "pool".to_string());
    path.with_file_name(format!("{name}.proxy-rs-backup-{secs}"))
}

/// 0600 on Unix; a no-op elsewhere (Windows inherits the user profile's ACLs).
fn set_private_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Write an export bundle to `path`, atomically and 0600.
pub fn write_bundle(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("无法写入 {}", tmp.display()))?;
    set_private_permissions(&tmp);
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow!("无法保存到 {}: {e}", path.display())
    })?;
    set_private_permissions(path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    /// Every test here relocates `PROXY_DATA_DIR`, which is process-global. The
    /// lock is shared with the `settings` tests for exactly that reason: three
    /// test modules writing the same env var to different roots would otherwise
    /// interleave and fail each other.
    struct TempDataDir {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: (Option<OsString>, Option<OsString>),
        dir: PathBuf,
    }

    impl TempDataDir {
        fn new(name: &str) -> Self {
            let guard = crate::settings::data_dir_test_lock();
            let previous = (
                std::env::var_os(crate::settings::DATA_DIR_ENV),
                std::env::var_os(crate::settings::LEGACY_DATA_DIR_ENV),
            );
            let dir = std::env::temp_dir().join(format!(
                "proxy-rs-pool-transfer-{}-{}",
                std::process::id(),
                name
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::env::remove_var(crate::settings::LEGACY_DATA_DIR_ENV);
            std::env::set_var(crate::settings::DATA_DIR_ENV, &dir);
            Self {
                _guard: guard,
                previous,
                dir,
            }
        }
    }

    impl Drop for TempDataDir {
        fn drop(&mut self) {
            match &self.previous.0 {
                Some(v) => std::env::set_var(crate::settings::DATA_DIR_ENV, v),
                None => std::env::remove_var(crate::settings::DATA_DIR_ENV),
            }
            match &self.previous.1 {
                Some(v) => std::env::set_var(crate::settings::LEGACY_DATA_DIR_ENV, v),
                None => std::env::remove_var(crate::settings::LEGACY_DATA_DIR_ENV),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn cred(id_token: &str, label: &str) -> WorkBuddyCredential {
        WorkBuddyCredential {
            id: credential_id(id_token),
            label: label.to_string(),
            access_token: id_token.to_string(),
            refresh_token: Some(format!("{id_token}-refresh")),
            expires_at_ms: Some(4_000_000_000_000),
            domain: "copilot.tencent.com".to_string(),
            machine_id: "machine-abc".to_string(),
            enabled: true,
            ..Default::default()
        }
    }

    fn key(material: &str, label: &str) -> ApiKeyEntry {
        ApiKeyEntry {
            id: api_key_id(material),
            label: label.to_string(),
            key: material.to_string(),
            enabled: true,
            ..Default::default()
        }
    }

    /// Seed the (relocated) stores the way the GUI would.
    fn seed(creds: &[WorkBuddyCredential], keys: &[ApiKeyEntry]) {
        workbuddy_auth::save_credentials(creds).unwrap();
        workbuddy_auth::save_api_keys(keys).unwrap();
    }

    #[test]
    fn plain_round_trip_restores_every_pool_member() {
        let _dir = TempDataDir::new("roundtrip");
        let creds = vec![cred("token-a", "工作号"), cred("token-b", "备用号")];
        let keys = vec![key("sk-aaa", "主密钥")];
        seed(&creds, &keys);
        workbuddy_auth::save_preferences(&PoolPreferences {
            default_identity_id: creds[0].id.clone(),
            daily_checkin_enabled: true,
            daily_checkin_time: "08:30".to_string(),
            ..Default::default()
        })
        .unwrap();

        let (text, outcome) = export_text(&ExportOptions::default()).unwrap();
        assert!(!outcome.encrypted);
        assert_eq!(outcome.counts.credentials, 2);
        assert_eq!(outcome.counts.api_keys, 1);

        // Wipe the machine, as if this were the new one.
        seed(&[], &[]);
        workbuddy_auth::save_preferences(&PoolPreferences::default()).unwrap();

        let preview = apply_import(&text, None, ImportMode::Merge).unwrap();
        assert_eq!(preview.credentials_added, 2);
        assert_eq!(preview.keys_added, 1);
        assert_eq!(
            preview.default_identity_applied,
            cred("token-a", "").id,
            "the exported default must come back"
        );

        let restored = workbuddy_auth::load_credentials();
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].label, "工作号");
        assert_eq!(restored[0].machine_id, "machine-abc");
        assert_eq!(workbuddy_auth::load_api_keys().len(), 1);
        assert_eq!(
            workbuddy_auth::effective_default_identity(),
            cred("token-a", "").id,
            "the account the bundle pinned is the identity in force"
        );
        let prefs = workbuddy_auth::load_preferences();
        assert!(prefs.daily_checkin_enabled);
        assert_eq!(prefs.daily_checkin_time, "08:30");
    }

    #[test]
    fn export_normalizes_machine_local_state() {
        let _dir = TempDataDir::new("normalize");
        let mut c = cred("token-a", "工作号");
        c.cooldown_until_ms = Some(crate::util::unix_millis() + 600_000);
        c.last_error = "429 too many requests".to_string();
        c.last_checkin_date = Some("2026-01-01".to_string());
        seed(&[c], &[]);

        let (text, _) = export_text(&ExportOptions::default()).unwrap();
        let bundle: BundleFile = serde_json::from_str(&text).unwrap();
        let exported = &bundle.credentials[0];
        assert_eq!(
            exported.cooldown_until_ms, None,
            "cooldown is machine-local"
        );
        assert!(
            exported.last_error.is_empty(),
            "stale error text must not travel"
        );
        assert_eq!(
            exported.last_checkin_date.as_deref(),
            Some("2026-01-01"),
            "the check-in day is the account's, not the machine's"
        );
        assert_eq!(exported.machine_id, "machine-abc");
    }

    #[test]
    fn encrypted_round_trip_matches_the_plain_one() {
        let _dir = TempDataDir::new("encrypted");
        let creds = vec![cred("token-a", "工作号")];
        seed(&creds, &[key("sk-aaa", "主密钥")]);
        let pass = "correct horse battery staple";

        let (plain, _) = export_text(&ExportOptions::default()).unwrap();
        let (sealed, outcome) = export_text(&ExportOptions {
            passphrase: Some(pass.to_string()),
        })
        .unwrap();
        assert!(outcome.encrypted);

        apply_import(&plain, None, ImportMode::Replace).unwrap();
        let from_plain = (
            workbuddy_auth::load_credentials(),
            workbuddy_auth::load_api_keys(),
        );

        seed(&[], &[]);
        apply_import(&sealed, Some(pass), ImportMode::Replace).unwrap();
        let from_sealed = (
            workbuddy_auth::load_credentials(),
            workbuddy_auth::load_api_keys(),
        );

        assert_eq!(from_plain.0, from_sealed.0);
        assert_eq!(from_plain.1, from_sealed.1);
    }

    #[test]
    fn an_empty_pool_exports_a_bundle_but_says_so() {
        let _dir = TempDataDir::new("empty-pool");
        seed(&[], &[]);

        let (text, outcome) = export_text(&ExportOptions::default()).unwrap();

        // The file is still written: it is a valid bundle, and it may legitimately
        // carry preferences alone. What must not happen is reporting success.
        assert_eq!(outcome.counts.credentials, 0);
        assert_eq!(outcome.counts.api_keys, 0);
        assert!(outcome.is_empty());
        assert!(
            outcome.warnings.iter().any(|w| w.contains("身份池为空")),
            "an empty export must explain itself: {:?}",
            outcome.warnings
        );
        // And it must still round-trip, so the user who exports an empty pool on
        // purpose (to move only the schedule) is not broken by the warning.
        let preview = preview_import(&text, None, ImportMode::Merge).unwrap();
        assert_eq!(preview.credentials_added, 0);
    }

    /// The failure that produced a 356-byte "backup" containing nothing: a
    /// corrupt store reads as empty through `load_*`, so the export cannot tell
    /// it apart from a genuinely empty pool unless it checks the files first.
    #[test]
    fn a_corrupt_store_is_reported_instead_of_exporting_silence() {
        let _dir = TempDataDir::new("corrupt-store");
        seed(&[cred("token-a", "工作号")], &[key("sk-aaa", "主密钥")]);

        // Truncate the credential file the way an interrupted write would.
        let path = workbuddy_auth::credentials_path();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.truncate(text.len() / 2);
        std::fs::write(&path, &text).unwrap();

        let (_, outcome) = export_text(&ExportOptions::default()).unwrap();

        assert_eq!(
            outcome.counts.credentials, 0,
            "a corrupt store does read as empty — that is the underlying design"
        );
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.contains("账号池文件已损坏")),
            "the corruption must be named, not silently exported as an empty pool: {:?}",
            outcome.warnings
        );
        // The healthy key pool is still exported: one broken file must not wipe
        // the rest of the bundle.
        assert_eq!(outcome.counts.api_keys, 1);
    }

    #[test]
    fn store_health_separates_missing_from_corrupt() {
        let _dir = TempDataDir::new("store-health");

        // Nothing on disk yet: every store is legitimately absent.
        for (_, state) in workbuddy_auth::store_health() {
            assert_eq!(state, workbuddy_auth::StoreState::Missing);
            assert!(!state.is_corrupt());
        }

        seed(&[cred("token-a", "工作号")], &[key("sk-aaa", "主密钥")]);
        workbuddy_auth::save_preferences(&PoolPreferences {
            daily_checkin_time: "09:00".to_string(),
            ..Default::default()
        })
        .unwrap();
        for (_, state) in workbuddy_auth::store_health() {
            assert_eq!(state, workbuddy_auth::StoreState::Ok);
        }

        std::fs::write(workbuddy_auth::api_keys_path(), "{ not json").unwrap();
        let health = workbuddy_auth::store_health();
        let keys = health.iter().find(|(n, _)| *n == "密钥池").unwrap();
        assert!(keys.1.is_corrupt(), "malformed JSON must be Corrupt");
    }

    #[test]
    fn encrypted_envelope_leaks_no_token_material() {
        let _dir = TempDataDir::new("no-leak");
        seed(
            &[cred("super-secret-access-token", "工作号")],
            &[key("sk-secret-material", "k")],
        );
        let (sealed, _) = export_text(&ExportOptions {
            passphrase: Some("pw".to_string()),
        })
        .unwrap();

        // Neither the full secret nor a distinguishing slice of it may appear in
        // the envelope — base64 of the ciphertext is the only thing that does.
        for needle in [
            "super-secret-access-token",
            "secret-access-token",
            "sk-secret-material",
            "copilot.tencent.com",
        ] {
            assert!(
                !sealed.contains(needle),
                "encrypted bundle must not contain {needle:?}"
            );
        }
        assert!(sealed.contains("argon2id"));
        assert!(sealed.contains("aes-256-gcm"));
    }

    #[test]
    fn wrong_passphrase_fails_and_writes_nothing() {
        let _dir = TempDataDir::new("wrong-pass");
        seed(&[cred("token-a", "工作号")], &[]);
        let (sealed, _) = export_text(&ExportOptions {
            passphrase: Some("right".to_string()),
        })
        .unwrap();
        seed(&[], &[]);

        let err = apply_import(&sealed, Some("wrong"), ImportMode::Merge).unwrap_err();
        assert!(
            err.to_string().contains("口令错误或文件已损坏"),
            "unexpected error: {err}"
        );
        assert!(
            workbuddy_auth::load_credentials().is_empty(),
            "a failed import must leave the pool untouched"
        );
    }

    #[test]
    fn encrypted_bundle_without_a_passphrase_says_so() {
        let _dir = TempDataDir::new("needs-pass");
        seed(&[cred("token-a", "工作号")], &[]);
        let (sealed, _) = export_text(&ExportOptions {
            passphrase: Some("pw".to_string()),
        })
        .unwrap();
        let err = preview_import(&sealed, None, ImportMode::Merge).unwrap_err();
        assert!(err.to_string().contains("请输入口令"), "unexpected: {err}");
    }

    #[test]
    fn merge_updates_only_what_changed_and_keeps_local_refresh_tokens() {
        let _dir = TempDataDir::new("merge");
        let mut local = cred("token-a", "旧的备注");
        local.refresh_token = Some("local-fresher-refresh".to_string());
        seed(&[local.clone(), cred("token-only-local", "只在本机")], &[]);

        // A bundle that renames the account but carries no refresh token.
        let mut traveling = cred("token-a", "新的备注");
        traveling.refresh_token = None;
        traveling.expires_at_ms = None;
        let bundle = BundlePayload {
            preferences: BundlePreferences::default(),
            credentials: vec![traveling],
            api_keys: Vec::new(),
        };
        let text = plain_bundle(&bundle);

        let preview = apply_import(&text, None, ImportMode::Merge).unwrap();
        assert_eq!(preview.credentials_updated, 1);
        assert_eq!(preview.credentials_added, 0);
        assert_eq!(preview.credentials_removed, 0, "merge never removes");

        let after = workbuddy_auth::load_credentials();
        assert_eq!(after.len(), 2, "the local-only account survives a merge");
        let touched = after.iter().find(|c| c.id == local.id).unwrap();
        assert_eq!(touched.label, "新的备注");
        assert_eq!(
            touched.refresh_token.as_deref(),
            Some("local-fresher-refresh"),
            "a bundle without refresh material must not erase the local one"
        );
    }

    #[test]
    fn replacing_drops_local_entries_and_backs_the_files_up() {
        let _dir = TempDataDir::new("replace");
        seed(&[cred("token-old", "旧账号")], &[key("sk-old", "旧密钥")]);
        let credentials_path = workbuddy_auth::credentials_path();
        let keys_path = workbuddy_auth::api_keys_path();
        assert!(credentials_path.exists());

        let bundle = BundlePayload {
            preferences: BundlePreferences::default(),
            credentials: vec![cred("token-new", "新账号")],
            api_keys: vec![key("sk-new", "新密钥")],
        };
        let preview = apply_import(&plain_bundle(&bundle), None, ImportMode::Replace).unwrap();

        assert_eq!(preview.credentials_removed, 1);
        assert_eq!(preview.keys_removed, 1);
        assert_eq!(workbuddy_auth::load_credentials().len(), 1);
        assert_eq!(workbuddy_auth::load_credentials()[0].label, "新账号");
        assert_eq!(workbuddy_auth::load_api_keys()[0].label, "新密钥");

        // Every replaced file must be recoverable by hand.
        assert!(
            backup_of(&credentials_path).is_some(),
            "credentials file was not backed up"
        );
        assert!(
            backup_of(&keys_path).is_some(),
            "key file was not backed up"
        );
    }

    #[test]
    fn dangling_default_identity_is_cleared_with_a_warning() {
        let _dir = TempDataDir::new("dangling");
        seed(&[cred("token-a", "工作号")], &[]);
        workbuddy_auth::save_preferences(&PoolPreferences {
            default_identity_id: credential_id("token-a"),
            ..Default::default()
        })
        .unwrap();

        let bundle = BundlePayload {
            preferences: BundlePreferences {
                default_identity_id: Some("wb-ffffff".to_string()),
                ..Default::default()
            },
            credentials: vec![cred("token-a", "工作号")],
            api_keys: Vec::new(),
        };
        let preview = apply_import(&plain_bundle(&bundle), None, ImportMode::Merge).unwrap();

        assert!(preview.default_identity_dropped);
        assert!(preview.default_identity_applied.is_empty());
        assert!(
            preview.warnings.iter().any(|w| w.contains("wb-ffffff")),
            "the warning must name the id that was dropped: {:?}",
            preview.warnings
        );
        assert!(workbuddy_auth::load_preferences()
            .default_identity_id
            .is_empty());
    }

    #[test]
    fn an_id_that_disagrees_with_its_token_is_recomputed() {
        let _dir = TempDataDir::new("recompute-id");
        let mut tampered = cred("token-a", "工作号");
        tampered.id = "wb-000000".to_string();
        let bundle = BundlePayload {
            preferences: BundlePreferences::default(),
            credentials: vec![tampered],
            api_keys: Vec::new(),
        };

        let preview = preview_import(&plain_bundle(&bundle), None, ImportMode::Merge).unwrap();
        assert!(preview.warnings.iter().any(|w| w.contains("重新计算")));
        apply_import(&plain_bundle(&bundle), None, ImportMode::Merge).unwrap();
        assert_eq!(
            workbuddy_auth::load_credentials()[0].id,
            credential_id("token-a")
        );
    }

    #[test]
    fn a_checksum_mismatch_warns_but_still_imports() {
        let _dir = TempDataDir::new("checksum");
        let bundle = BundlePayload {
            preferences: BundlePreferences::default(),
            credentials: vec![cred("token-a", "工作号")],
            api_keys: Vec::new(),
        };
        let mut file: BundleFile = serde_json::from_str(&plain_bundle(&bundle)).unwrap();
        file.checksum = Some("deadbeef".to_string());
        let text = serde_json::to_string(&file).unwrap();

        let preview = apply_import(&text, None, ImportMode::Merge).unwrap();
        assert_eq!(workbuddy_auth::load_credentials().len(), 1);
        assert!(preview.credentials_added == 1);
        assert!(
            preview.warnings.iter().any(|w| w.contains("校验和")),
            "a hand-edited file must be reported, not imported silently: {:?}",
            preview.warnings
        );
    }

    #[test]
    fn malformed_input_is_rejected_with_a_readable_message() {
        let _dir = TempDataDir::new("malformed");
        for (text, needle) in [
            ("not json at all", "不是有效的 JSON"),
            (
                r#"{"format":"something-else","version":1,"kind":"plain"}"#,
                "不是 Proxy RS",
            ),
            (
                r#"{"format":"proxy-rs-pool","version":1,"kind":"martian"}"#,
                "未知的导出文件类型",
            ),
        ] {
            let err = preview_import(text, None, ImportMode::Merge).unwrap_err();
            assert!(
                err.to_string().contains(needle),
                "expected {needle:?} in {err:?}"
            );
        }
    }

    #[test]
    fn a_newer_format_version_warns_instead_of_failing() {
        let _dir = TempDataDir::new("newer-version");
        let bundle = BundlePayload {
            preferences: BundlePreferences::default(),
            credentials: vec![cred("token-a", "工作号")],
            api_keys: Vec::new(),
        };
        let mut file: BundleFile = serde_json::from_str(&plain_bundle(&bundle)).unwrap();
        file.version = BUNDLE_VERSION + 7;
        let preview = apply_import(
            &serde_json::to_string(&file).unwrap(),
            None,
            ImportMode::Merge,
        )
        .unwrap();
        assert!(preview.warnings.iter().any(|w| w.contains("更新版本")));
        assert_eq!(workbuddy_auth::load_credentials().len(), 1);
    }

    #[test]
    fn oversized_input_is_refused_before_parsing() {
        let _dir = TempDataDir::new("oversized");
        let huge = "x".repeat(MAX_BUNDLE_BYTES + 1);
        let err = preview_import(&huge, None, ImportMode::Merge).unwrap_err();
        assert!(err.to_string().contains("文件过大"), "unexpected: {err}");
    }

    #[test]
    fn sanitize_filename_strips_paths_and_forces_a_json_suffix() {
        assert_eq!(sanitize_filename("备份"), "备份.json");
        assert_eq!(sanitize_filename("pool 2026"), "pool-2026.json");
        assert_eq!(
            sanitize_filename("../../.ssh/authorized_keys"),
            "authorized_keys.json"
        );
        assert_eq!(sanitize_filename("a/b/pool.json"), "pool.json");
        assert_eq!(sanitize_filename(""), "proxy-rs-pool.json");
        assert_eq!(sanitize_filename("..."), "proxy-rs-pool.json");
    }

    #[test]
    fn secret_files_are_written_owner_only() {
        let dir = TempDataDir::new("perms");
        seed(&[cred("token-a", "工作号")], &[key("sk-aaa", "主密钥")]);

        let (text, _) = export_text(&ExportOptions::default()).unwrap();
        let bundle = dir.dir.join("pool.json");
        write_bundle(&bundle, &text).unwrap();

        // Re-import so the credential/key stores are written by this module too.
        seed(&[], &[]);
        apply_import(&text, None, ImportMode::Merge).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode_of = |p: &Path| {
                std::fs::metadata(p)
                    .unwrap_or_else(|e| panic!("stat {}: {e}", p.display()))
                    .permissions()
                    .mode()
                    & 0o777
            };
            // The bundle and both token-bearing stores are 0600. The preferences
            // file holds no secret, so it is left at the process default.
            for path in [
                bundle.clone(),
                workbuddy_auth::credentials_path(),
                workbuddy_auth::api_keys_path(),
            ] {
                assert_eq!(mode_of(&path), 0o600, "{} is not 0600", path.display());
            }
        }
        #[cfg(not(unix))]
        let _ = &bundle;
    }

    #[test]
    fn the_export_stamp_follows_the_local_day_in_both_hemispheres() {
        // 2026-01-02T23:30:00Z. In UTC+8 that is already the 3rd; in UTC-5 it is
        // still the 2nd. A naive implementation that clamped the negative offset
        // to zero would stamp the western case as the 3rd.
        let now_ms = 1_767_396_600_000;
        assert_eq!(export_stamp(now_ms, 8 * 3600), "20260103");
        assert_eq!(export_stamp(now_ms, -5 * 3600), "20260102");
        assert_eq!(export_stamp(now_ms, 0), "20260102");
    }

    /// Build a plain bundle document for a payload, the way `export_text` does,
    /// so tests can seed arbitrary payloads (including tampered ones).
    fn plain_bundle(payload: &BundlePayload) -> String {
        let file = BundleFile {
            format: BUNDLE_FORMAT.to_string(),
            version: BUNDLE_VERSION,
            kind: KIND_PLAIN.to_string(),
            exported_at_ms: crate::util::unix_millis(),
            app_version: "test".to_string(),
            counts: BundleCounts {
                credentials: payload.credentials.len(),
                api_keys: payload.api_keys.len(),
            },
            checksum: Some(payload_checksum(payload).unwrap()),
            preferences: Some(payload.preferences.clone()),
            credentials: payload.credentials.clone(),
            api_keys: payload.api_keys.clone(),
            kdf: None,
            cipher: None,
        };
        serde_json::to_string_pretty(&file).unwrap()
    }

    /// The most recent `*.proxy-rs-backup-*` beside `path`, if one exists.
    fn backup_of(path: &Path) -> Option<PathBuf> {
        let dir = path.parent()?;
        let prefix = format!("{}.proxy-rs-backup-", path.file_name()?.to_string_lossy());
        std::fs::read_dir(dir)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with(&prefix))
                    .unwrap_or(false)
            })
    }
}
