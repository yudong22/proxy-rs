//! Diagnosing, describing and repairing upstream failures.
//!
//! Split out of `proxy.rs`: the "what does this upstream error mean, and can we
//! retry it differently" logic. Deliberately free of credential-selection concerns —
//! that belongs to `crate::proxy::failover`.

use super::*;

/// Neutral, content-free system prompt used for the single degraded retry after
/// an upstream content-policy block. Deliberately minimal so it introduces no
/// fingerprint of its own. Mirrors `prompt.Degraded` in the Go upstream.
pub(crate) const DEGRADED_SYSTEM_PROMPT: &str =
    "You are a helpful assistant. Respond in the user's language, follow the user's instructions, and be direct and concise.";

/// Detect an upstream content-policy block. The WorkBuddy/CodeBuddy gateway
/// reports it as HTTP 400 with business code `11128` ("Illegal API invocation
/// from an unapproved channel"). We treat that specific shape as a content block
/// rather than a generic upstream failure, so the caller can attempt the
/// degraded retry instead of spinning on a bare 4xx.
pub(crate) fn is_content_blocked(status: u16, body: &str) -> bool {
    if status != 400 {
        return false;
    }
    body.contains("11128") || body.contains("unapproved channel")
}

/// Re-send a request as a stream and aggregate it into one non-streaming
/// response, for upstreams that reject plain bodies (`11101`).
///
/// Used as a fallback when the provider was not known to require streaming up
/// front. The client still receives a single JSON body, since it never asked
/// for a stream.
#[allow(clippy::too_many_arguments)]
pub(super) async fn retry_as_stream(
    config: &Config,
    client: &Client,
    openai_req: &openai::OpenAIRequest,
    api_key: &Option<String>,
    credential: Option<&crate::workbuddy_auth::WorkBuddyCredential>,
    gui_logs: &Arc<crate::settings::LogBuffer>,
    client_model: &str,
    stats: &Arc<StatsDb>,
    flavor: ApiFlavor,
    route: &'static str,
    start: Instant,
    session: &SessionInfo,
    tag: &str,
    url: &str,
    overrides: &Arc<std::sync::Mutex<OverrideTrace>>,
) -> ProxyResult<Response> {
    let mut streamed_req = openai_req.clone();
    streamed_req.stream = Some(true);
    streamed_req.stream_options = Some(openai::StreamOptions {
        include_usage: true,
    });

    let builder = client.post(url).json(&streamed_req);
    // Always a streamed response here, so no total timeout: it would kill any
    // stream outliving it. The shared client's idle read_timeout covers stalls
    // — see forward_request for the rationale.
    //
    // Timed from this send: the first attempt was rejected outright (11101), so
    // its round-trip is not the model's latency.
    let retry_start = Instant::now();
    let response = apply_upstream_auth(builder, config, api_key, credential)
        .send()
        .await
        .map_err(ProxyError::Http)?;

    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        return Err(ProxyError::Upstream(format!(
            "Upstream returned {} (also as a stream): {}",
            status, body
        )));
    }

    let (resp, timing) = collect_stream_into_response(response, retry_start).await?;
    gui_logs
        .push(
            "INFO",
            format!(
                "流式重试成功，已聚合为单次响应 (model={}) {}",
                client_model, tag
            ),
        )
        .await;
    non_streaming_response(
        resp,
        flavor,
        &streamed_req.model,
        client_model.to_string(),
        route,
        start,
        timing,
        config,
        stats.clone(),
        session,
        tag,
        overrides,
    )
    .await
}

/// Detect "this upstream only serves streaming bodies".
///
/// Reported as HTTP 400 with business code `11101` and the message
/// `Non-stream chat request is currently not supported`. Rather than surfacing
/// a 502 to the client, the caller re-sends the same request with
/// `stream:true` and aggregates it back into one body.
pub(crate) fn is_non_stream_unsupported(body: &str) -> bool {
    body.contains("11101") && body.contains("Non-stream")
}

/// Replace the leading `system` message(s) with the neutral degraded prompt.
/// Other (user/assistant/tool) messages are preserved so the user's actual
/// request still gets answered — we are only washing out the blocked system
/// template, which is exactly what the content filter object to.
pub(crate) fn apply_degraded_prompt(req: &mut openai::OpenAIRequest) {
    let mut replaced = false;
    for msg in req.messages.iter_mut() {
        if msg.role == "system" && !replaced {
            msg.content = Some(openai::MessageContent::Text(
                DEGRADED_SYSTEM_PROMPT.to_string(),
            ));
            msg.reasoning_content = None;
            msg.tool_calls = None;
            replaced = true;
        }
    }
    // If there was no system message at all, prepend one.
    if !replaced {
        req.messages.insert(
            0,
            openai::Message {
                role: "system".to_string(),
                content: Some(openai::MessageContent::Text(
                    DEGRADED_SYSTEM_PROMPT.to_string(),
                )),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
        );
    }
}

/// Parse a vendor error envelope into a compact, human-readable summary.
///
/// WorkBuddy/CodeBuddy report failures as
/// `{"code":11128,"msg":"...","requestId":"...","displayMsg":{"zh":"...","en":"..."}}`.
/// We surface the numeric `code` plus the message so the GUI log is actionable
/// instead of an opaque "502".
pub(crate) fn describe_upstream_error(status: u16, body: &str) -> String {
    let trimmed = body.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(obj) = value.as_object() {
            // WorkBuddy style: {"code":..., "msg":...}
            if let Some(code) = obj.get("code") {
                let code_str = match code {
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::String(s) => s.clone(),
                    _ => code.to_string(),
                };
                let msg = obj
                    .get("msg")
                    .or_else(|| obj.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let request_id = obj
                    .get("requestId")
                    .or_else(|| obj.get("request_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let display = obj
                    .get("displayMsg")
                    .and_then(|d| d.get("zh").or_else(|| d.get("en")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let mut text = format!("HTTP {} · code {} {}", status, code_str, msg);
                if !display.is_empty() && display != msg {
                    text.push_str(&format!(" · {}", display));
                }
                if !request_id.is_empty() {
                    text.push_str(&format!(" · requestId {}", request_id));
                }
                return text;
            }
            // Anthropic-style: {"error":{"message":...}}
            if let Some(err) = obj.get("error").and_then(|e| e.as_object()) {
                let m = err.get("message").and_then(|v| v.as_str()).unwrap_or("");
                if !m.is_empty() {
                    return format!("HTTP {} · {}", status, truncate(m, 400));
                }
            }
        }
    }
    format!("HTTP {} · {}", status, truncate(trimmed, 400))
}

/// A friendlier one-line hint for known WorkBuddy business codes.
pub(crate) fn upstream_code_hint(body: &str) -> Option<&'static str> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let code = value.get("code")?.as_i64()?;
    Some(match code {
        11101 => "上游不支持非流式请求或参数解析失败，可强制 stream=true",
        11102 => "模型名不在上游可用列表（注意 [1M] 等后缀；在控制台“模型重定向”里加 请求模型:可用模型 映射，保存即生效）",
        11103 => "后端不支持该模型（如图像模型）",
        11128 => "非法 API 调用：请求形态被上游拒绝",
        11129 => "function 参数非法",
        11133 => "请求被模型提供方拒绝（参数/权限）",
        11135 => "图片数据无效",
        11148 => "tool_calls 与 tool_result 不匹配",
        11151 => "存在空内容消息",
        11152 => "工具名称不符合规范或存在重复名称（只允许字母、数字、下划线，最多64字符且不可重名）",
        _ => return None,
    })
}

/// Push a structured upstream failure into the GUI log buffer (and tracing).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn log_upstream_failure(
    gui_logs: &Arc<crate::settings::LogBuffer>,
    stage: &str,
    url: &str,
    model: &str,
    status: u16,
    body: &str,
    elapsed_ms: u128,
    tag: &str,
) {
    let summary = describe_upstream_error(status, body);
    let mut msg = format!(
        "UPSTREAM ERROR [{}] url={} model={} {}ms {} | {}",
        stage, url, model, elapsed_ms, tag, summary
    );
    if let Some(hint) = upstream_code_hint(body) {
        msg.push_str(&format!(" | hint: {}", hint));
    }
    msg.push_str(&format!(" | body: {}", truncate(body, 800)));
    gui_logs.push("ERROR", msg.clone()).await;
    tracing::warn!("Upstream failure: {}", msg);
}

/// Compact skeleton of the outbound message list, for diagnosing upstream
/// rejections that depend on message *shape* rather than content.
///
/// A 11148 ("tool calls and tool results do not match") is unreadable without
/// knowing which calls had results and in what order, so log the roles, the
/// tool-call ids and the result ids — never the message bodies (too large, and
/// they may hold user content).
pub(crate) fn describe_message_shape(messages: &[crate::models::openai::Message]) -> String {
    let parts: Vec<String> = messages
        .iter()
        .map(|m| {
            let calls: Vec<String> = m
                .tool_calls
                .as_ref()
                .map(|v| {
                    v.iter()
                        .map(|t| format!("{}:{}", t.id, t.function.name))
                        .collect()
                })
                .unwrap_or_default();
            match m.role.as_str() {
                "tool" => format!("tool({})", m.tool_call_id.as_deref().unwrap_or("<no-id>")),
                "assistant" if !calls.is_empty() => format!("assistant[{}]", calls.join(",")),
                other => other.to_string(),
            }
        })
        .collect();
    format!("{} msgs: {}", messages.len(), parts.join(" > "))
}
