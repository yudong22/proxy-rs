//! Process-local hooks for the desktop app's own capabilities.
//!
//! ## Why this exists
//!
//! The desktop console is served over HTTP by the gateway itself, and the
//! webview loads it from `http://127.0.0.1:<port>/admin/` — an origin the
//! Tauri runtime does **not** inject its IPC into (a page on that origin has
//! no `window.__TAURI__`; only `tauri://localhost` gets one). So the desktop
//! cannot reach its own native capabilities over Tauri IPC from the console
//! page, and cannot reach them over HTTP either — there is no HTTP handler
//! that can call `std::process::Command`.
//!
//! This module closes that gap without a second transport: the desktop
//! **registers** its capabilities here at startup, and the RPC layer calls
//! them in-process. The request still arrives over HTTP (one wire format, one
//! authorisation path); only the *last hop* is a direct function call rather
//! than a message.
//!
//! ## What is registered
//!
//! Exactly the capabilities that are process/OS state rather than business
//! logic:
//!
//! * start / stop the service (they control the very listener that serves the
//!   console — a self-locking call over HTTP would need a separate channel)
//! * restart the service, which is how a saved setting reaches the running
//!   router (`Config` is a snapshot taken at listener build time)
//! * open the log directory / reveal a path in the file manager
//! * report the console listener's port, and open **that one URL** in the
//!   system default browser (`open_session_in_browser`)
//!
//! ## One URL, not "a URL"
//!
//! Opening a browser is the one capability here that hands a string to another
//! program which will happily act on it, so it is deliberately **not** a
//! general-purpose "open whatever I am given" hook. The only URL this module
//! will ever produce or forward is
//! `http://127.0.0.1:<本机控制台端口>/admin/#sessions/<uuid>`, composed by
//! [`session_console_url`] from a validated session id and a port the desktop
//! supplied. See that function and [`open_url`] for why every wider shape was
//! rejected.
//!
//! Everything else (settings, credentials, sessions, growth, OAuth, …) is
//! ordinary business logic and lives in [`crate::admin_rpc`] for **both**
//! builds — which is the point of the whole exercise: one implementation, not
//! two.
//!
//! ## The server build
//!
//! Registers nothing. The RPC arms then answer with the "systemd owns the
//! lifecycle" explanation, which is the truth for a headless deployment.

use std::sync::{Mutex, OnceLock};

pub type SimpleHook = std::sync::Arc<dyn Fn() -> Result<String, String> + Send + Sync>;
pub type ArgHook = std::sync::Arc<dyn Fn(String) -> Result<String, String> + Send + Sync>;

/// The desktop capabilities the RPC layer can invoke.
#[derive(Clone, Default)]
pub struct DesktopHooks {
    /// Start the proxy listener. Returns a human-readable outcome.
    pub start_service: Option<SimpleHook>,
    /// Stop the proxy listener.
    pub stop_service: Option<SimpleHook>,
    /// Rebuild the **running** proxy listener from the settings just saved.
    ///
    /// `Config` is a snapshot taken when the listener is built
    /// (`Config::from_settings`), so everything derived from settings — the
    /// upstream URL, `model_map`, `sanitize_terms`, `force_stream`,
    /// `intercept_safety_classifier` — only exists inside that snapshot. Saving
    /// the file therefore changes nothing until the listener is rebuilt, and
    /// this hook is what rebuilds it.
    ///
    /// Why a hook rather than something in this crate: only the desktop owns a
    /// restartable listener. A headless server registers nothing, because
    /// systemd is what restarts it (`systemctl restart proxy-rs`), and its
    /// console says so.
    ///
    /// A `None` answer means "this build has no restartable listener", which the
    /// caller reports with the systemd wording rather than as an error: the save
    /// itself succeeded either way.
    pub restart_service: Option<SimpleHook>,
    /// Open the log directory in the OS file manager.
    pub open_logs_dir: Option<SimpleHook>,
    /// Reveal a path in the OS file manager.
    pub reveal_path: Option<ArgHook>,
    /// 控制台监听端口（`127.0.0.1:<console_port>`），由桌面宿主在 `setup` 里注册。
    ///
    /// 为什么必须是钩子、而不是从 [`crate::admin_state::ProcessInfo`] 读：那个
    /// `bound_port` 是**数据面**端口（客户端 CLI 指向的 `/v1` 入口），控制台跑在
    /// 另一个端口上（桌面侧在 `proxy_port+1..+9` 里挑一个实际绑定的）。两者往往
    /// 只差 1，指错端口的表现不是「打不开」，而是「打开了但 404/空白」，所以这个
    /// 端口只能由真正持有控制台监听的进程自己回答。
    ///
    /// 桌面端在控制台绑定成功后才写入真实端口，之前读到 0 —— 调用方必须把 0 当作
    /// 「控制台未就绪」报错，不能拼出 `:0`，也不能回落到数据面端口。
    pub console_port: Option<PortHook>,
    /// 在系统默认浏览器里打开**本网关自己的控制台地址**。
    ///
    /// 这是个只做一件事的钩子：入参必须是 [`session_console_url`] 拼出的
    /// `http://127.0.0.1:<port>/admin/#sessions/<uuid>`，执行方（Tauri 宿主）
    /// 会再校验一次前缀才 spawn。
    ///
    /// 为什么不让它「打开任意 URL」：这个钩子的入口是一条 HTTP RPC，控制台页面
    /// 里的任何一段脚本（包括将来某次 XSS、或某个被塞进来的第三方资源）都能调用
    /// 它。一旦接受任意字符串，攻击者就能让用户的**默认浏览器**打开 `file://` 下
    /// 的本地文件、或一个钓鱼站点——那是把浏览器变成攻击者的执行器，而且是用户
    /// 自己点开的。所以边界放在这里：URL 由后端按 id 拼，前端只说「哪个会话」。
    pub open_url: Option<ArgHook>,
    /// The console's status payload, built by the desktop host.
    ///
    /// A server reports a fixed shape (`managed_by: "systemd"`, always
    /// running). The desktop's truth is different and dynamic: whether the
    /// service is running right now, whether launchd owns the login item, and
    /// whether this is a dev build that must not write one. The desktop
    /// registers this and the RPC layer prefers it — that is what lets
    /// `get_status` exist in **one** place instead of two.
    pub status: Option<StatusHook>,
}

/// A hook that produces the console status payload.
pub type StatusHook = std::sync::Arc<dyn Fn() -> serde_json::Value + Send + Sync>;

/// A hook that reports the console listener's port (`0` = not listening yet).
///
/// A separate type from [`SimpleHook`] because the answer is a number the caller
/// composes a URL out of, not prose to display: parsing it back out of a string
/// would be the kind of round-trip that turns "controls the console's address"
/// into "mostly controls it".
pub type PortHook = std::sync::Arc<dyn Fn() -> u16 + Send + Sync>;

/// The only URL shape this module will ever hand to a browser.
///
/// ## Why it is built here and not by the caller
///
/// Because it is the security boundary. `session_id` must already have passed
/// [`crate::claude_sessions::is_valid_session_id`] (strict lowercase hex UUID);
/// `console_port` must be the port the *console* listener actually bound. Given
/// those two, the result is always
///
/// ```text
/// http://127.0.0.1:<console_port>/admin/#sessions/<session_id>
/// ```
///
/// — a fixed scheme, a fixed host literal, a checked numeric port and a fixed
/// path prefix. No input can move any of those, which is exactly why the front
/// end is never allowed to send a URL of its own: with `file://`, an arbitrary
/// origin, or a `?query` in play, the console page could steer the user's
/// default browser anywhere.
///
/// ## Two traps this encodes
///
/// 1. **`console_port`, not the data-plane port.** The two differ by as little
///    as 1 and the wrong one is not an error the user can see: the browser opens
///    something that answers 404 or renders blank. `0` therefore means "the
///    console is not listening yet" and is an error here — never `:0`, and never
///    a silent fallback to the data plane.
/// 2. **No query parameters.** The `#` is part of the URL and must survive into
///    the browser. The Windows launcher is `cmd /C start`, where an `&` in the
///    URL would be read as a command separator; keeping the URL query-free keeps
///    that from ever mattering.
pub fn session_console_url(session_id: &str, console_port: u16) -> Result<String, String> {
    session_console_url_for(None, session_id, console_port)
}

/// The same, for a named source.
///
/// `None` means the default source, and that is the shape the old address
/// (`#sessions/<id>`) already carries — it is still produced, so a URL minted by
/// 3.5.5 keeps working. A named source adds one segment:
/// `#sessions/<slug>/<id>`.
///
/// The two inputs that reach the URL are still only an enumerated slug and an
/// id validated by that source's own checker, so the "no scheme, host or query
/// can be expressed" argument is unchanged. `is_console_url` is deliberately not
/// touched: it validates up to `/admin/` and never looked at the hash.
pub fn session_console_url_for(
    source: Option<&str>,
    session_id: &str,
    console_port: u16,
) -> Result<String, String> {
    if console_port == 0 {
        return Err("控制台未就绪：控制台端口尚未绑定，无法打开会话页面".to_string());
    }
    let src = match source {
        None => crate::sessions::default_source(),
        Some(slug) => crate::sessions::by_slug(slug).map_err(|e| e.to_string())?,
    };
    if !src.is_valid_id(session_id) {
        return Err(format!("会话 id 无效: {session_id}"));
    }
    Ok(match source {
        // The default source keeps the historical two-segment shape, so a link
        // handed out by 3.5.5 (or by a script that hardcoded it) still resolves.
        None => format!("http://127.0.0.1:{console_port}/admin/#sessions/{session_id}"),
        Some(slug) => {
            format!("http://127.0.0.1:{console_port}/admin/#sessions/{slug}/{session_id}")
        }
    })
}

/// Whether `url` is the one shape [`session_console_url`] produces.
///
/// Used as the second, independent check immediately before a URL is handed to
/// the OS launcher. Deliberately a **prefix + shape** test, not a substring
/// test: a `file://` URL that merely mentions `http://127.0.0.1:` in its query
/// must not pass, and `http://127.0.0.1:example.com/…` (host parsing tricks)
/// must not either — so the prefix is compared against the literal scheme and
/// the rest is required to stay inside the console's own path space.
pub fn is_console_url(url: &str) -> bool {
    const PREFIX: &str = "http://127.0.0.1:";
    const PATH: &str = "/admin/";
    // Scheme + host literal first. `starts_with` on the literal also excludes
    // `http://127.0.0.1.evil.example/` (no colon follows the host there).
    let Some(rest) = url.strip_prefix(PREFIX) else {
        return false;
    };
    // Then `<port><path>`: the port is digits only and non-zero, and the path
    // must be our own `/admin/` prefix — anything else (a `@`, a second host, a
    // `?` before the path) fails here.
    let Some((port, tail)) = rest.split_once('/') else {
        return false;
    };
    if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) || port == "0"
    {
        return false;
    }
    format!("/{tail}").starts_with(PATH)
}

static HOOKS: OnceLock<Mutex<Option<DesktopHooks>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<DesktopHooks>> {
    HOOKS.get_or_init(|| Mutex::new(None))
}

/// Register the desktop capabilities. Called once, at desktop startup.
///
/// Overwrites any previous registration, which is what a restart of the
/// in-process listener implies.
pub fn register(hooks: DesktopHooks) {
    let mut guard = slot().lock().unwrap_or_else(|p| p.into_inner());
    *guard = Some(hooks);
}

/// Forget the desktop capabilities.
///
/// Used when the desktop tears its listener down, so a later RPC cannot reach
/// a capability whose process state is gone.
pub fn unregister() {
    let mut guard = slot().lock().unwrap_or_else(|p| p.into_inner());
    *guard = None;
}

/// Whether the desktop registered its capabilities.
pub fn is_registered() -> bool {
    slot().lock().unwrap_or_else(|p| p.into_inner()).is_some()
}

/// Invoke one hook, or report that this build has none.
///
/// The error text names the systemd alternative, because that is what a server
/// operator should be told — and it is also what the desktop's own UI shows
/// when the capability is genuinely unavailable.
pub fn call(
    what: &str,
    f: impl FnOnce(
        &DesktopHooks,
    ) -> Option<std::sync::Arc<dyn Fn() -> Result<String, String> + Send + Sync>>,
) -> Result<String, String> {
    let guard = slot().lock().unwrap_or_else(|p| p.into_inner());
    let Some(registered) = guard.as_ref() else {
        return Err(format!(
            "服务器模式下服务由 systemd 托管，请用 systemctl {what} proxy-rs"
        ));
    };
    match f(registered) {
        Some(h) => h(),
        None => Err(format!(
            "服务器模式下服务由 systemd 托管，请用 systemctl {what} proxy-rs"
        )),
    }
}

/// The console listener's port, or `None` when this build has none (a server).
///
/// `Some(0)` is a valid answer and means "the desktop is up but its console
/// listener has not bound yet": callers must treat it as not-ready rather than
/// as a port.
pub fn console_port() -> Option<u16> {
    let guard = slot().lock().unwrap_or_else(|p| p.into_inner());
    guard
        .as_ref()
        .and_then(|h| h.console_port.as_ref())
        .map(|f| f())
}

/// 「在浏览器打开某个会话」 —— the whole desktop half of that feature.
///
/// Takes the **session id only** and returns `(url, message)`. There is
/// deliberately no variant that accepts a URL: this function is the security
/// boundary itself, so the only string a caller can influence is a session id
/// that must pass the strict UUID check inside [`session_console_url`].
///
/// ## The order of the gates matters
///
/// 1. **Is there a desktop at all?** Checked first, so a headless server gets
///    the honest "your console is already in the browser" answer. Checking the
///    port first would instead report "控制台未就绪" — a server *has* no console
///    port to wait for, and that message would send an operator hunting a
///    startup problem that does not exist.
/// 2. **Is the console listening?** `None`/`0` → "控制台未就绪". Never `:0`, and
///    never a fallback to the data-plane port: that would open a URL that
///    answers 404 and look like a broken feature rather than a timing problem.
/// 3. **Compose** — [`session_console_url`], the single producer of the one
///    allowed shape.
/// 4. **Re-check the shape** via [`is_console_url`] before the hook runs, so a
///    future refactor that starts building the string somewhere else still has
///    to pass this.
/// 5. **Then** invoke the desktop's launcher (which checks the prefix a third
///    time before spawning).
pub fn open_session_in_browser(session_id: &str) -> Result<(String, String), String> {
    open_session_in_browser_for(None, session_id)
}

/// The same, for a named source.
///
/// The extra input is a **source slug**, not a URL: the caller still cannot
/// express a scheme, a host, a port or a query, which is the entire reason this
/// boundary exists. The slug is looked up in the registry (an enum, not a free
/// string) and the id is then validated by that source's own checker.
pub fn open_session_in_browser_for(
    source: Option<&str>,
    session_id: &str,
) -> Result<(String, String), String> {
    let guard = slot().lock().unwrap_or_else(|p| p.into_inner());
    let Some(registered) = guard.as_ref() else {
        // No hooks at all: this is a server deployment, where the console the
        // operator is clicking in *is* the browser tab. Opening "it" again has
        // no meaning, and pretending it worked would be a lie about an action
        // the user asked for explicitly.
        return Err("服务器控制台本身就在浏览器里，直接复制浏览器地址即可".to_string());
    };
    let Some(open) = registered.open_url.clone() else {
        return Err("当前运行环境不支持在浏览器中打开会话".to_string());
    };
    let port = registered.console_port.as_ref().map(|f| f()).unwrap_or(0);
    let url = session_console_url_for(source, session_id, port)?;
    // Second check, independent of how `url` was produced. See the doc comment.
    if !is_console_url(&url) {
        return Err(format!(
            "拒绝打开非本机控制台地址（只允许 http://127.0.0.1:<端口>/admin/…）: {url}"
        ));
    }
    let message = open(url.clone())?;
    Ok((url, message))
}

/// The desktop's status payload, or `None` when this build has none (a server).
pub fn status() -> Option<serde_json::Value> {
    let guard = slot().lock().unwrap_or_else(|p| p.into_inner());
    guard.as_ref().and_then(|h| h.status.as_ref()).map(|f| f())
}

/// The same, for the hook that takes an argument (`reveal_path`).
pub fn call_with_arg(
    what: &str,
    arg: String,
    f: impl FnOnce(
        &DesktopHooks,
    )
        -> Option<std::sync::Arc<dyn Fn(String) -> Result<String, String> + Send + Sync>>,
) -> Result<String, String> {
    let guard = slot().lock().unwrap_or_else(|p| p.into_inner());
    let Some(registered) = guard.as_ref() else {
        return Err(format!(
            "服务器模式下服务由 systemd 托管，请用 systemctl {what} proxy-rs"
        ));
    };
    match f(registered) {
        Some(h) => h(arg),
        None => Err(format!(
            "服务器模式下服务由 systemd 托管，请用 systemctl {what} proxy-rs"
        )),
    }
}
