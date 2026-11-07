//! Command dispatch for the server-hosted desktop console.
//!
//! ## What this is
//!
//! The gateway serves the real desktop front-end (see [`crate::console`]), whose
//! only transport seam is `invoke(command, args)`. This module answers that call
//! over HTTP: `POST /admin/api/rpc/<command>` with the argument object as the
//! body, returning `{"result": <command's value>}`.
//!
//! The command names and argument shapes are **the Tauri command signatures**,
//! deliberately. Renaming them here would mean patching 47 call sites in a UI
//! that is otherwise shared verbatim, and every patch is a place the two
//! front-ends could diverge again — which is exactly the failure this whole
//! approach exists to prevent.
//!
//! ## The three commands that cannot mean the same thing
//!
//! * `start_service` / `stop_service` — the process is systemd's. Refused with an
//!   explanation rather than faked; see [`crate::console`]'s module docs.
//! * `pool_export` — the desktop version opens a native "save as" panel and
//!   writes a file at a path the user chose. A server must not be handed the
//!   ability to write arbitrary paths, so the export is an HTTP download the
//!   browser saves instead. The UI already treats `cancelled` as a normal
//!   outcome, and it reports the returned path — see [`export_ack`] for why the
//!   download flow still satisfies that contract.
//!
//! Everything else is a straight adapter over the storage layer, sharing its
//! implementation with the REST surface in [`crate::admin_routes`] so the two
//! cannot disagree.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};
use std::sync::Arc;

use crate::admin;
use crate::admin_routes::AdminState;

/// Dispatch one command.
///
/// `args` is the object the UI passed to `invoke`. Unknown commands are an error
/// rather than a `null` result: a silent `null` would surface in the UI as an
/// empty panel with no explanation, which is the hardest kind of bug to report.
pub async fn dispatch(state: &Arc<AdminState>, command: &str, args: &Value) -> Result<Value> {
    let args = args.as_object().cloned().unwrap_or_default();

    match command {
        // ── Status / observation ────────────────────────────────────────
        "get_status" => Ok(state.process.status_json()),
        "get_stats" => admin::stats_today(state.stats.clone()).await,
        "get_session_metrics" => session_metrics(state).await,
        "get_logs" => Ok(json!({ "entries": state.logs.snapshot().await })),
        "get_request_logs" => {
            // The UI sends `{ filter: {...} }`; an absent filter means "first
            // page, no constraints", which the stats layer already defaults.
            let filter: crate::stats::RequestLogFilter = args
                .get("filter")
                .cloned()
                .filter(|v| !v.is_null())
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| anyhow!("日志筛选参数无效: {e}"))?
                .unwrap_or_default();
            let stats = state.stats.clone();
            let res = tokio::task::spawn_blocking(move || stats.query_request_logs(&filter))
                .await
                .map_err(|e| anyhow!("任务异常终止: {e}"))?
                .map_err(|e| anyhow!("查询请求日志失败: {e}"))?;
            crate::api::request_logs_json(&res).map_err(|e| anyhow!("{e}"))
        }
        "clear_logs" => {
            state.logs.clear().await;
            Ok(json!({ "ok": true }))
        }
        "clear_request_logs" => {
            let stats = state.stats.clone();
            tokio::task::spawn_blocking(move || stats.clear_request_logs())
                .await
                .map_err(|e| anyhow!("任务异常终止: {e}"))?
                .map_err(|e| anyhow!("清空请求日志失败: {e}"))?;
            Ok(json!({ "ok": true }))
        }

        // ── Settings ────────────────────────────────────────────────────
        "get_settings" => {
            let mut payload = admin::settings_json();
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("actual_port".into(), json!(state.process.bound_port));
            }
            Ok(payload)
        }
        "save_settings" => {
            // The UI sends `{ body: {...} }`, matching the Tauri signature
            // `save_settings(body: SaveSettingsBody)`.
            let body = args.get("body").cloned().unwrap_or(Value::Null);
            let update: admin::SettingsUpdate =
                serde_json::from_value(body).map_err(|e| anyhow!("设置参数无效: {e}"))?;
            let saved = admin::save_settings(update)?;
            // `hot_apply_client_auth` reads the saved flag itself, so an
            // unrelated save (no client_auth_enabled in the body) still keeps
            // the live gate in sync with the file.
            admin::hot_apply_client_auth(&state.client_auth).await;
            state
                .logs
                .push(
                    "INFO",
                    "设置已保存（服务器模式：端口/绑定的改动需 systemctl restart proxy-rs 才生效）"
                        .to_string(),
                )
                .await;
            Ok(saved)
        }
        "get_providers" => Ok(json!({ "providers": crate::providers::builtin_presets() })),
        "fetch_models" => fetch_models().await,
        "test_upstream" => test_upstream(state).await,

        // ── Identity pool ───────────────────────────────────────────────
        "wb_credentials_list" => admin::credentials_list(&state.pool).await,
        "api_keys_list" => admin::api_keys_list(),
        "wb_preferences" => Ok(admin::preferences_json()),
        "wb_credentials_toggle" => {
            let id = req_str(&args, "id")?;
            let enabled = req_bool(&args, "enabled")?;
            admin::set_credential_enabled(&id, enabled)
        }
        "wb_credentials_delete" => {
            let id = req_str(&args, "id")?;
            let out = admin::delete_credential(&id)?;
            state.pool.reload_from_store().await;
            Ok(out)
        }
        "api_keys_toggle" => {
            let id = req_str(&args, "id")?;
            let enabled = req_bool(&args, "enabled")?;
            let out = admin::set_key_enabled(&id, enabled)?;
            state.pool.reload_from_store().await;
            Ok(out)
        }
        "api_keys_delete" => {
            let id = req_str(&args, "id")?;
            let out = admin::delete_key(&id)?;
            state.pool.reload_from_store().await;
            Ok(out)
        }
        "api_keys_add" => {
            let key = req_str(&args, "key")?;
            let label = args
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let out = admin::add_key(&key, &label)?;
            state.pool.reload_from_store().await;
            Ok(out)
        }
        "wb_set_default_identity" => {
            let id = req_str(&args, "id")?;
            let out = admin::set_default_identity(&id)?;
            state.pool.reload_from_store().await;
            Ok(out)
        }
        "wb_reorder_identities" => {
            let ids: Vec<String> = args
                .get("ids")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("缺少 ids"))?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            let out = admin::reorder_identities(&ids)?;
            state.pool.reload_from_store().await;
            Ok(out)
        }
        "wb_sticky_reset" => {
            let cleared = state.pool.sticky_count().await;
            state.pool.reset_sticky().await;
            Ok(json!({ "ok": true, "cleared": cleared }))
        }
        "wb_set_schedule" => {
            let enabled = req_bool(&args, "enabled")?;
            let time = args
                .get("time")
                .and_then(Value::as_str)
                .unwrap_or("09:00")
                .to_string();
            // `growthEnabled` is camelCase because that is the Tauri parameter
            // name the UI passes; serde's rename would apply only to structs.
            let growth = args.get("growthEnabled").and_then(Value::as_bool);
            admin::save_schedule(enabled, &time, growth)
        }
        "wb_relogin" => {
            let body = args.get("body").ok_or_else(|| anyhow!("缺少 body"))?;
            let id = body
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("缺少 id"))?
                .to_string();
            let login_state = body
                .get("login_state")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("缺少 login_state"))?
                .to_string();
            let out = admin::relogin_credential(&id, &login_state)?;
            state.pool.reload_from_store().await;
            Ok(out)
        }

        // ── Account maintenance ─────────────────────────────────────────
        "wb_checkin_all" => {
            let client = reqwest::Client::new();
            let report = crate::workbuddy_auth::batch_claim_daily_checkin_forced(&client).await;
            let summary = format!(
                "手动打卡完成: 共 {} 个账号，成功 {} 个，已打卡 {} 个，失败 {} 个",
                report.total, report.success, report.already_checked_in, report.failed
            );
            state.logs.push("INFO", summary).await;
            Ok(serde_json::to_value(&report).unwrap_or(Value::Null))
        }
        "wb_checkin_single" => {
            let id = req_str(&args, "id")?;
            let credentials = crate::workbuddy_auth::load_credentials();
            let cred = credentials
                .iter()
                .find(|c| c.id == id)
                .ok_or_else(|| anyhow!("账号不存在: {id}"))?;
            let client = reqwest::Client::new();
            let result = crate::workbuddy_auth::claim_daily_checkin_forced(&client, cred).await;
            state
                .logs
                .push("INFO", format!("手动打卡 {id}: {}", result.message))
                .await;
            serde_json::to_value(&result).map_err(|e| anyhow!("{e}"))
        }
        "wb_growth_run" => {
            let client = reqwest::Client::new();
            let credentials: Vec<_> = crate::workbuddy_auth::load_credentials()
                .into_iter()
                .filter(|c| c.enabled)
                .collect();
            // Stream progress into the shared buffer so a *polling* console
            // (no event channel over HTTP) can show the same per-account lines
            // the desktop used to receive as Tauri events.
            let buf = state.growth_progress.clone();
            let progress = |p: crate::growth_runner::GrowthProgress| {
                if let Ok(v) = serde_json::to_value(&p) {
                    if let Ok(mut b) = buf.lock() {
                        b.push(v);
                    }
                }
            };
            let report = crate::growth_runner::run_pool_with_progress(
                &client,
                &credentials,
                None,
                false,
                Some(&progress),
            )
            .await;
            let summary = crate::growth_runner::summarize(&report);
            state.logs.push("INFO", summary).await;
            serde_json::to_value(&report).map_err(|e| anyhow!("{e}"))
        }
        "wb_growth_run_single" => {
            let id = req_str(&args, "id")?;
            let dry_run = args.get("dryRun").and_then(Value::as_bool).unwrap_or(false);
            let credentials = crate::workbuddy_auth::load_credentials();
            let cred = credentials
                .iter()
                .find(|c| c.id == id)
                .ok_or_else(|| anyhow!("账号不存在: {id}"))?;
            let client = reqwest::Client::new();
            let run = crate::growth_runner::run_account(&client, cred, None, dry_run).await;
            serde_json::to_value(&run).map_err(|e| anyhow!("{e}"))
        }
        // Drain the growth-progress buffer (take-and-clear: the next poll
        // sees only lines emitted since). Empty when no run is streaming.
        "get_growth_progress" => {
            let mut buf = state
                .growth_progress
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            Ok(json!({ "lines": std::mem::take(&mut *buf) }))
        }
        "wb_refresh_points" => admin::refresh_all_points(state).await,
        "wb_refresh_points_single" => {
            let id = req_str(&args, "id")?;
            admin::refresh_one_points(state, &id).await
        }
        // ── QR login (OAuth) ────────────────────────────────────────────
        //
        // The old "server mode doesn't support QR login" refusal assumed the
        // console had no way to *show* a QR code and the upstream callback
        // needed a local browser. Neither holds: the flow polls a remote
        // endpoint (no localhost callback involved — see `poll_oauth_token`),
        // and the QR arrives as an inline SVG the web console renders exactly
        // like the desktop window did. Same implementation, same UI, one less
        // desktop-only command.
        "wb_oauth_start" => {
            let client = reqwest::Client::new();
            let res = crate::workbuddy_auth::start_oauth_flow(&client, None)
                .await
                .map_err(|e| anyhow!("生成扫码授权失败: {e}"))?;
            state
                .logs
                .push("INFO", format!("已生成 WorkBuddy 扫码授权: {}", res.state))
                .await;
            Ok(json!({
                "state": res.state,
                "auth_url": res.auth_url,
                "qr_svg": res.qr_svg,
            }))
        }
        "wb_oauth_poll" => {
            let flow_state = req_str(&args, "state")?;
            let client = reqwest::Client::new();
            let res = crate::workbuddy_auth::poll_oauth_token(&client, None, &flow_state)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            match res {
                crate::workbuddy_auth::OAuthPollResult::Pending => {
                    Ok(json!({ "status": "pending" }))
                }
                crate::workbuddy_auth::OAuthPollResult::Failed { error } => {
                    state
                        .logs
                        .push(
                            "ERROR",
                            format!("WorkBuddy 扫码授权失败: {error} (state={flow_state})"),
                        )
                        .await;
                    Ok(json!({ "status": "failed", "error": error }))
                }
                crate::workbuddy_auth::OAuthPollResult::Success { credential } => {
                    state
                        .logs
                        .push(
                            "INFO",
                            format!(
                                "WorkBuddy 扫码授权成功并保存: {} ({})",
                                credential.label, credential.id
                            ),
                        )
                        .await;
                    // Same payload the desktop returned: the UI reads
                    // credential.label / credential.id and re-renders the pool.
                    let masked = admin::credential_json(&credential, 0, false);
                    state.pool.reload_from_store().await;
                    Ok(json!({
                        "status": "success",
                        "credential": masked,
                    }))
                }
            }
        }

        // ── Inbound access accounts ─────────────────────────────────────
        "client_accounts_list" => Ok(admin::client_accounts_json(&state.client_auth).await),
        "client_account_create" => {
            let label = req_str(&args, "label")?;
            let out = admin::create_client_account(&label, "")?;
            state.client_auth.reload_from_store().await;
            Ok(out)
        }
        "client_account_toggle" => {
            let id = req_str(&args, "id")?;
            let enabled = req_bool(&args, "enabled")?;
            let out = admin::set_client_account_enabled(&id, enabled)?;
            state.client_auth.reload_from_store().await;
            Ok(out)
        }
        "client_account_reissue" => {
            let id = req_str(&args, "id")?;
            let out = admin::reissue_client_account_key(&id)?;
            state.client_auth.reload_from_store().await;
            Ok(out)
        }
        "client_account_delete" => {
            let id = req_str(&args, "id")?;
            let out = admin::delete_client_account(&id)?;
            state.client_auth.reload_from_store().await;
            Ok(out)
        }

        // ── Pool transfer ───────────────────────────────────────────────
        "pool_export" => export_ack(state, &Value::Object(args.clone())).await,
        "pool_import_preview" => {
            let body = args.get("body").ok_or_else(|| anyhow!("缺少 body"))?;
            let text = body
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("缺少导入内容"))?;
            let passphrase = body.get("passphrase").and_then(Value::as_str);
            let mode = body.get("mode").and_then(Value::as_str).unwrap_or("merge");
            admin::preview_pool(text, passphrase, mode)
        }
        "pool_import" => {
            let body = args.get("body").ok_or_else(|| anyhow!("缺少 body"))?;
            let text = body
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("缺少导入内容"))?;
            let passphrase = body.get("passphrase").and_then(Value::as_str);
            let mode = body.get("mode").and_then(Value::as_str).unwrap_or("merge");
            let out = admin::apply_pool_import(text, passphrase, mode)?;
            state.pool.reload_from_store().await;
            state.client_auth.reload_from_store().await;
            Ok(out)
        }

        // ── Client-config writers ───────────────────────────────────────
        "get_claude_config" => {
            Ok(crate::claude_config::read_current().map_err(|e| anyhow!("{e}"))?)
        }
        "apply_claude_config" => {
            let body = args.get("body").cloned().unwrap_or(Value::Null);
            apply_claude_config(state, body)
        }
        "get_codex_config" => get_codex_config(),
        "apply_codex_config" => apply_codex_config(state.process.bound_port).await,
        // DSH writes into a profile's patch file. The implementation lives in
        // `admin` because the REST surface offers the same operation, and one
        // implementation is what keeps the two from disagreeing about which
        // provider rows they rewrite.
        "get_dsh_config" => crate::admin::read_dsh_config(),
        "apply_dsh_config" => crate::admin::apply_dsh_config(state).await,

        // ── Local sessions (会话 page) ──────────────────────────────────
        // Argument names mirror the Tauri signatures: `args` is the paging
        // object, and the detail commands take a bare `id` (+ `agentId`).
        "claude_sessions_list" => {
            let list_args = args
                .get("args")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| anyhow!("会话参数无效: {e}"))?
                .unwrap_or_default();
            crate::admin::claude_sessions_list(Some(state.stats.clone()), list_args).await
        }
        "claude_sessions_detail" => {
            let id = req_str(&args, "id")?;
            let detail_args = args
                .get("args")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| anyhow!("会话参数无效: {e}"))?
                .unwrap_or_default();
            crate::admin::claude_session_detail(Some(state.stats.clone()), id, detail_args).await
        }
        "claude_sessions_subagent" => {
            let id = req_str(&args, "id")?;
            let agent_id = req_str(&args, "agentId")?;
            let detail_args = args
                .get("args")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| anyhow!("会话参数无效: {e}"))?
                .unwrap_or_default();
            crate::admin::claude_subagent_detail(id, agent_id, detail_args).await
        }

        // The two commands whose meaning does not survive the move to a server.
        // Refused with the reason the operator needs, not faked: a button that
        // silently does nothing is worse than one that says why. The shared UI
        // renders this message in a toast (`操作失败: …`).
        "start_service" | "stop_service" => bail!(
            "服务器模式下服务由 systemd 托管，请用 systemctl {} proxy-rs",
            if command == "start_service" {
                "start"
            } else {
                "stop"
            }
        ),

        other => bail!("未知命令: {other}"),
    }
}

// ── Argument helpers ───────────────────────────────────────────────────────

fn req_str(args: &Map<String, Value>, name: &str) -> Result<String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("缺少参数 {name}"))
}

fn req_bool(args: &Map<String, Value>, name: &str) -> Result<bool> {
    args.get(name)
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow!("缺少参数 {name}"))
}

// ── Commands needing more than an adapter ──────────────────────────────────

/// One session-metrics row, as the console's speed panel reads it.
///
/// A local copy of the desktop app's mapping rather than a shared helper: the
/// desktop version lives in the Tauri crate, which this library cannot depend
/// on. The field list is pinned by the UI, and `get_session_metrics` is the one
/// command whose payload shape the console indexes by name, so a test asserts it.
fn session_metrics_json(m: &crate::stats::SessionMetrics) -> Value {
    json!({
        "session_id": m.session_id,
        "model": m.model,
        "last_at": m.last_at,
        "turns": m.turns,
        "measured_turns": m.measured_turns,
        "output_tokens": m.output_tokens,
        "model_ms": m.model_ms,
        "speed_tps": m.tps(),
        "avg_ttft_ms": m.avg_ttft_ms(),
        "tool_wait_ms": m.tool_wait_ms(),
        "tool_waits": m.tool_waits,
        "requests": m.turns,
        "input_tokens": m.input_tokens,
        "cache_read_tokens": m.cache_read_tokens,
        "cache_hit_pct": m.cache_hit_pct(),
        "subagent_sessions": m.subagent_sessions,
        "is_subagent": m.is_subagent,
    })
}

/// Session speed metrics: the current session plus the recent ones it compares
/// against.
async fn session_metrics(state: &Arc<AdminState>) -> Result<Value> {
    let stats = state.stats.clone();
    let (latest, recent) = tokio::task::spawn_blocking(move || {
        let latest = stats.query_latest_session_metrics()?;
        let recent = stats.query_recent_session_metrics(3)?;
        Ok::<_, anyhow::Error>((latest, recent))
    })
    .await
    .map_err(|e| anyhow!("任务异常终止: {e}"))?
    .map_err(|e| anyhow!("会话指标查询失败: {e}"))?;

    // One mapping for both, so a field added here cannot go missing from
    // `recent` — that is how the two silently drift apart.
    let mut payload = session_metrics_json(&latest);
    let entries: Vec<Value> = recent.iter().map(session_metrics_json).collect();
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("recent".into(), Value::Array(entries));
    }
    Ok(payload)
}

/// Fetch the upstream model list for the configured provider.
async fn fetch_models() -> Result<Value> {
    let settings = crate::settings::GuiSettings::load();
    let preset = settings.models_preset();
    let client = reqwest::Client::new();
    let models = crate::providers::fetch_models(&client, &preset, &settings.api_key)
        .await
        .map_err(|e| anyhow!("拉取模型失败: {e}"))?;
    Ok(json!({ "models": models }))
}

/// Round-trip a trivial completion through the configured upstream.
async fn test_upstream(state: &Arc<AdminState>) -> Result<Value> {
    let started = std::time::Instant::now();
    let settings = crate::settings::GuiSettings::load();
    let presets = crate::providers::builtin_presets();
    let url = settings.chat_url(&presets);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default();
    // The completion model the proxy would use for a plain request: the
    // configured override, else whatever the client asked for (here a literal
    // probe name — the point is to prove the endpoint and credential work, not
    // to pin a model).
    let model = {
        let s = crate::settings::GuiSettings::load();
        if s.completion_model.trim().is_empty() {
            "hy4std".to_string()
        } else {
            s.completion_model.trim().to_string()
        }
    };
    let body = json!({
        "model": model,
        "max_tokens": 8,
        "messages": [{ "role": "user", "content": "ping" }],
    });
    let resp = client
        .post(&url)
        .bearer_auth(&settings.api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("连接上游失败: {e}"))?;
    let status = resp.status();
    let ms = started.elapsed().as_millis() as u64;
    state
        .logs
        .push(
            "INFO",
            format!("上游连通性测试: {} -> {}, {} ms", url, status, ms),
        )
        .await;
    Ok(json!({
        "ok": status.is_success(),
        "status": status.as_u16(),
        "url": url,
        "latency_ms": ms,
    }))
}

/// Tell the UI an export is ready, because the bytes travel by download.
///
/// The desktop flow is: open a native panel → write the file → reply with the
/// path so the UI can offer "在文件夹中显示". A browser has neither the panel nor
/// the path, so the browser downloads `GET /admin/api/pool/export` itself and this
/// reply only needs to say "the download has been triggered".
///
/// It still has to satisfy the UI's checks: `ok` truthy, `counts` present (it
/// prints them), `encrypted` truthful, and `empty` honest — an export with no
/// accounts must read as a warning, not a success, because the user's next step
/// is to trust that file on another machine.
async fn export_ack(state: &Arc<AdminState>, args: &Value) -> Result<Value> {
    let body = args.get("body").cloned().unwrap_or(Value::Null);
    let encrypted = body
        .get("passphrase")
        .and_then(Value::as_str)
        .map(|p| !p.trim().is_empty())
        .unwrap_or(false);

    let credentials = crate::workbuddy_auth::load_credentials();
    let keys = crate::workbuddy_auth::load_api_keys();
    let empty = credentials.is_empty() && keys.is_empty();
    let mut warnings = Vec::new();
    if empty {
        warnings.push("本机身份池为空".to_string());
    }

    state
        .logs
        .push("INFO", "账号池导出：已生成下载".to_string())
        .await;

    Ok(json!({
        "ok": true,
        "encrypted": encrypted,
        "empty": empty,
        "warnings": warnings,
        "counts": {
            "credentials": credentials.len(),
            "api_keys": keys.len(),
        },
        // The browser owns the destination, so there is no server-side path. The
        // UI renders this string; saying so is more useful than an empty span or
        // a fake path.
        "path": "浏览器下载（服务器不落盘）",
        // Tells the client half of the flow to start the download. The desktop UI
        // ignores unknown keys, so this is additive.
        "download_url": "/admin/api/pool/export",
    }))
}

/// Write the Claude Code configuration, honouring the fields the form sent.
fn apply_claude_config(state: &Arc<AdminState>, body: Value) -> Result<Value> {
    let base_url = format!("http://127.0.0.1:{}", state.process.bound_port);
    let mut updates = Vec::new();
    for (slot, key) in [
        ("model", "model"),
        ("sonnet", "sonnet"),
        ("opus", "opus"),
        ("haiku", "haiku"),
    ] {
        if let Some(m) = body
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            updates.push(crate::claude_config::SlotUpdate {
                slot: slot.to_string(),
                model: m.to_string(),
                name: None,
            });
        }
    }
    if updates.is_empty() {
        bail!("未指定任何模型");
    }
    let value = crate::claude_config::apply_slots(&updates, &base_url)
        .map_err(|e| anyhow!("写入 Claude 配置失败: {e}"))?;
    Ok(value)
}

/// Report the Codex catalog wiring so the UI can show the current state.
fn get_codex_config() -> Result<Value> {
    let Some(config_path) = crate::codex_config::config_path() else {
        return Ok(json!({
            "supported": false,
            "reason": "CODEX_HOME / HOME is not set",
        }));
    };
    let catalog_path = crate::codex_config::current_catalog_path(&config_path);
    Ok(json!({
        "supported": true,
        "config_path": config_path.display().to_string(),
        "config_exists": config_path.exists(),
        "catalog_path": catalog_path.as_ref().map(|p| p.display().to_string()),
        "catalog_exists": catalog_path.as_ref().map(|p| p.exists()).unwrap_or(false),
    }))
}

/// Generate `model_catalog_json` from the provider's live model list.
async fn apply_codex_config(bound_port: u16) -> Result<Value> {
    let config_path = crate::codex_config::config_path()
        .ok_or_else(|| anyhow!("CODEX_HOME / HOME 未设置，无法定位 Codex 配置"))?;
    let settings = crate::settings::GuiSettings::load();
    let preset = settings.models_preset();
    if settings.api_key.is_empty() {
        bail!("尚未配置 API Key，无法拉取模型列表");
    }
    let client = reqwest::Client::new();
    let models = crate::providers::fetch_models(&client, &preset, &settings.api_key)
        .await
        .map_err(|e| anyhow!("从上游 {} 拉取模型失败: {e}", preset.id))?;
    if models.is_empty() {
        bail!("上游 {} 未返回任何模型，未改动 Codex 配置", preset.id);
    }
    let (catalog_path, config_path) = crate::codex_config::write_catalog(&models, &config_path)
        .map_err(|e| anyhow!("写入 Codex 配置失败: {e}"))?;
    // Wire the provider and its key too. The catalog only fixes the *picker*;
    // without a provider block and a usable key, Codex still cannot make a
    // request to this gateway.
    let base_url = format!("http://127.0.0.1:{}/v1", bound_port);
    let env_path = crate::codex_config::apply_provider(&config_path, &base_url, "proxy_rs")
        .map_err(|e| anyhow!("写入 Codex provider 失败: {e}"))?;
    Ok(json!({
        "ok": true,
        "models": models.len(),
        "catalog_path": catalog_path.display().to_string(),
        "config_path": config_path.display().to_string(),
        "env_path": env_path.map(|p| p.display().to_string()),
        "key_written": crate::client_accounts::has_writable_key(),
        "note": "重启 Codex 后模型选择器生效",
    }))
}
