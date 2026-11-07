//! The on-disk document: format constants and the envelope/payload wire shapes.

use super::*;

/// Magic value in every bundle. Checked on import so a random JSON file fails
/// with "这不是账号池导出文件" rather than a confusing serde error.
pub const BUNDLE_FORMAT: &str = "proxy-rs-pool";

/// Transfer-format version, deliberately independent of the app version: the
/// bundle layout changes far less often than the app, and a version that moved
/// with the release number would make every upgrade look incompatible.
pub const BUNDLE_VERSION: u32 = 2;

/// Import ceiling. The pools are kilobytes at any realistic size, so anything
/// larger is a mistake or a hostile input, and refusing early keeps a stray
/// multi-megabyte paste from being buffered and parsed.
pub const MAX_BUNDLE_BYTES: usize = 1024 * 1024;

pub(crate) const KIND_PLAIN: &str = "plain";

pub(crate) const KIND_ENCRYPTED: &str = "encrypted";

pub(crate) const KDF_ALGO: &str = "argon2id";

pub(crate) const CIPHER_ALGO: &str = "aes-256-gcm";

/// Argon2id cost: 19 MiB, 2 passes, 1 lane (the OWASP-recommended minimum).
/// Written into the bundle, so raising it later does not invalidate old files.
pub(crate) const KDF_M_COST: u32 = 19_456;

pub(crate) const KDF_T_COST: u32 = 2;

pub(crate) const KDF_P_COST: u32 = 1;

pub(crate) const SALT_LEN: usize = 16;

pub(crate) const NONCE_LEN: usize = 12;

pub(crate) const KEY_LEN: usize = 32;

// ── Wire format ────────────────────────────────────────────────────────────

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
pub(crate) struct BundlePayload {
    #[serde(default)]
    pub(crate) preferences: BundlePreferences,
    #[serde(default)]
    pub(crate) credentials: Vec<WorkBuddyCredential>,
    #[serde(default)]
    pub(crate) api_keys: Vec<ApiKeyEntry>,
    /// **Inbound** gateway accounts (`client_accounts.rs`).
    ///
    /// Carried in the same file as the outbound identity pool because the user
    /// is moving "my gateway" to another machine, and a gateway that arrives
    /// without its client keys would lock its own users out. The two remain
    /// separate lists — they are opposite directions and never merge.
    ///
    /// Only the *hash* travels, so even a plain bundle carries no replayable
    /// key; the client's configured key keeps working on the target machine
    /// because verification compares hashes.
    #[serde(default)]
    pub(crate) client_accounts: Vec<crate::client_accounts::ClientAccount>,
}

/// Plaintext counts, kept outside the ciphertext so an encrypted bundle can be
/// previewed ("2 个账号 / 1 个密钥") before the passphrase is known.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BundleCounts {
    pub credentials: usize,
    pub api_keys: usize,
    /// Defaulted so a v1 envelope (which has no such field) still deserializes.
    #[serde(default)]
    pub client_accounts: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct KdfParams {
    pub(crate) algo: String,
    pub(crate) m_cost: u32,
    pub(crate) t_cost: u32,
    pub(crate) p_cost: u32,
    pub(crate) salt_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CipherBlob {
    pub(crate) algo: String,
    pub(crate) nonce_b64: String,
    pub(crate) ct_b64: String,
}

/// The on-disk document. One struct covers both kinds: the plain kind fills
/// `preferences`/`credentials`/`api_keys`, the encrypted kind fills `kdf` and
/// `cipher`, and `kind` says which. Two separate structs would mean two serde
/// definitions that could drift apart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BundleFile {
    pub(crate) format: String,
    pub(crate) version: u32,
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) exported_at_ms: i64,
    #[serde(default)]
    pub(crate) app_version: String,
    #[serde(default)]
    pub(crate) counts: BundleCounts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) checksum: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) preferences: Option<BundlePreferences>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) credentials: Vec<WorkBuddyCredential>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) api_keys: Vec<ApiKeyEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) client_accounts: Vec<crate::client_accounts::ClientAccount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kdf: Option<KdfParams>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cipher: Option<CipherBlob>,
}

// ── Public API types ───────────────────────────────────────────────────────
