use anyhow::{bail, Result};
use std::path::PathBuf;

/// Manage a macOS LaunchAgent for "launch at login".
/// Label: com.proxy-rs.gui
const LABEL: &str = "com.proxy-rs.gui";

pub fn plist_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", LABEL))
}

pub fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().map_err(|e| anyhow::anyhow!("cannot resolve current exe: {}", e))
}

/// Escape the five XML predefined entities.
///
/// The plist is assembled by string interpolation of paths that the user
/// controls (the app can live under a directory whose name contains `&` or
/// `<`). An unescaped path would produce an invalid plist — or, worse, inject
/// additional keys into the job definition.
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn plist_content(exe: &str, workdir: &str, process_log_path: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>--gui-child</string>
    </array>
    <key>WorkingDirectory</key>
    <string>{workdir}</string>
    <key>RunAtLoad</key>
    <true/>
    <!-- Restart after a crash, but not after a clean exit.
         `<true/>` would relaunch the process even when the single-instance
         guard asked it to exit(0), which turns "another copy is already
         running" into an endless launchd restart loop. Letting a clean exit
         stick keeps the single-instance guard usable for every copy of the
         app, including the launchd child. -->
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <!-- Floor between restarts so a genuinely broken binary cannot spin. -->
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = LABEL,
        exe = xml_escape(exe),
        workdir = xml_escape(workdir),
        log = xml_escape(process_log_path)
    )
}

/// Path for launchd's own stdout/stderr capture.
///
/// Deliberately **not** `proxy.log`: that file has exactly one writer, the
/// `LogBuffer` writer thread, which appends whole lines through a single
/// handle. launchd's stdout/stderr redirection is a second, independent writer
/// that knows nothing about that ordering, so pointing both at one file lets
/// the OS splice a write into the middle of a line the app is writing —
/// corrupting the very log lines used to diagnose request problems.
fn process_log_path() -> Option<PathBuf> {
    Some(crate::settings::log_dir()?.join("process.log"))
}

/// Whether launchd currently has the job registered.
///
/// This — not the presence of the plist file — is what "launch at login is
/// on" actually means. The two can disagree: a user-initiated quit boots the
/// job out but deliberately leaves the plist behind so the next login still
/// auto-starts.
pub fn is_loaded() -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let Ok(uid) = uid() else {
        return false;
    };
    std::process::Command::new("launchctl")
        .args(["print", &format!("gui/{}/{}", uid, LABEL)])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Write the plist if it does not already match what this build would install.
///
/// File-only: never touches launchd. Returns whether the file was rewritten.
/// Split out so the launchd-managed copy can bring the on-disk definition up to
/// date without booting out the job that is currently running it.
pub fn rewrite_plist_if_stale() -> Result<bool> {
    if !cfg!(target_os = "macos") {
        bail!("launch-at-login is only supported on macOS");
    }
    let exe = current_exe()?;
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let path = plist_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Keep the process capture in ~/.proxy-rs/logs so an app reinstall keeps
    // history. Separate from proxy.log on purpose — see [`process_log_path`].
    let log_path = process_log_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/tmp/proxy-rs-process.log".to_string());

    let desired = plist_content(
        &exe.display().to_string(),
        &workdir.display().to_string(),
        &log_path,
    );
    let current = std::fs::read_to_string(&path).ok();
    if plist_needs_write(current.as_deref(), &desired) {
        std::fs::write(&path, &desired)?;
        return Ok(true);
    }
    Ok(false)
}

/// Whether the startup path may (re-)register the login item.
///
/// Two conditions, both required:
///
/// - Not the launchd-managed copy. Registering means `bootout` followed by
///   `bootstrap`, and that copy *is* the running job: launchd kills it
///   part-way through the call, which booted out the user's own login item
///   while the GUI still reported launch-at-login as enabled.
/// - The job is not already loaded. When it is, there is nothing to re-arm and
///   calling `install()` anyway would `bootstrap` a job whose `RunAtLoad` starts
///   a *second* app process on every single launch.
///
/// The job is unloaded but the plist present after a user-initiated quit, so
/// this is also what restores launch-at-login afterwards.
pub fn should_rearm_at_startup(
    launch_at_login: bool,
    is_launchd_child: bool,
    job_loaded: bool,
) -> bool {
    launch_at_login && !is_launchd_child && !job_loaded
}

/// Whether `install()` must rewrite the plist on disk.
///
/// Kept separate from the launchd calls so the "no change means do not touch the
/// job" rule is directly testable — that rule is what stops a redundant
/// `bootstrap` from launching a second copy of the app.
pub fn plist_needs_write(current: Option<&str>, desired: &str) -> bool {
    current != Some(desired)
}

/// Install (or repair) the login item: bring the plist up to date, then make
/// sure launchd has the job loaded.
///
/// Idempotent by design. Unconditionally running `bootout` + `bootstrap` looks
/// harmless but is not: `bootstrap` honours `RunAtLoad`, so a redundant call
/// launches a *second* copy of the app, and the `bootout` half kills whichever
/// copy was already serving the fixed proxy port.
pub fn install() -> Result<PathBuf> {
    if !cfg!(target_os = "macos") {
        bail!("launch-at-login is only supported on macOS");
    }
    let path = plist_path();
    let changed = rewrite_plist_if_stale()?;

    if !changed && is_loaded() {
        return Ok(path);
    }

    // Only boot out when something is actually registered; `bootout` on an
    // unknown job reports a non-zero status that is deliberately ignored.
    if is_loaded() {
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &format!("gui/{}/{}", uid()?, LABEL)])
            .output();
    }
    let out = std::process::Command::new("launchctl")
        .args([
            "bootstrap",
            &format!("gui/{}", uid()?),
            &path.display().to_string(),
        ])
        .output()?;
    if !out.status.success() {
        let legacy = std::process::Command::new("launchctl")
            .args(["load", &path.display().to_string()])
            .output()?;
        if !legacy.status.success() {
            bail!(
                "launchctl bootstrap failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    Ok(path)
}

pub fn uninstall() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("launch-at-login is only supported on macOS");
    }
    let path = plist_path();
    if path.exists() {
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &format!("gui/{}/{}", uid()?, LABEL)])
            .output();
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &path.display().to_string()])
            .output();
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Unload the job but keep the plist, so the next login still auto-starts.
///
/// This is what a user-initiated quit needs. Booting the job out first removes
/// launchd's reason to relaunch while leaving the plist on disk so
/// launch-at-login survives for next time.
///
/// Safe to call when the job is not loaded: `bootout` on an unknown job reports
/// a non-zero status that is deliberately ignored.
pub fn suspend() -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    if !plist_path().exists() {
        return Ok(());
    }
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("gui/{}/{}", uid()?, LABEL)])
        .output();
    Ok(())
}

/// Whether this process was started by launchd rather than by a user.
///
/// The plist passes `--gui-child` as its second argument. It exists because the
/// launchd-managed copy must not re-arm its own job while starting up: doing so
/// would `bootout` the very job that launched it.
pub fn running_as_launchd_child(args: impl IntoIterator<Item = String>) -> bool {
    args.into_iter().any(|a| a == "--gui-child")
}

/// Whether a plist file exists on disk.
///
/// Deliberately **not** the answer to "is launch-at-login on" — that is
/// [`is_loaded`]. A user-initiated quit boots the job out while leaving the
/// plist behind so the next login still auto-starts, so this stays `true` while
/// nothing is actually registered. Reported to the UI only as a diagnostic.
pub fn plist_exists() -> bool {
    plist_path().exists()
}

fn uid() -> Result<u32> {
    let out = std::process::Command::new("id").arg("-u").output()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim()
        .parse::<u32>()
        .map_err(|e| anyhow::anyhow!("cannot parse uid: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn gui_child_flag_is_recognised() {
        // The plist's exact argv, so the flag stays in sync with the plist.
        assert!(running_as_launchd_child(args(&[
            "/Applications/Proxy RS.app/Contents/MacOS/proxy-rs-gui",
            "--gui-child"
        ])));
    }

    #[test]
    fn a_user_launch_is_not_mistaken_for_a_launchd_child() {
        assert!(!running_as_launchd_child(args(&[
            "/Applications/Proxy RS.app/Contents/MacOS/proxy-rs-gui"
        ])));
        // macOS passes `-psn_…` when launching from Finder; it must not match.
        assert!(!running_as_launchd_child(args(&[
            "/Applications/Proxy RS.app/Contents/MacOS/proxy-rs-gui",
            "-psn_0_12345"
        ])));
    }

    #[test]
    fn plist_restarts_after_a_crash_but_not_after_a_clean_exit() {
        // A bare `<true/>` KeepAlive relaunches even after the single-instance
        // guard's exit(0), which makes every duplicate launch an endless
        // restart loop. The restart decision must be conditioned on a failed
        // exit instead.
        let content = plist_content("/tmp/app", "/tmp", "/tmp/process.log");
        assert!(content.contains("<key>KeepAlive</key>"));
        assert!(content.contains("<key>SuccessfulExit</key>"));
        assert!(
            !content.contains("<key>KeepAlive</key>\n    <true/>"),
            "KeepAlive must not be unconditionally true"
        );
        assert!(content.contains("<key>ThrottleInterval</key>"));
        assert!(content.contains("<string>--gui-child</string>"));
        assert!(content.contains("<key>RunAtLoad</key>"));
    }

    #[test]
    fn plist_captures_process_output_outside_the_apps_own_log() {
        // launchd's stdout/stderr redirection does not take `LogBuffer`'s write
        // mutex, so sharing `proxy.log` lets the OS splice a write into the
        // middle of a line the app is writing. The two files must stay distinct.
        let content = plist_content("/tmp/app", "/tmp", "/tmp/process.log");
        assert!(content.contains("/tmp/process.log"));
        assert!(
            !content.contains("proxy.log"),
            "the process capture must not be the app's own log file"
        );

        let capture = process_log_path().expect("log dir resolves");
        let app_log = crate::settings::log_file_path().expect("log path resolves");
        assert_ne!(capture, app_log);
    }

    #[test]
    fn plist_escapes_paths_that_would_break_the_xml() {
        let content = plist_content("/tmp/a&b/<weird>", "/tmp/c\"d", "/tmp/e'f.log");
        assert!(content.contains("/tmp/a&amp;b/&lt;weird&gt;"));
        assert!(content.contains("/tmp/c&quot;d"));
        assert!(content.contains("/tmp/e&apos;f.log"));
        // No raw metacharacter survives inside the value.
        assert!(!content.contains("<weird>"));
    }

    #[test]
    fn xml_escape_leaves_ordinary_paths_alone() {
        assert_eq!(
            xml_escape("/Applications/Anthropic Proxy.app/Contents/MacOS/app"),
            "/Applications/Anthropic Proxy.app/Contents/MacOS/app"
        );
    }

    #[test]
    fn the_launchd_copy_never_rearms_its_own_job() {
        // Re-arming means bootout + bootstrap; for the launchd copy that kills
        // the caller mid-call and can start a second, unguarded instance.
        assert!(!should_rearm_at_startup(true, true, false));
        assert!(!should_rearm_at_startup(true, true, true));
        // A user-launched copy with the setting on and no live job is the only
        // case that arms.
        assert!(should_rearm_at_startup(true, false, false));
        assert!(!should_rearm_at_startup(false, false, false));
        assert!(!should_rearm_at_startup(false, true, false));
    }

    #[test]
    fn a_healthy_job_is_left_alone() {
        // `install()` on an already-loaded job would bootstrap it again, and
        // `RunAtLoad` would start a second app process on every launch.
        assert!(!should_rearm_at_startup(true, false, true));
    }

    #[test]
    fn an_unchanged_plist_is_not_rewritten() {
        let desired = plist_content("/tmp/app", "/tmp", "/tmp/process.log");
        // Same bytes on disk: no write, so `install` can skip bootout/bootstrap
        // entirely and cannot spawn a duplicate.
        assert!(!plist_needs_write(Some(&desired), &desired));
        // Missing file, or a stale path/argv, must be rewritten.
        assert!(plist_needs_write(None, &desired));
        assert!(plist_needs_write(
            Some(&plist_content("/old/app", "/tmp", "/tmp/process.log")),
            &desired
        ));
    }

    #[test]
    fn install_is_idempotent_across_repeated_calls() {
        // Two identical installs must produce byte-identical plists; otherwise
        // the second one would look like a change and re-bootstrap the job.
        let a = plist_content("/tmp/app", "/tmp", "/tmp/process.log");
        let b = plist_content("/tmp/app", "/tmp", "/tmp/process.log");
        assert_eq!(a, b);
        assert!(!plist_needs_write(Some(&a), &b));
    }

    /// The plist is assembled by string interpolation, so a stray metacharacter
    /// or an illegal `--` inside a comment produces a file launchd will refuse
    /// to load. String assertions cannot catch that; `plutil` can.
    #[test]
    fn generated_plist_is_valid_xml() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let dir = std::env::temp_dir().join(format!("proxy-rs-plist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        // Include path characters that must be escaped, plus the real log path.
        let path = dir.join("com.proxy-rs.gui.test.plist");
        std::fs::write(
            &path,
            plist_content("/tmp/a&b/<weird>/proxy-rs-gui", "/tmp/c\"d", "/tmp/e'f.log"),
        )
        .expect("write plist");

        let out = std::process::Command::new("plutil")
            .args(["-lint", &path.display().to_string()])
            .output()
            .expect("plutil runs on macOS");
        assert!(
            out.status.success(),
            "generated plist is not valid: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_file(&path);
    }
}
