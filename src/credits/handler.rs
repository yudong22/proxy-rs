//! The `/v1/credits` HTTP handler.

use super::*;

/// axum handler for `GET /v1/credits`.
pub async fn credits_handler(
    Extension(config): Extension<Arc<crate::config::Config>>,
    Extension(client): Extension<reqwest::Client>,
    Extension(gui_logs): Extension<Arc<LogBuffer>>,
    Extension(service): Extension<Arc<crate::service::ServiceController>>,
    Extension(pool): Extension<crate::session_pool::SharedCredentialPool>,
    headers: HeaderMap,
) -> ProxyResult<Response> {
    // Route through the shared redactor: this line is mirrored to
    // `~/.proxy-rs/logs/proxy.log`, and callers reach `/v1/credits` with their
    // own `authorization` / `x-api-key` header attached. Hand-rolling the
    // header loop here wrote those values out in the clear.
    let incoming = crate::util::format_headers(&headers);

    gui_logs
        .push("INFO", format!("GET /v1/credits headers: {}", incoming))
        .await;
    tracing::info!("GET /v1/credits headers: {}", incoming);

    if !service.is_running() {
        return Ok(crate::service::service_unavailable_response());
    }

    // Credential pool first: when the WorkBuddy login-state pool is active the
    // balance is queried with the default credential's access token — the
    // account whose quota the requests actually burn. Falls back to the static
    // key path when the pool is off.
    //
    // The identity travels with the value: a login token authenticates as a bare
    // bearer, while a static gateway key needs the `Bearer` + `X-API-Key` pair.
    // Sending a login token in `X-API-Key` is a 401, which used to surface as the
    // 剩余积分 card showing "—" whenever the account pool was on.
    let (api_key, identity) = if config.credential_pool_enabled && pool.is_enabled().await {
        match pool.pick("").await {
            Some(cred) => (Some(cred.access_token), CreditsIdentity::LoginState),
            None => (config.api_key.clone(), CreditsIdentity::ApiKey),
        }
    } else {
        (config.api_key.clone(), CreditsIdentity::ApiKey)
    };
    let result = fetch_credits(
        &config.credits_endpoint,
        config.models_flavor.clone(),
        &client,
        &api_key,
        identity,
    )
    .await;

    match result {
        Ok(resp) => {
            if let Some(ref err) = resp.credit_error {
                gui_logs
                    .push("WARN", format!("GET /v1/credits unavailable: {}", err))
                    .await;
            } else {
                gui_logs
                    .push(
                        "INFO",
                        format!(
                            "GET /v1/credits ok credits={} segments={}",
                            resp.credits,
                            resp.segments.len()
                        ),
                    )
                    .await;
            }
            Ok(Json(resp).into_response())
        }
        Err(e) => {
            gui_logs
                .push("ERROR", format!("GET /v1/credits failed | {}", e))
                .await;
            Err(e)
        }
    }
}
