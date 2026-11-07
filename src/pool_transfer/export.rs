//! Building a bundle from the local stores and writing it to disk.

use super::*;

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
        self.counts.credentials == 0
            && self.counts.api_keys == 0
            && self.counts.client_accounts == 0
    }
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
    let client_accounts = crate::client_accounts::load_accounts();
    let prefs = workbuddy_auth::load_preferences();

    if let crate::workbuddy_auth::StoreState::Corrupt(detail) =
        crate::client_accounts::store_health()
    {
        warnings.push(format!(
            "访问账户文件已损坏，本次导出会缺少客户端访问密钥（{detail}）。修复或删除该文件后重试：{}",
            crate::client_accounts::accounts_path().display()
        ));
    }

    if credentials.is_empty()
        && api_keys.is_empty()
        && client_accounts.is_empty()
        && warnings.is_empty()
    {
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
        client_accounts,
    };
    (payload, warnings)
}

/// Checksum over the payload's canonical JSON.
///
/// This is a *tamper detector*, not a signature: it catches a hand-edited or
/// truncated file and tells the user their edits were seen. MD5 is therefore the
/// right tool, and reusing [`md5_hex`] avoids a second hash implementation.
pub(crate) fn payload_checksum(payload: &BundlePayload) -> Result<String> {
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
        client_accounts: payload.client_accounts.len(),
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
        client_accounts: payload.client_accounts.clone(),
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
        // Same rule as the two above: nothing secret may survive in the
        // envelope. The account hashes are not usable keys, but they are still
        // credential-derived material and the envelope's contract is "counts
        // and KDF parameters only".
        bundle.client_accounts = Vec::new();
        bundle.kdf = Some(kdf);
        bundle.cipher = Some(cipher);
    }

    // An unencrypted export is a file the user carries around and may leave in
    // Downloads. It has always contained live upstream credentials, but the
    // inbound *access keys* were safe to include because only their hashes were
    // stored. Now that the plaintext is kept (so client configs can be written),
    // including it here would turn a "move my pool to another machine" file into
    // a list of working gateway keys.
    //
    // Stripped from the plaintext branch only. The encrypted branch already drops
    // every secret, and a user who wants the keys to travel can encrypt — which
    // is where that material belongs.
    for account in &mut bundle.client_accounts {
        account.key_plaintext = None;
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
    if let Some(home) = crate::settings::home_dir() {
        let downloads = home.join("Downloads");
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
pub(crate) fn export_stamp(now_ms: i64, offset_secs: i64) -> String {
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

/// Back up `path` (if present) and replace it atomically.
///
/// The temp-file + rename dance is what makes an interrupted import recoverable:
/// a reader either sees the old file or the new one, never a half-written JSON
/// document. `secret` applies 0600, which the two token-bearing stores need and
/// the preferences file does not.
pub(crate) fn write_json_atomic<T: Serialize>(path: &Path, value: &T, secret: bool) -> Result<()> {
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
