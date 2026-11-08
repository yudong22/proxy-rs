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

        // Settings are re-read from disk on every (re)start, not taken from the
        // cached `ctx.settings`. The cache is a snapshot of process start, and a
        // restart triggered by "保存配置" exists precisely to pick up what the
        // save just wrote — reading the cache here would rebuild the listener
        // with the same stale values it is being restarted to replace.
        let initial_settings = {
            let fresh = settings::GuiSettings::load();
            *ctx.settings.write().await = fresh.clone();
            fresh
        };
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

        // The listener is gone at this point (`axum::serve` has returned and
        // dropped it), so the port is free again — publish that *before* anything
        // else. `restart_proxy_server` waits on exactly this going to zero, and
        // leaving it set would make a settings save think the old listener was
        // still holding the port.
        //
        // Skipped when a newer instance is already current, so a respawn cannot
        // have its own `bound_port` cleared by the instance it replaced.
        if ctx.server_epoch.load(std::sync::atomic::Ordering::SeqCst) == server_id {
            ctx.bound_port.store(0, std::sync::atomic::Ordering::SeqCst);
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
    //
    // Skipped when no listener is bound. `restart_proxy_server` cancels the old
    // listener and then calls this, and the terminating task marks the service
    // stopped on its way out — so "running" can briefly be true while no socket
    // exists. Trusting the flag there would turn a settings-driven rebuild into
    // a no-op and leave the app reporting a service that is not listening.
    if ctx.service_ctrl.is_running()
        && ctx.bound_port.load(std::sync::atomic::Ordering::SeqCst) != 0
    {
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

/// Rebuild the **running** listener so a settings save takes effect.
///
/// ## Why this cannot be a plain stop + start
///
/// `stop_proxy_server` only marks the service stopped; the listener, its router
/// and — crucially — the `Arc<Config>` inside it stay alive so the console keeps
/// answering on the same port. `start_proxy_server` therefore sees
/// `bound_port != 0`, treats the call as *resume*, and returns without rebuilding
/// anything. That is correct for the start/stop button and wrong for a settings
/// save, which is why this is a separate path: it cancels the live listener,
/// waits for the port to be released, and only then starts a fresh one.
///
/// Waiting is the load-bearing part. `cancelled()` makes `axum::serve` begin a
/// graceful shutdown, but the socket is not closed until the task actually
/// returns; starting the replacement before that would fail to bind its own
/// port ("address in use" for a port we ourselves are holding) and leave the
/// app stopped — worse than not reloading at all.
///
/// A stopped service stays stopped: before this fix, saving settings while the
/// service was stopped would silently *start* it, which is not what the save
/// asked for.
pub(crate) fn restart_proxy_server(app: tauri::AppHandle, ctx: Arc<AppContext>) {
    let was_running = ctx.service_ctrl.is_running();
    let had_listener = ctx.bound_port.load(std::sync::atomic::Ordering::SeqCst) != 0;

    if had_listener {
        // Cancel this instance's graceful shutdown and wait for it to let go of
        // the port. `bound_port` is zeroed by the task itself once `axum::serve`
        // has returned, so its going to zero is the signal that the socket is
        // actually closed — and therefore that a rebind can succeed.
        //
        // The epoch is deliberately *not* bumped here. Bumping it would make the
        // terminating task believe it had been superseded, and it would then skip
        // the cleanup that zeroes `bound_port` — so the wait below could never
        // finish. The bump happens in `start_proxy_server`, after the port is
        // free.
        if let Ok(guard) = ctx.server_shutdown.lock() {
            guard.cancel();
        }

        let ctx_for_wait = ctx.clone();
        let app_for_wait = app.clone();
        tauri::async_runtime::spawn(async move {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while ctx_for_wait
                .bound_port
                .load(std::sync::atomic::Ordering::SeqCst)
                != 0
                && std::time::Instant::now() < deadline
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            if ctx_for_wait
                .bound_port
                .load(std::sync::atomic::Ordering::SeqCst)
                != 0
            {
                // The old listener never let go. Say so instead of rebinding into
                // a port that is still held — and do not pretend the save applied.
                ctx_for_wait
                    .logs
                    .push(
                        "ERROR",
                        "设置已保存，但旧监听器未能及时释放端口，未重启代理（请手动停止后启动服务）"
                            .to_string(),
                    )
                    .await;
                return;
            }
            ctx_for_wait
                .logs
                .push(
                    "INFO",
                    "设置已保存，正在按新配置重启代理服务...".to_string(),
                )
                .await;
            start_proxy_server(app_for_wait, ctx_for_wait);
        });
        return;
    }

    // No listener yet: nothing to rebuild. Honour the state that was there, so a
    // save made while stopped does not start the service behind the user's back.
    if was_running {
        start_proxy_server(app, ctx);
    }
}
