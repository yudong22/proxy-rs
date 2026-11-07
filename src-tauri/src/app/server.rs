//! The two listener lifecycles: the data plane and the console.

use crate::app::context::*;
use crate::app::tray::*;
use crate::*;

/// Monotonic id for the currently-running proxy server task. Incremented on
/// every (re)start so a stale `stop` request can't cancel a newer server.
pub(crate) fn next_server_id(ctx: &Arc<AppContext>) -> u16 {
    ctx.server_epoch
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        + 1
}

/// Spawn (or respawn) the proxy listener. A fresh `CancellationToken` governs
/// this specific server instance so it can be shut down independently of the
/// whole process. Errors are logged and the service is marked stopped.
pub(crate) fn run_proxy_server(
    app: tauri::AppHandle,
    ctx: Arc<AppContext>,
    server_id: u16,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let ctx_for_watch = ctx.clone();
    spawn_supervised(ctx_for_watch, "代理监听", async move {
        // If a newer start was requested while we were spinning up, bail out.
        if ctx.server_epoch.load(std::sync::atomic::Ordering::SeqCst) != server_id {
            return;
        }

        let initial_settings = ctx.settings.read().await.clone();
        let cfg = match Config::from_settings(&initial_settings) {
            Ok(cfg) => cfg,
            Err(e) => {
                ctx.logs.push("ERROR", format!("代理配置无效: {}", e)).await;
                ctx.service_ctrl.mark_stopped();
                update_tray_state(&app, &ctx);
                emit_service_state(&app);
                return;
            }
        };

        let metrics_handle = metrics::install();
        let config_arc = Arc::new(cfg);

        let app_router = router::build_app_router(
            ctx.service_ctrl.clone(),
            ctx.logs.clone(),
            config_arc.clone(),
            ctx.client.clone(),
            ctx.stats.clone(),
            metrics_handle,
            ctx.credential_pool.clone(),
            ctx.client_auth.clone(),
        );

        let bind_addr = config_arc.bind.clone();
        // No `+1` fallback: every client CLI is pointed at one fixed gateway
        // URL, so binding a different port would break them invisibly. The
        // single-instance guard is what keeps this port free.
        let (listener, bound_port) = match service::ServiceController::bind(
            &bind_addr,
            config_arc.port,
            ctx.service_ctrl.allow_port_fallback(),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                let in_use = e.kind() == std::io::ErrorKind::AddrInUse;
                let hint = if in_use {
                    "（端口已被占用：请先退出另一个 Proxy RS 实例，或改用其它端口）"
                } else {
                    ""
                };
                ctx.logs
                    .push(
                        "ERROR",
                        format!("代理监听端口绑定失败 {}: {}{}", config_arc.port, e, hint),
                    )
                    .await;
                ctx.service_ctrl.mark_stopped();
                update_tray_state(&app, &ctx);
                emit_service_state(&app);
                return;
            }
        };

        // Another start may have superseded this one before we bound.
        if ctx.server_epoch.load(std::sync::atomic::Ordering::SeqCst) != server_id {
            let _ = listener.local_addr();
            return;
        }

        ctx.bound_port
            .store(bound_port, std::sync::atomic::Ordering::SeqCst);
        ctx.service_ctrl.mark_running();
        update_tray_state(&app, &ctx);
        emit_service_state(&app);
        ctx.logs
            .push(
                "INFO",
                format!("代理服务已在 http://{}:{} 启动", bind_addr, bound_port),
            )
            .await;

        // Peer-address extraction, same as the server binary: the desktop's
        // desktop-boot endpoint answers loopback requests only.
        let server = axum::serve(
            listener,
            app_router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move { shutdown.cancelled().await });

        if let Err(e) = server.await {
            ctx.logs
                .push("ERROR", format!("代理服务运行异常: {}", e))
                .await;
        }

        // Only clear state if this is still the active server instance.
        if ctx.server_epoch.load(std::sync::atomic::Ordering::SeqCst) == server_id {
            ctx.service_ctrl.mark_stopped();
            update_tray_state(&app, &ctx);
            emit_service_state(&app);
        }
    });
}

/// Start (or restart) the proxy listener.
/// Start the console listener on its own port, for the life of the process.
///
/// ## Why it is separate from the proxy
///
/// The console is where "启动服务 / 停止服务" live. When both shared one
/// listener, stopping the proxy closed the port that served the console, so the
/// page lost the very button needed to start it again — a dead end the user
/// hit for real. (The data plane already answers 503 while stopped, so nothing
/// about the proxy's own behaviour needed the port closed.)
///
/// ## Where the port comes from
///
/// `proxy port + 1`, falling back up to `+9` if those are taken. No setting:
/// the console is an implementation detail of the app, and a second port in
/// `gui-settings.json` would be one more thing to keep in step with `port`.
/// The bootstrap page reads it from `get_bootstrap_info`.
pub(crate) fn start_console_listener(ctx: Arc<AppContext>) {
    let (bind, base_port) = {
        let settings = settings::GuiSettings::load();
        let bind = if settings.bind.trim().is_empty() {
            "127.0.0.1".to_string()
        } else {
            settings.bind.trim().to_string()
        };
        let port = if settings.port == 0 {
            DEFAULT_PORT
        } else {
            settings.port
        };
        (bind, port)
    };

    tauri::async_runtime::spawn(async move {
        let admin_state = Arc::new(proxy_rs::admin_routes::AdminState {
            sessions: ctx.admin_sessions.clone(),
            stats: ctx.stats.clone(),
            logs: ctx.logs.clone(),
            client_auth: ctx.client_auth.clone(),
            pool: ctx.credential_pool.clone(),
            process: proxy_rs::admin::ProcessInfo {
                version: env!("CARGO_PKG_VERSION"),
                // The proxy's live port: the console reports the port clients
                // should be pointed at, not the console's own.
                bound_port: ctx.bound_port.clone(),
                started_at: ctx.started_at,
                bind_addr: bind.clone(),
            },
            client: ctx.client.clone(),
            desktop: true,
            growth_progress: ctx.growth_progress.clone(),
        });
        let router = proxy_rs::admin_routes::build_admin_router(admin_state.clone())
            .merge(proxy_rs::console::build_console_router(admin_state));

        let mut chosen = None;
        for offset in 1..=9u16 {
            let Some(port) = base_port.checked_add(offset) else {
                break;
            };
            match tokio::net::TcpListener::bind(format!("{bind}:{port}")).await {
                Ok(l) => {
                    chosen = Some((l, port));
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
                Err(e) => {
                    ctx.logs
                        .push("ERROR", format!("控制台端口绑定失败 {bind}:{port}: {e}"))
                        .await;
                    return;
                }
            }
        }
        let Some((listener, port)) = chosen else {
            ctx.logs
                .push(
                    "ERROR",
                    format!("控制台端口绑定失败：{base_port} 之后的 9 个端口都不可用"),
                )
                .await;
            return;
        };

        ctx.console_port
            .store(port, std::sync::atomic::Ordering::SeqCst);
        ctx.logs
            .push(
                "INFO",
                format!("控制台已在 http://{bind}:{port}/admin/ 就绪"),
            )
            .await;

        let server = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        );
        if let Err(e) = server.await {
            ctx.logs
                .push("ERROR", format!("控制台服务异常退出: {e}"))
                .await;
        }
    });
}

pub(crate) fn start_proxy_server(app: tauri::AppHandle, ctx: Arc<AppContext>) {
    // Already up: nothing to do.
    if ctx.service_ctrl.is_running() {
        return;
    }
    // The listener stays up while the service is stopped (it also serves the
    // console), so "start" after a "stop" is a **resume**: the port is already
    // bound and the routes are already mounted. Re-binding would fail with
    // "address in use" for a port we ourselves are holding.
    //
    // `bound_port` is the discriminator: it is non-zero exactly while a
    // listener exists. It is zero before the first bind and stays non-zero
    // after a stop (the port keeps answering 503).
    if ctx.bound_port.load(std::sync::atomic::Ordering::SeqCst) != 0 {
        ctx.service_ctrl.mark_running();
        update_tray_state(&app, &ctx);
        emit_service_state(&app);
        return;
    }
    // No listener at all (first start): bind one.
    // Cancel any existing server and bump the epoch so stale tasks exit.
    if let Ok(guard) = ctx.server_shutdown.lock() {
        guard.cancel();
    }
    let server_id = next_server_id(&ctx);
    let shutdown = tokio_util::sync::CancellationToken::new();
    if let Ok(mut guard) = ctx.server_shutdown.lock() {
        *guard = shutdown.clone();
    }
    run_proxy_server(app, ctx, server_id, shutdown);
}

/// Stop the proxy listener entirely (graceful shutdown). Unlike `pause`, this
/// actually closes the port so the old 503 behavior is gone. The server task
/// refreshes the tray once it has fully shut down.
pub(crate) fn stop_proxy_server(ctx: Arc<AppContext>) {
    // **The listener stays up.** It also serves the console, so closing it
    // would take the "启动服务" button down together with the service it is
    // supposed to restart — the user could not start it again. The data plane
    // already refuses work while the service is stopped (`proxy.rs` answers
    // 503 `service_unavailable`), so marking the state is what "stopped"
    // means; the port keeps answering, and the console keeps answering with
    // the truth.
    //
    // `bound_port` is deliberately left alone: the port is still listening, and
    // it is what tells a later "start" that it should *resume* rather than try
    // to bind a port we already hold.
    ctx.service_ctrl.mark_stopped();
}
