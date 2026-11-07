//! The upstream send/retry path.

use super::*;

/// Forward a translated request to the configured upstreams, with failover.
///
/// This is the single upstream send path for the Anthropic, Responses and Chat
/// Completions APIs, in both streaming and non-streaming mode. It walks
/// `config.chat_completions_urls()` in order, retrying the next upstream on a
/// connection error or a retriable status (429/5xx) and failing fast otherwise.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn forward_request(
    config: Arc<Config>,
    client: Client,
    mut openai_req: openai::OpenAIRequest,
    api_key: Option<String>,
    gui_logs: Arc<crate::settings::LogBuffer>,
    client_model: String,
    stats: Arc<StatsDb>,
    flavor: ApiFlavor,
    streaming: bool,
    route: &'static str,
    start: Instant,
    session: SessionInfo,
    tag: String,
    overrides: std::sync::Arc<std::sync::Mutex<OverrideTrace>>,
    pool: crate::session_pool::SharedCredentialPool,
    credential: Option<crate::workbuddy_auth::WorkBuddyCredential>,
) -> ProxyResult<Response> {
    let urls = config.chat_completions_urls();
    let mut last_err = None;

    // The credential chosen for THIS attempt. Starts as the pool's selection
    // (default-first, or the session's sticky replacement); the 4xx handler
    // below may fail over to a different credential and re-enter the loop.
    let mut active_credential = credential;
    let mut current_api_key = api_key;
    let mut current_key_id = if active_credential.is_none() {
        if let Some(entry) = crate::workbuddy_auth::active_api_key_entry() {
            Some(entry.id)
        } else if let Some(ref k) = current_api_key {
            crate::workbuddy_auth::enabled_api_keys()
                .into_iter()
                .find(|e| e.key.trim() == k.trim())
                .map(|e| e.id)
        } else {
            None
        }
    } else {
        None
    };
    let mut tried_key_ids = std::collections::HashSet::new();
    if let Some(ref kid) = current_key_id {
        tried_key_ids.insert(kid.clone());
    }

    // Some providers (WorkBuddy) only accept streaming bodies and answer a
    // non-streaming one with `11101 Non-stream chat request is currently not
    // supported`. For those, request a stream and aggregate it back into the
    // single response the client asked for, so the caller's `stream` flag is
    // honoured at the API boundary regardless.
    //
    // Providers are not all configured alike, so this is also discovered at
    // runtime: a non-streaming request that comes back 11101 is retried once
    // as a stream. That keeps `/v1/messages` without `stream:true` working
    // against a provider we do not know rejects plain bodies.
    let upstream_streaming = streaming || config.force_stream_upstream;
    if upstream_streaming {
        openai_req.stream = Some(true);
        openai_req.stream_options = Some(openai::StreamOptions {
            include_usage: true,
        });
    } else {
        openai_req.stream = Some(false);
        openai_req.stream_options = None;
    }

    // Neutralize content-filter fingerprints across every message field once,
    // before sending. This is the Rust port of the Go sanitize pass and covers
    // the user/assistant/tool/reasoning channels that a system-only scrub misses.
    // It runs for all three API flavors through this shared path (Anthropic,
    // Responses, Chat), so both Claude Code and Codex benefit.
    if config.sanitize_fingerprints {
        pipeline::sanitize_openai_request(&mut openai_req);
    }

    // Normalize message sequence so consecutive assistant messages are merged,
    // tool_calls are paired and ordered with tool_results, and empty tool calls are scrubbed.
    // Safe and idempotent for all endpoints (Anthropic, Responses, Chat).
    openai_req.messages = pipeline::normalize_message_sequence(openai_req.messages);

    // DSH's own system prompt carries two literals the gateway rejects as an
    // "unapproved channel" (`11-128`): its identity opener and the `DeepSeek
    // Harness Web GUI` phrase. Rewriting both here means a DSH request is served
    // on the first attempt instead of paying a failed round-trip and losing its
    // real system prompt to the degraded retry below.
    //
    // System messages only, and independent of `sanitize_fingerprints`: the same
    // literals appear throughout tool results (project paths, source code, `git
    // log` output) where they cause no rejection, so touching them would corrupt
    // user data for no benefit — the gateway inspects the system channel only.
    if pipeline::neutralize_harness_system_messages(&mut openai_req) > 0 {
        tracing::debug!("Rewrote DSH system-prompt literals rejected by the gateway");
    }

    // Stall guard: a content-policy block (WorkBuddy business code 11128) is
    // *not* a transient 5xx, and retrying with the identical fingerprinted body
    // only re-hits the same `unapproved channel` rejection. We make exactly one
    // "degraded" attempt per upstream URL: re-send with a neutral system prompt
    // so the user's real request still gets answered. This mirrors the Go
    // upstream's ErrContentBlocked → Degraded retry and stops Claude Code/Codex
    // from spinning on a bare 400.
    let mut degraded_attempt = false;

    // `free` night-node degradation: when a virtual `free` request resolved to
    // the night node (`hy4-preview`) and that node is exhausted across *every*
    // account and *every* key (no switchable replacement could be found), we
    // retry the request once against the daytime free node (`hy3`) rather than
    // erroring. `client_model` carries the originally-requested `free`, so the
    // stats/logs keep showing the virtual node while the served model reflects
    // the degraded `hy3`.
    let mut free_degraded_retried = false;

    // Credential attempts: the pool's pick first, then at most
    // `min(pool-1, 2)` failovers when a limit/failure exception hits. An empty
    // pool (or a static-key/passthrough configuration) yields exactly one
    // iteration with `active_credential = None` — the 1.7.1 behaviour.
    let pool_size = if pool.is_enabled().await && config.credential_pool_enabled {
        pool.snapshot().await.iter().filter(|c| c.enabled).count()
    } else {
        0
    };
    let enabled_keys_count = crate::workbuddy_auth::enabled_api_keys().len();
    let total_identities = pool_size + enabled_keys_count;
    let max_credential_attempts = if config.session_switch_enabled {
        total_identities.clamp(1, 5)
    } else {
        1
    };

    'credential: for credential_attempt in 0..max_credential_attempts {
        let mut current_credential = active_credential.clone();

        // A credential that a recent request already proved broken is skipped
        // outright, instead of re-paying the failing round-trip (and the 3 s
        // `not_found` wait) on every single request. Only the very first
        // attempt consults the memory; once the request is under way the
        // normal discovery path owns it, so a memory that turns out to be
        // wrong just costs one ordinary retry.
        if credential_attempt == 0 {
            if let Some(target) = remembered_failover(&pool, current_credential.as_ref()).await {
                match target {
                    FailoverTarget::Credential(id) => {
                        if let Some(next) = pool
                            .snapshot()
                            .await
                            .into_iter()
                            .find(|c| c.id == id && c.enabled)
                        {
                            if let Ok(mut t) = overrides.lock() {
                                t.set_key(next.id.clone(), "复用近期可用凭据");
                                t.set_model(
                                    openai_req.model.clone(),
                                    "复用近期可用凭据(跳过失败探测)",
                                );
                            }
                            active_credential = Some(next.clone());
                            current_credential = Some(next);
                        }
                    }
                    FailoverTarget::ApiKey(id) => {
                        if let Some(key) = crate::workbuddy_auth::enabled_api_keys()
                            .into_iter()
                            .find(|k| k.id == id)
                        {
                            if let Ok(mut t) = overrides.lock() {
                                t.set_key(key.id.clone(), "复用近期可用密钥");
                                t.set_model(
                                    openai_req.model.clone(),
                                    "复用近期可用密钥(跳过失败探测)",
                                );
                            }
                            tried_key_ids.insert(key.id.clone());
                            active_credential = None;
                            current_credential = None;
                            current_api_key = Some(key.key.clone());
                            current_key_id = Some(key.id.clone());
                        }
                    }
                }
            }
        }

        // Proactive renewal: a token already past (or within the margin of) its
        // expiry is renewed before spending a round-trip on it. This is what
        // keeps a long-idle app working on the first request after the login
        // state's access token expired.
        if let Some(cred) = current_credential.clone() {
            if cred.needs_refresh(crate::util::unix_millis()) {
                if let Ok(fresh) =
                    refresh_workbuddy_credential(&client, &cred, &pool, &gui_logs, &tag).await
                {
                    active_credential = Some(fresh.clone());
                    current_credential = Some(fresh);
                }
                // On failure fall through: the request will 401 and the
                // reactive handler below makes one more (bounded) attempt,
                // then fails over.
            }
        }

        // One token refresh per credential attempt: a 401 is retried once on
        // the same credential with a fresh token before any failover.
        let mut token_refresh_attempted = false;

        'url: for url in &urls {
            let mode = if streaming {
                "streaming"
            } else {
                "non-streaming"
            };

            // Up to three attempts against this URL: the original request, plus at
            // most one recovery retry of either kind — a content-policy degraded
            // retry, or an upgrade to a stream when the provider refuses plain
            // bodies (`11101`). A connect error or retriable 5xx instead moves on
            // to the next URL.
            for attempt in 0..=2 {
                tracing::debug!(
                    "Sending {} {}request to {} (model: {})",
                    mode,
                    flavor.label(),
                    url,
                    openai_req.model
                );

                let mut req_builder = client.post(url).json(&openai_req);
                // Credential auth is authoritative when present: the token must
                // not be accompanied by a static-key Authorization/x-api-key.
                req_builder = apply_upstream_auth(
                    req_builder,
                    &config,
                    &current_api_key,
                    current_credential.as_ref(),
                );
                if !streaming {
                    // Streaming requests get no per-request timeout on purpose: a
                    // total timeout hard-kills any stream that outlives it — exactly
                    // the "Codex occasionally dies mid-run" symptom when a long
                    // generation exceeds the limit. Stalls are instead caught by the
                    // shared client's idle `read_timeout` (see the GUI client
                    // builder), which only fires when the upstream stops sending.
                    req_builder = req_builder.timeout(Duration::from_secs(300));
                }

                let upstream_start = Instant::now();
                let response = match req_builder.send().await {
                    Ok(resp) => {
                        metrics::upstream_latency(
                            upstream_start.elapsed().as_secs_f64(),
                            "chat_completions",
                        );
                        resp
                    }
                    Err(err) => {
                        tracing::warn!("Failed to reach {}: {:?}", url, err);
                        metrics::upstream_error("chat_completions");
                        gui_logs
                            .push(
                                "ERROR",
                                format!(
                                    "UPSTREAM ERROR [connect] url={} model={} {}ms {} | {}",
                                    url,
                                    openai_req.model,
                                    upstream_start.elapsed().as_millis(),
                                    tag,
                                    err
                                ),
                            )
                            .await;
                        last_err = Some(ProxyError::Http(err));
                        continue 'url; // try next upstream URL
                    }
                };

                let status = response.status();
                if !status.is_success() {
                    let body = response
                        .text()
                        .await
                        .unwrap_or_else(|_| "Unknown error".to_string());
                    let elapsed = upstream_start.elapsed().as_millis();
                    metrics::upstream_error("chat_completions");
                    log_upstream_failure(
                        &gui_logs,
                        flavor.stage(),
                        url,
                        &openai_req.model,
                        status.as_u16(),
                        &body,
                        elapsed,
                        &tag,
                    )
                    .await;

                    // Limit/failure exception on an active credential → fail over.
                    // Only the session that hit the exception is relocated (the
                    // plan's default-first contract), the replacement sticks for
                    // that session so its prompt cache survives, and the current
                    // request is retried on the replacement immediately.
                    // ── 401 special-case: distinguish token expiry from routing failure ──
                    //
                    // Tencent / WorkBuddy upstreams return HTTP 401 for two very
                    // different root causes that call for opposite treatments:
                    //
                    //  a) {"message":"not_found"} — the gateway could not route to
                    //     the session / pod. The access token is fine; refreshing it
                    //     wastes ~200 ms and won't fix a routing hiccup. A 3s
                    //     wait lets the load-balancer resolve the bad path; if the
                    //     retry still fails we fall through to the account / key switch.
                    //
                    //  b) Any other 401 — the access token is stale or revoked.
                    //     Renew it and retry the *same* credential once before
                    //     switching; with a single account, failing over has nowhere
                    //     to go and would surface the 401 to the client directly.
                    if status.as_u16() == 401 && body.contains("not_found") {
                        // Only pay the exploratory wait while the failure is
                        // still unknown. Once a recent request has already
                        // proven this credential broken, re-probing it costs
                        // the client 3 s for an answer we can predict — go
                        // straight to the failover we know works.
                        let already_known = current_credential
                            .as_ref()
                            .map(|c| failover_is_known(&c.id))
                            .unwrap_or(false);
                        if attempt == 0 && !already_known {
                            // First attempt: brief 3s pause, then retry without refresh.
                            gui_logs
                                .push(
                                    "WARN",
                                    format!(
                                        "UPSTREAM 401 not_found (疑似瞬时路由故障), 3s 后重试同一凭据 model={} {}",
                                        openai_req.model, tag
                                    ),
                                )
                                .await;
                            tokio::time::sleep(Duration::from_millis(3000)).await;
                            continue; // attempt 0 → 1, no token refresh
                        }
                        // Known failure (or attempt ≥ 1): fall through to
                        // is_switchable_error → account / key failover.
                    } else if status.as_u16() == 401 && !token_refresh_attempted {
                        if let Some(cred) = current_credential.as_ref() {
                            // Normal 401: the access token is stale/revoked. Renew it
                            // and retry the *same* credential once before failing
                            // over: with a single account, failing over has nowhere
                            // to go and would just surface the 401 to the client.
                            token_refresh_attempted = true;
                            let updated = cred.clone();
                            if let Ok(fresh) = refresh_workbuddy_credential(
                                &client, &updated, &pool, &gui_logs, &tag,
                            )
                            .await
                            {
                                // A transparent renewal of the *default* account is
                                // not an identity override: only label it when the
                                // credential itself differs from the default.
                                if pool.configured_default_id().await.as_deref()
                                    != Some(fresh.id.as_str())
                                {
                                    if let Ok(mut t) = overrides.lock() {
                                        t.set_key(fresh.id.clone(), "401 刷新令牌后重试");
                                        t.set_model(openai_req.model.clone(), "401 refresh 后重试");
                                    }
                                }
                                active_credential = Some(fresh.clone());
                                current_credential = Some(fresh);
                                continue; // retry this URL with the renewed token
                            }
                        }
                    }

                    if is_switchable_error(status.as_u16(), &body) {
                        if let Some(cred) = current_credential.as_ref() {
                            let from = cred.id.clone();
                            // Attempt the switch BEFORE marking the failing
                            // credential as limited, so we can use the
                            // availability of a healthy replacement to decide
                            // how long the cooldown should be:
                            //
                            //  • Switch succeeds → full 60 s: the error is
                            //    likely account-specific (rate-limited, banned).
                            //  • Switch fails (pool exhausted) → short 5 s: the
                            //    error may be transient/global (e.g. upstream
                            //    hiccup hitting all accounts simultaneously).
                            //    A 60 s cooldown would make pool.is_enabled()
                            //    return false and take every subsequent request
                            //    offline for the full minute.
                            let replacement = if config.session_switch_enabled {
                                pool.switch(&session.session_id, &from).await
                            } else {
                                None
                            };
                            match replacement {
                                Some(next) => {
                                    // Park the failing credential while the
                                    // session moves to a replacement. A daily
                                    // quota-exhausted account (WorkBuddy business
                                    // codes 14018/11105/11106) is sidelined until
                                    // the next local midnight — its quota window
                                    // resets then, not in 60 s — so the pool
                                    // round-robins to the next healthy account
                                    // instead of re-picking the exhausted one
                                    // every minute. Any other switchable error
                                    // keeps the ordinary 60 s cooldown.
                                    let cooldown = if is_quota_business_code(&body) {
                                        crate::util::ms_until_local_midnight(60_000)
                                    } else {
                                        60_000
                                    };
                                    pool.mark_limited(&from, cooldown).await;
                                    gui_logs
                                        .push(
                                            "WARN",
                                            format!(
                                                "session 切换 from={} to={} reason={} {} session={}",
                                                from,
                                                next.id,
                                                status,
                                                tag,
                                                session.session_id
                                            ),
                                        )
                                        .await;
                                    if let Ok(mut t) = overrides.lock() {
                                        t.set_key(next.id.clone(), format!("{status} from {from}"));
                                        t.set_model(
                                            openai_req.model.clone(),
                                            format!("failover {status} from {from}"),
                                        );
                                    }
                                    active_credential = Some(next.clone());
                                    // Remember it: the next request must not
                                    // pay for this discovery again.
                                    remember_failover(
                                        &from,
                                        FailoverTarget::Credential(next.id.clone()),
                                    );
                                    continue 'credential; // retry the request on the replacement
                                }
                                None => {
                                    // No healthy replacement account available in the pool.
                                    // Apply short 5s cooldown so the pool recovers quickly.
                                    pool.mark_limited(&from, 5_000).await;

                                    // Fallback: check if API key pool has available keys for failover
                                    if config.session_switch_enabled {
                                        let available_keys =
                                            crate::workbuddy_auth::enabled_api_keys();
                                        if let Some(key) = available_keys
                                            .into_iter()
                                            .find(|k| !tried_key_ids.contains(&k.id))
                                        {
                                            tried_key_ids.insert(key.id.clone());
                                            gui_logs
                                                .push(
                                                    "WARN",
                                                    format!(
                                                        "账号池无可用替换账号 from={} reason={}，自动降级至密钥池: key_id={} label={} {} session={}",
                                                        from, status, key.id, key.label, tag, session.session_id
                                                    ),
                                                )
                                                .await;
                                            if let Ok(mut t) = overrides.lock() {
                                                t.set_key(
                                                    key.id.clone(),
                                                    format!("{status} from {from} (降级密钥池)"),
                                                );
                                                t.set_model(
                                                    openai_req.model.clone(),
                                                    format!("failover {status} from {from}"),
                                                );
                                            }
                                            active_credential = None;
                                            current_api_key = Some(key.key.clone());
                                            current_key_id = Some(key.id.clone());
                                            // The account pool was exhausted, so
                                            // the key pool is the only thing that
                                            // worked — remember exactly that.
                                            remember_failover(
                                                &from,
                                                FailoverTarget::ApiKey(key.id.clone()),
                                            );
                                            continue 'credential; // retry the request on the fallback API key
                                        }
                                    }

                                    gui_logs
                                        .push(
                                            "WARN",
                                            format!(
                                                "凭据池与密钥池均无可用替换身份 from={} reason={} {} (短暂冷却5s后恢复)",
                                                from, status, tag
                                            ),
                                        )
                                        .await;
                                }
                            }
                        } else if config.session_switch_enabled {
                            // Currently using API key (current_credential is None)
                            let from = current_key_id
                                .clone()
                                .unwrap_or_else(|| "api-key".to_string());
                            tried_key_ids.insert(from.clone());
                            let available_keys = crate::workbuddy_auth::enabled_api_keys();
                            if let Some(key) = available_keys
                                .into_iter()
                                .find(|k| !tried_key_ids.contains(&k.id))
                            {
                                tried_key_ids.insert(key.id.clone());
                                gui_logs
                                    .push(
                                        "WARN",
                                        format!(
                                            "密钥失败 from={} reason={}，自动轮询切换至下一密钥: key_id={} label={} {} session={}",
                                            from, status, key.id, key.label, tag, session.session_id
                                        ),
                                    )
                                    .await;
                                if let Ok(mut t) = overrides.lock() {
                                    t.set_key(key.id.clone(), format!("{status} from {from}"));
                                    t.set_model(
                                        openai_req.model.clone(),
                                        format!("failover {status} from {from}"),
                                    );
                                }
                                active_credential = None;
                                current_api_key = Some(key.key.clone());
                                current_key_id = Some(key.id.clone());
                                continue 'credential;
                            } else if config.credential_pool_enabled && pool.is_enabled().await {
                                if let Some(next) = pool.pick(&session.session_id).await {
                                    gui_logs
                                        .push(
                                            "WARN",
                                            format!(
                                                "密钥池已耗尽 from={} reason={}，自动切换至账号池: cred_id={} {} session={}",
                                                from, status, next.id, tag, session.session_id
                                            ),
                                        )
                                        .await;
                                    if let Ok(mut t) = overrides.lock() {
                                        t.set_key(
                                            next.id.clone(),
                                            format!("{status} from {from} (切换账号池)"),
                                        );
                                        t.set_model(
                                            openai_req.model.clone(),
                                            format!("failover {status} from {from}"),
                                        );
                                    }
                                    active_credential = Some(next);
                                    continue 'credential;
                                }
                            }

                            gui_logs
                                .push(
                                    "WARN",
                                    format!(
                                        "密钥池无可用替换密钥 from={} reason={} {}",
                                        from, status, tag
                                    ),
                                )
                                .await;
                        }
                    }

                    // ── `free` night-node degradation ──
                    // By now every switchable path has been exhausted for this
                    // attempt: pool→replacement, pool→key pool, key pool→next key,
                    // key pool→account pool. If this is a virtual `free` request
                    // whose resolved night node (`hy4-preview`) is depleted across
                    // the whole identity set, degrade to the daytime free node
                    // (`hy3`) and retry exactly once. We only do this for a
                    // quota/capacity signal (`14018`/`11105`/`11106` or a 429),
                    // never for a hard auth/permission failure (401/403/402) where
                    // `hy3` would fail identically. `client_model` stays `free`.
                    if !free_degraded_retried
                        && client_model == crate::translate::pipeline::FREE_MODEL_NAME
                        && openai_req.model == crate::translate::pipeline::free_night_model()
                        && (is_quota_business_code(&body) || status.as_u16() == 429)
                    {
                        free_degraded_retried = true;
                        let day = crate::translate::pipeline::free_degraded_model();
                        openai_req.model = day.clone();
                        // Reset identity state so the degraded request probes
                        // both the account pool and the key pool fresh.
                        active_credential = None;
                        current_credential = None;
                        current_api_key = None;
                        current_key_id = None;
                        tried_key_ids.clear();
                        gui_logs
                            .push(
                                "WARN",
                                format!(
                                    "free 夜间节点 {} 额度在账号池与密钥池均已耗尽，退化到 {} 重试 {} session={}",
                                    crate::translate::pipeline::free_night_model(),
                                    day,
                                    tag,
                                    session.session_id
                                ),
                            )
                            .await;
                        if let Ok(mut t) = overrides.lock() {
                            t.set_model(
                                &day,
                                format!(
                                    "free 夜间 {} 耗尽，退化到 {}",
                                    crate::translate::pipeline::free_night_model(),
                                    day
                                ),
                            );
                        }
                        continue 'credential; // retry the whole request on hy3
                    }

                    // Shape-dependent rejections (e.g. 11148 "tool calls and tool
                    // results do not match") can only be diagnosed from the message
                    // skeleton, so attach it whenever the upstream complains about
                    // tool-call pairing.
                    if body.contains("11148") || body.contains("11152") || body.contains("11133") {
                        gui_logs
                            .push(
                                "ERROR",
                                format!(
                                    "REQUEST SHAPE model={} {} | {}",
                                    openai_req.model,
                                    tag,
                                    describe_message_shape(&openai_req.messages)
                                ),
                            )
                            .await;
                    }

                    // Content-policy block → at most one degraded retry against the
                    // same URL, then surface a clear content_blocked error instead of
                    // a raw 400/502.
                    if is_content_blocked(status.as_u16(), &body)
                        && !degraded_attempt
                        && attempt == 0
                    {
                        degraded_attempt = true;
                        tracing::warn!(
                            "Upstream content block ({}); attempting one degraded retry",
                            status
                        );
                        gui_logs
                            .push(
                                "WARN",
                                format!(
                                "UPSTREAM content_blocked ({}) → 1 degraded retry (model={}) {}",
                                status, openai_req.model, tag
                            ),
                            )
                            .await;
                        apply_degraded_prompt(&mut openai_req);
                        if let Ok(mut t) = overrides.lock() {
                            t.set_model(&openai_req.model, format!("content_blocked {status}"));
                        }
                        continue; // attempt == 1: retry the SAME URL with the neutral prompt
                    }

                    // Safety net: a provider that only serves streaming bodies may
                    // not be known up front (custom URL rather than a known
                    // preset). If it rejects a plain body with `11101`, re-send as
                    // a stream and aggregate it back into the single body the
                    // client asked for; the client's own `stream` flag is
                    // unaffected.
                    if !upstream_streaming && is_non_stream_unsupported(&body) {
                        tracing::warn!(
                            "Upstream rejected non-streaming body ({}); retrying as a stream",
                            status
                        );
                        gui_logs
                            .push(
                                "WARN",
                                format!(
                                    "非流式被上游拒绝(11101) → 改用流式请求重试 (model={}) {}",
                                    openai_req.model, tag
                                ),
                            )
                            .await;
                        if let Ok(mut t) = overrides.lock() {
                            t.set_model(&openai_req.model, "11101 stream upgrade");
                        }
                        return retry_as_stream(
                            &config,
                            &client,
                            &openai_req,
                            &current_api_key,
                            current_credential.as_ref(),
                            &gui_logs,
                            &client_model,
                            &stats,
                            flavor,
                            route,
                            start,
                            &session,
                            &tag,
                            url,
                            &overrides,
                        )
                        .await;
                    }

                    // After the degraded retry still fails, report a content_blocked
                    // error the client can understand rather than a generic upstream
                    // one. Checked before `body` is consumed below, so a rejected
                    // body is never copied just to build the error.
                    if is_content_blocked(status.as_u16(), &body) {
                        return Err(ProxyError::Upstream(
                        "Upstream rejected the request as content policy violation (code 11-128). \
                         The degraded retry also failed."
                            .to_string(),
                    ));
                    }
                    // Terminal rejections (400/413/422) keep the upstream's own
                    // status so the client can act on them; anything else stays a
                    // 502 gateway fault. See `ProxyError::from_upstream` — a
                    // context-overflow 400 reported as 502 made Codex retry the
                    // identical prompt 37 times instead of compacting.
                    let err = ProxyError::from_upstream(status.as_u16(), body);
                    if is_retriable_status(status.as_u16()) {
                        last_err = Some(err);
                        continue 'url; // try next upstream URL
                    }
                    return Err(err);
                }

                // The identity serving this request succeeded, so it is
                // healthy. If it was the one parked in failover memory, the
                // half-open probe just passed: forget the memory so later
                // requests return to the default-first path instead of staying
                // pinned to the override.
                if let Some(cred) = current_credential.as_ref() {
                    if failover_is_known(&cred.id) {
                        clear_failover();
                        tracing::info!(
                            "credential {} served a request successfully; leaving failover memory",
                            cred.id
                        );
                    }
                }

                return if streaming {
                    streaming_response(
                        response,
                        flavor,
                        &openai_req.model,
                        client_model,
                        route,
                        start,
                        upstream_start,
                        gui_logs,
                        stats,
                        session,
                        tag,
                        Arc::clone(&overrides),
                    )
                } else {
                    // The client wants one JSON body. If the request was upgraded
                    // to a stream upstream, aggregate it first — that path sees
                    // every chunk, so it is also the one that can report TTFT and
                    // the model's generation span. A plain JSON response exposes
                    // neither, so its timings stay at zero ("unknown").
                    //
                    // Reading the body happens *here* rather than through `?`
                    // because it can fail mid-transfer: reqwest reports a
                    // truncated body as `Decode`/“error decoding response body”,
                    // which the `?` turned into a 502/500 the client could not
                    // act on even though the next attempt normally succeeds.
                    let materialized = if upstream_streaming {
                        collect_stream_into_response(response, upstream_start).await
                    } else {
                        response
                            .json::<openai::OpenAIResponse>()
                            .await
                            .map(|resp| (resp, UpstreamTiming::default()))
                            .map_err(ProxyError::Http)
                    };

                    let (resp, timing) = match materialized {
                        Ok(pair) => pair,
                        Err(err) => {
                            // A body cut short mid-transfer is the upstream or
                            // the network, not this request — retry on the next
                            // URL (and, if none is left, the next credential)
                            // instead of failing the client outright. A body
                            // that arrived whole but could not be deserialized
                            // will fail identically everywhere, so it is
                            // reported rather than retried.
                            if !err.is_transient_transport_error() {
                                return Err(err);
                            }
                            let message = err.diagnostic();
                            metrics::upstream_error("chat_completions");
                            gui_logs
                                .push(
                                    "WARN",
                                    format!(
                                        "UPSTREAM BODY FAILED MID-TRANSFER url={} model={} {}ms {} | {} (retrying next upstream)",
                                        url,
                                        openai_req.model,
                                        upstream_start.elapsed().as_millis(),
                                        tag,
                                        message
                                    ),
                                )
                                .await;
                            tracing::warn!("Upstream body failed mid-transfer: {}", message);
                            last_err = Some(err);
                            continue 'url; // try the next upstream URL
                        }
                    };

                    non_streaming_response(
                        resp,
                        flavor,
                        &openai_req.model,
                        client_model,
                        route,
                        start,
                        timing,
                        &config,
                        stats,
                        &session,
                        &tag,
                        &overrides,
                    )
                    .await
                };
            }
        }

        // The credential's attempts were exhausted without an exception that
        // warrants a switch (e.g. a plain upstream error): stop, so a 5xx
        // storm does not burn the whole pool.
        if credential_attempt + 1 < max_credential_attempts {
            // A 4xx limit exception already failed over inside the loop (see
            // the 429/402 handling); reaching here means non-switch failures.
            break 'credential;
        }
    }

    Err(last_err.unwrap_or_else(|| ProxyError::Upstream("All upstreams failed".to_string())))
}
