//! Where the harness and its profiles live.
//!
//! The filesystem addresses this writer edits, and the read-only scan that
//! picks which profile to edit when `DSH_PROFILE` does not name one.

use super::*;

/// Filename of a profile's user patch layer.
const PROFILE_PATCH_FILENAME: &str = "cordis.patch.yml";

/// Directory under the Harness home holding every profile.
const PROFILES_DIRNAME: &str = "profiles";

/// Shipped profile `dsh web` uses, and the fallback when nothing else matches.
const DEFAULT_PROFILE: &str = "web";

/// Harness home, honoring `DSH_HOME` and falling back to `~/.dsh`.
pub fn dsh_home() -> Option<PathBuf> {
    std::env::var("DSH_HOME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::settings::home_dir().map(|h| h.join(".dsh")))
}

/// Directory holding every profile.
pub fn profiles_dir() -> Option<PathBuf> {
    dsh_home().map(|h| h.join(PROFILES_DIRNAME))
}

/// Name of the profile whose patch file should be edited.
///
/// Resolution order, most authoritative first:
///
/// 1. `DSH_PROFILE`, when it names a directory that exists — the launcher sets
///    it, so this is exact whenever the GUI inherits the environment.
/// 2. The most recently modified profile whose patch file already declares this
///    provider: that is the profile the user's DSH is actually wired to.
/// 3. The most recently modified profile with a patch file at all.
/// 4. `web`, the shipped default for `dsh web`.
///
/// A GUI process normally has no `DSH_PROFILE`, which is why the on-disk
/// evidence outranks the guess in cases 2–3 rather than writing to `web` blind
/// and leaving a second, dead provider block behind.
pub fn active_profile() -> String {
    if let Some(name) = std::env::var("DSH_PROFILE")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        if profiles_dir()
            .map(|d| d.join(&name).is_dir())
            .unwrap_or(false)
        {
            return name;
        }
    }

    let Some(dir) = profiles_dir() else {
        return DEFAULT_PROFILE.to_string();
    };
    let mut declaring: Vec<(std::time::SystemTime, String)> = Vec::new();
    let mut any_patch: Vec<(std::time::SystemTime, String)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return DEFAULT_PROFILE.to_string();
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let patch = entry.path().join(PROFILE_PATCH_FILENAME);
        let Ok(meta) = std::fs::metadata(&patch) else {
            continue;
        };
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        if std::fs::read_to_string(&patch)
            .map(|text| declares_provider(&text))
            .unwrap_or(false)
        {
            declaring.push((modified, name.clone()));
        }
        any_patch.push((modified, name));
    }

    let newest = |mut v: Vec<(std::time::SystemTime, String)>| {
        v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        v.into_iter().next().map(|(_, name)| name)
    };
    newest(declaring)
        .or_else(|| newest(any_patch))
        .unwrap_or_else(|| DEFAULT_PROFILE.to_string())
}

/// Whether a patch document already wires this provider.
///
/// Both document shapes count: the profile patch list, where the namespace is
/// the entry's `id:` (`- id: llm-pi-ai`), and the retired `settings.yaml`, where
/// it is a top-level key. Requiring the provider key as well keeps this from
/// matching a document that merely mentions the namespace.
pub(crate) fn declares_provider(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    let namespaced = lines.iter().any(|l| {
        let t = l.trim_start();
        t.trim() == format!("{NAMESPACE}:")
            || t.strip_prefix("- ")
                .map(|rest| rest.trim() == format!("id: {NAMESPACE}"))
                .unwrap_or(false)
    });
    namespaced && lines.iter().any(|l| l.trim() == format!("{PROVIDER}:"))
}

/// The profile patch document this writer edits.
pub fn patch_path() -> Option<PathBuf> {
    profiles_dir().map(|d| d.join(active_profile()).join(PROFILE_PATCH_FILENAME))
}

/// The credential store.
pub fn credentials_path() -> Option<PathBuf> {
    dsh_home().map(|h| h.join(".credentials.yaml"))
}
