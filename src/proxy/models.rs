//! `GET /v1/models` and its config-driven variants.

use super::*;

pub async fn list_models_handler(
    Extension(config): Extension<Arc<Config>>,
    Extension(client): Extension<Client>,
    Extension(gui_logs): Extension<Arc<crate::settings::LogBuffer>>,
    headers: HeaderMap,
) -> ProxyResult<Response> {
    let api_key = resolve_api_key(&config, &headers);

    // No body on this route, so header sources are the only ones available.
    let session = resolve_session(&headers, None);
    let tag = session.log_tag();

    let incoming_headers = format_headers(&headers);
    gui_logs
        .push(
            "INFO",
            format!("GET /v1/models {} headers: {}", tag, incoming_headers),
        )
        .await;
    tracing::info!("GET /v1/models {} headers: {}", tag, incoming_headers);

    // Vendors like WorkBuddy publish their catalog on a custom config
    // endpoint instead of the OpenAI `/v1/models` route; honour it.
    if config.models_flavor == ModelsFlavor::WorkBuddyConfig {
        return list_models_via_config(&config, &client, &api_key, &gui_logs, &tag).await;
    }

    let urls = config.models_urls();
    let mut last_err = None;

    for url in &urls {
        tracing::debug!("Fetching models from {}", url);

        let mut req_builder = client.get(url).timeout(Duration::from_secs(60));
        if let Some(ref key) = api_key {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", key));
        }

        match req_builder.send().await {
            Ok(response) if response.status().is_success() => {
                let openai_resp: openai::ModelsListResponse = response.json().await?;
                let anthropic_resp = pipeline::translate_models_list(openai_resp);
                return Ok(Json(anthropic_resp).into_response());
            }
            Ok(response) => {
                let status = response.status();
                let error_text = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string());
                tracing::warn!("Upstream {} returned {}: {}", url, status, error_text);
                if is_retriable_status(status.as_u16()) {
                    last_err = Some(format!("Upstream returned {}: {}", status, error_text));
                    continue;
                }
                return Err(ProxyError::from_upstream(status.as_u16(), error_text));
            }
            Err(err) => {
                tracing::warn!("Failed to reach {}: {:?}", url, err);
                last_err = Some(format!("HTTP error: {}", err));
                continue;
            }
        }
    }

    Err(ProxyError::Upstream(
        last_err.unwrap_or_else(|| "All upstreams failed".to_string()),
    ))
}

/// Serve `/v1/models` from a vendor config endpoint (WorkBuddy `/v3/config`).
///
/// The vendor catalog is filtered to CLI-authorized models and then shaped
/// like an OpenAI list so the existing translation applies unchanged.
async fn list_models_via_config(
    config: &Config,
    client: &Client,
    api_key: &Option<String>,
    gui_logs: &Arc<crate::settings::LogBuffer>,
    tag: &str,
) -> ProxyResult<Response> {
    let Some(url) = config.models_config_url.clone() else {
        return Err(ProxyError::Upstream(
            "provider has no models config endpoint".to_string(),
        ));
    };
    let Some(key) = api_key.clone() else {
        return Err(ProxyError::Upstream(
            "API key required to list models".to_string(),
        ));
    };

    let preset = crate::providers::ProviderPreset {
        id: "vendor".to_string(),
        name: "vendor".to_string(),
        chat_completions_url: String::new(),
        models_url: None,
        models_config_url: Some(url),
        config_headers: Default::default(),
        force_stream: false,
    };

    let models = crate::providers::fetch_models(client, &preset, &key)
        .await
        .map_err(|e| ProxyError::Upstream(e.to_string()))?;

    let mut data: Vec<_> = models
        .into_iter()
        .map(|m| openai::ModelInfo {
            id: m.id,
            object: Some("model".to_string()),
            created: None,
            owned_by: None,
        })
        .collect();

    // Advertise the virtual `free` model so clients can pin to it. It is not a
    // real upstream id (the gateway resolves it per request), so expose it only
    // once and only when the catalog would not already contain it.
    if !data.iter().any(|m| m.id == pipeline::FREE_MODEL_NAME) {
        data.push(openai::ModelInfo {
            id: pipeline::FREE_MODEL_NAME.to_string(),
            object: Some("model".to_string()),
            created: None,
            owned_by: Some("proxy-free".to_string()),
        });
    }

    let openai_resp = openai::ModelsListResponse {
        object: Some("list".to_string()),
        data,
    };

    gui_logs
        .push(
            "INFO",
            format!(
                "GET /v1/models (vendor config) -> {} models {}",
                openai_resp.data.len(),
                tag
            ),
        )
        .await;

    Ok(Json(pipeline::translate_models_list(openai_resp)).into_response())
}

pub(crate) fn translation_policy(config: &Config) -> pipeline::TranslationPolicy {
    pipeline::TranslationPolicy {
        reasoning_model: config.reasoning_model.clone(),
        completion_model: config.completion_model.clone(),
        model_map: config.model_map.clone(),
        ignore_terms: config.system_prompt_ignore_terms.clone(),
        strip_model_suffix: config.models_flavor == ModelsFlavor::WorkBuddyConfig,
        sanitize_fingerprints: config.sanitize_fingerprints,
    }
}
