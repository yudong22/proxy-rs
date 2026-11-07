//! App lifecycle helpers: legacy migration, window focus, quit.

use crate::*;

// ── WorkBuddy credential pool management ───────────────────────────────────

/// Fold the legacy single `GuiSettings::api_key` into the key pool.
///
/// Called once at startup. The model-provider API Key field is removed from the
/// GUI (the key pool now owns all keys), but existing installs still have a key
/// there; importing it as a key entry means the unified 身份池 selector can
/// manage it and the proxy keeps using it. Existing keys take precedence: a key
/// whose material already exists is left untouched (its id is content-derived),
/// so we only add when the pool has nothing matching yet.
pub(crate) fn migrate_legacy_api_key(settings: &GuiSettings) {
    let key = settings.api_key.trim();
    if key.is_empty() {
        return;
    }
    let id = proxy_rs::workbuddy_auth::api_key_id(key);
    let items = proxy_rs::workbuddy_auth::load_api_keys();
    if items.iter().any(|k| k.id == id) {
        return;
    }
    let entry = proxy_rs::workbuddy_auth::ApiKeyEntry {
        id,
        label: "默认 API Key".to_string(),
        key: key.to_string(),
        enabled: true,
        points: None,
        points_fetched_at_ms: None,
    };
    let mut merged = items;
    merged.push(entry);
    let _ = proxy_rs::workbuddy_auth::save_api_keys(&merged);
}

// ── Upstream API key pool ──────────────────────────────────────────────────

/// Bring the main window to the front, restoring it if it was hidden.
pub(crate) fn focus_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Quit for real, surviving launch-at-login.
///
/// The LaunchAgent plist restarts the job when the process exits *unsuccessfully*
/// (`KeepAlive`/`SuccessfulExit=false`), so a plain `app.exit(0)` is enough to
/// stop the current run. Booting the job out as well is what makes "退出" mean
/// "do not come back": it removes launchd's record of the job, which would
/// otherwise relaunch it at the next login. The plist stays on disk so
/// launch-at-login is re-armed the next time the user starts the app with the
/// setting still enabled.
pub(crate) fn quit_app(app: &tauri::AppHandle) {
    if let Err(e) = launch_agent::suspend() {
        eprintln!("Warning: could not suspend launch-at-login before quitting: {e}");
    }
    app.exit(0);
}
