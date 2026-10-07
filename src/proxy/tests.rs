//! `proxy` unit tests, kept in their own file so the production modules stay
//! readable.
//!
//! This file *is* the test module (`proxy/tests.rs`), so it must not wrap its
//! contents in another `mod tests { … }` — doing so would put everything one level
//! deeper and make the `super::` imports resolve to `proxy::tests` instead of
//! `crate::proxy`, which is where the items under test live.

use super::apply_upstream_auth;
use super::create_sse_stream;
use super::json_request;
use super::TimingTracker;
use super::{
    apply_degraded_prompt, is_content_blocked, is_non_stream_unsupported, upstream_auth_headers,
    OverrideTrace, MAX_SSE_FRAME_BYTES,
};
use super::{
    clear_failover, failover_is_known, failover_memory, remember_failover, FailoverTarget,
};
use crate::models::{openai, responses};
use crate::session::SessionInfo;
use crate::stats::UpstreamTiming;
use axum::response::IntoResponse;
use bytes::Bytes;
use futures::stream::{self, StreamExt};
use reqwest::Client;
use serde_json::{json, Value};
use std::fmt;
use std::time::Instant;

/// The failover memory: remembered per failing credential, scoped, and
/// expiring.
///
/// One test rather than three, on purpose: the memory is a process-wide
/// static and `cargo test` runs tests in parallel threads, so separate
/// tests would race on it and fail intermittently.
#[test]
fn failover_memory_is_scoped_and_expires() {
    // Clean slate regardless of what ran before.
    failover_memory().write().ok().and_then(|mut g| g.take());

    remember_failover("wb-bad", FailoverTarget::ApiKey("k-good".to_string()));
    assert!(
        failover_is_known("wb-bad"),
        "the failing credential must be recognised"
    );
    // A different credential is not implicated by someone else's failure.
    assert!(
        !failover_is_known("wb-other"),
        "memory must be scoped to the credential that failed"
    );

    // Wind the clock past the TTL: a transient blip must not pin traffic
    // away from the configured default forever.
    if let Ok(mut guard) = failover_memory().write() {
        if let Some(m) = guard.as_mut() {
            m.expires_at_ms = crate::util::unix_millis() - 1;
        }
    }
    assert!(
        !failover_is_known("wb-bad"),
        "an expired memory must not be consulted"
    );

    // Half-open recovery: a successful request on the parked identity
    // clears the memory, so later requests return to the default-first
    // path and a recovered default stops being overridden.
    remember_failover("wb-bad", FailoverTarget::ApiKey("k-good".to_string()));
    assert!(failover_is_known("wb-bad"));
    clear_failover();
    assert!(
        !failover_is_known("wb-bad"),
        "the identity must be probed again after it proved healthy"
    );

    // Leave it clean for any test that runs after.
    failover_memory().write().ok().and_then(|mut g| g.take());
}

/// A credential override and a model override must both survive: they used
/// to share one `reason` field, so the second `set_*` call erased the first
/// reason and the log showed only half of a two-part override.
#[test]
fn override_trace_keeps_both_reasons() {
    let mut t = OverrideTrace::default();
    t.set_key("wb-b1", "429 from wb-a1");
    t.set_model("glm-5.3", "content_blocked 400");

    assert_eq!(t.key, "wb-b1");
    assert_eq!(t.model, "glm-5.3");
    let reason = t.reason();
    assert!(
        reason.contains("429 from wb-a1") && reason.contains("content_blocked 400"),
        "both reasons must be reported, got: {reason}"
    );
    let suffix = t.log_suffix();
    assert!(suffix.contains("wb-b1@glm-5.3"), "got: {suffix}");
    assert!(suffix.contains("429 from wb-a1"), "got: {suffix}");
}

/// A clean request leaves both the columns and the log suffix empty.
#[test]
fn override_trace_is_empty_when_nothing_was_overridden() {
    let t = OverrideTrace::default();
    assert!(t.log_suffix().is_empty());
    assert!(t.reason().is_empty());
}

#[test]
fn content_blocked_detects_11128() {
    assert!(is_content_blocked(
        400,
        "{\"code\":11128,\"msg\":\"Illegal API invocation from an unapproved channel\"}"
    ));
    // Non-400 is never a content block.
    assert!(!is_content_blocked(502, "11128"));
    // 400 without the block signature is a generic upstream failure.
    assert!(!is_content_blocked(400, "{\"error\":\"bad request\"}"));
}

/// A config in the WorkBuddy flavor, which is the one that mirrors the
/// `x-api-key`/`Authorization` pair.
fn workbuddy_config() -> crate::config::Config {
    crate::config::Config {
        models_flavor: crate::config::ModelsFlavor::WorkBuddyConfig,
        ..Default::default()
    }
}

/// A login-state credential with a token distinct from any static key.
fn test_credential() -> crate::workbuddy_auth::WorkBuddyCredential {
    crate::workbuddy_auth::WorkBuddyCredential {
        id: "wb-abc123".to_string(),
        access_token: "credential-token".to_string(),
        machine_id: "machine-1".to_string(),
        account: crate::workbuddy_auth::WorkBuddyAccount {
            uid: "uid-1".to_string(),
            enterprise_id: "ent-1".to_string(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A static key and a credential must never both authenticate one request.
/// reqwest's `.header()` appends, so a naive implementation emits two
/// `Authorization` values and Tencent's `stgw` gateway answers a bare HTML
/// 400. Regression for the "都是 400 / Bad Request" report.
///
/// The credential path must also NOT mirror its token into `x-api-key`: a
/// login token is not an API key, and the gateway answers
/// `401 {"message":"not_found"}` when one is sent — which is what silently
/// pushed every account-pool request onto the key pool. Verified against the
/// live upstream (2026-09-28): `Bearer <token>` → 200,
/// `Bearer <token>` + `x-api-key: <token>` → 401 not_found.
#[test]
fn credential_auth_replaces_the_static_key_on_every_shared_header() {
    let config = workbuddy_config();
    let key = Some("static-key".to_string());
    let cred = test_credential();

    let headers = upstream_auth_headers(&config, &key, Some(&cred));

    // Exactly one Authorization, and it is the credential's bearer.
    assert_eq!(
        headers.get_all("authorization").iter().count(),
        1,
        "must be a single Authorization header"
    );
    assert_eq!(
        headers.get("authorization").unwrap(),
        "Bearer credential-token"
    );
    // The account token must not be mirrored as an api key.
    assert!(
        headers.get("x-api-key").is_none(),
        "a login-state credential must not set x-api-key; got {:?}",
        headers.get("x-api-key")
    );
    // No trace of the static key anywhere.
    assert!(
        !headers
            .values()
            .any(|v| v.to_str().unwrap_or_default().contains("static-key")),
        "the static key must not survive alongside a credential"
    );
    // The session fingerprint is still present.
    assert_eq!(headers.get("x-user-id").unwrap(), "uid-1");
    assert_eq!(headers.get("x-enterprise-id").unwrap(), "ent-1");
    assert_eq!(headers.get("x-machine-id").unwrap(), "machine-1");
}

/// Without a credential the static key keeps its 1.7.x behaviour.
#[test]
fn static_key_path_still_sends_authorization_and_x_api_key() {
    let config = workbuddy_config();
    let key = Some("static-key".to_string());

    let headers = upstream_auth_headers(&config, &key, None);

    assert_eq!(headers.get("authorization").unwrap(), "Bearer static-key");
    assert_eq!(headers.get("x-api-key").unwrap(), "static-key");
    assert_eq!(headers.get_all("authorization").iter().count(), 1);
    assert_eq!(headers.get_all("x-api-key").iter().count(), 1);
}

/// The same contract, asserted on the *wired* request rather than the
/// intermediate map.
///
/// `apply_upstream_auth` is what the send path actually calls, and a helper
/// that builds the right map means nothing if the caller adds a header of
/// its own afterwards. Building the real `reqwest::Request` catches that:
/// the account path must put the token in `Authorization` only, the static
/// path must keep both headers, and neither may ever carry two values for
/// one name (reqwest `.header()` appends — the `stgw` 400 regression).
#[test]
fn wired_upstream_request_never_leaks_a_mirrored_account_token() {
    let config = workbuddy_config();
    let client = Client::new();
    let key = Some("static-key".to_string());
    let cred = test_credential();
    let body = openai::OpenAIRequest {
        model: "glm-5.3-flash".to_string(),
        messages: vec![],
        ..Default::default()
    };

    let account_req = apply_upstream_auth(
        client
            .post("https://upstream.invalid/v2/chat/completions")
            .json(&body),
        &config,
        &key,
        Some(&cred),
    )
    .build()
    .expect("account request builds");

    let headers = account_req.headers();
    assert_eq!(
        headers.get("authorization").unwrap(),
        "Bearer credential-token",
        "the account bearer must travel in Authorization"
    );
    assert!(
        headers.get("x-api-key").is_none(),
        "the account token must not be mirrored into x-api-key, got {:?}",
        headers.get("x-api-key")
    );
    assert_eq!(headers.get_all("authorization").iter().count(), 1);
    assert_eq!(headers.get_all("x-api-key").iter().count(), 0);
    // The fingerprint still rides along.
    assert_eq!(headers.get("x-user-id").unwrap(), "uid-1");

    let key_req = apply_upstream_auth(
        client
            .post("https://upstream.invalid/v2/chat/completions")
            .json(&body),
        &config,
        &key,
        None,
    )
    .build()
    .expect("static-key request builds");

    let headers = key_req.headers();
    assert_eq!(headers.get("authorization").unwrap(), "Bearer static-key");
    assert_eq!(headers.get("x-api-key").unwrap(), "static-key");
    assert_eq!(headers.get_all("authorization").iter().count(), 1);
    assert_eq!(headers.get_all("x-api-key").iter().count(), 1);
}

/// A non-WorkBuddy provider keeps a single `Authorization` and never gains
/// the WorkBuddy `x-api-key`.
#[test]
fn openai_flavor_uses_authorization_only() {
    let config = crate::config::Config {
        models_flavor: crate::config::ModelsFlavor::OpenAI,
        ..Default::default()
    };
    let key = Some("provider-key".to_string());

    let headers = upstream_auth_headers(&config, &key, None);

    assert_eq!(headers.get("authorization").unwrap(), "Bearer provider-key");
    assert!(headers.get("x-api-key").is_none());
}

/// TTFT is the first *output* delta — text or tool call — and the model span
/// is the last one. Reasoning deltas and metadata-only frames must not count,
/// or a turn would report a first token the client never received.
#[test]
fn timing_tracker_measures_first_output_and_last_delta() {
    let mut tracker = TimingTracker::default();
    let start = Instant::now();

    // A role-only preamble is not output.
    tracker.observe(start, &chunk_with(json!({"delta": {"role": "assistant"}})));
    assert_eq!(tracker.ttft_ms, None, "a role frame is not a first token");

    // Nor is a reasoning-only delta: the Anthropic translator never surfaces
    // upstream reasoning, so counting it would lie about TTFT.
    tracker.observe(
        start,
        &chunk_with(json!({"delta": {"reasoning_content": "thinking…"}})),
    );
    assert_eq!(
        tracker.ttft_ms, None,
        "reasoning is not client-visible output"
    );

    // First real content fixes TTFT.
    tracker.observe(start, &chunk_with(json!({"delta": {"content": "hi"}})));
    let ttft = tracker.ttft_ms.expect("first content sets TTFT");
    assert!(ttft >= 0);

    // A later non-empty delta extends the span but must not move TTFT.
    tracker.observe(start, &chunk_with(json!({"delta": {"content": " there"}})));
    assert_eq!(tracker.ttft_ms, Some(ttft), "TTFT is fixed by the first");

    let timing = tracker.finish();
    assert_eq!(timing.ttft_ms, ttft);
    assert!(timing.model_ms >= timing.ttft_ms);
    assert!(!timing.ended_with_tool_call);
}

/// A turn that opens with a tool call still has a first output token, and
/// `finish_reason: tool_calls` marks it as a tool turn.
#[test]
fn timing_tracker_treats_a_tool_call_as_output() {
    let mut tracker = TimingTracker::default();
    let start = Instant::now();

    tracker.observe(
        start,
        &chunk_with(json!({
            "delta": {"tool_calls": [{"index": 0, "id": "call_1",
                      "function": {"name": "read_file", "arguments": "{}"}}]}
        })),
    );
    assert!(
        tracker.ttft_ms.is_some(),
        "a tool-only turn still produces a first token"
    );

    tracker.observe(
        start,
        &chunk_with(json!({"delta": {}, "finish_reason": "tool_calls"})),
    );
    assert!(tracker.finish().ended_with_tool_call, "tool_calls finish");
}

/// An unobserved stream reports zeros, which the UI renders as "unknown"
/// rather than as a fabricated speed.
#[test]
fn timing_tracker_reports_zero_when_nothing_was_observed() {
    let tracker = TimingTracker::default();
    let timing = tracker.finish();
    assert_eq!(timing.ttft_ms, 0);
    assert_eq!(timing.model_ms, 0);
    assert!(!timing.ended_with_tool_call);
    assert_eq!(timing.tps(1000), None, "no span means no speed");
}

/// Build a `StreamChunk` from a partial choice body (`delta`/`finish_reason`).
fn chunk_with(choice: Value) -> openai::StreamChunk {
    let mut choice = choice;
    if choice.get("index").is_none() {
        choice["index"] = json!(0);
    }
    serde_json::from_value(json!({
        "id": "cmb-1",
        "model": "glm",
        "choices": [choice]
    }))
    .expect("test chunk parses")
}

#[test]
fn non_stream_unsupported_detects_11101() {
    assert!(is_non_stream_unsupported(
        "{\"code\":11101,\"msg\":\"Non-stream chat request is currently not supported\"}"
    ));
    // 11101 also covers parameter parse failures, which must NOT be
    // retried as a stream — only the non-stream refusal carries the
    // "Non-stream" marker.
    assert!(!is_non_stream_unsupported(
        "{\"code\":11101,\"msg\":\"invalid parameter\"}"
    ));
    assert!(!is_non_stream_unsupported(
        "{\"code\":11148,\"msg\":\"tool calls and tool results do not match\"}"
    ));
}

#[test]
fn degraded_prompt_replaces_system_message() {
    let mut req = openai::OpenAIRequest {
        model: "m".to_string(),
        messages: vec![
            openai::Message {
                role: "system".to_string(),
                content: Some(openai::MessageContent::Text(
                    "You are Claude Code, Anthropic's official CLI for Claude".to_string(),
                )),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            openai::Message {
                role: "user".to_string(),
                content: Some(openai::MessageContent::Text(
                    "what is the cache key?".to_string(),
                )),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
        ],
        max_tokens: Some(1),
        max_completion_tokens: None,
        temperature: None,
        top_p: None,
        stop: None,
        stream: Some(false),
        stream_options: None,
        tools: None,
        tool_choice: None,
        extra: serde_json::Map::new(),
    };
    apply_degraded_prompt(&mut req);
    assert_eq!(req.messages.len(), 2);
    assert_eq!(req.messages[0].role, "system");
    let sys = match req.messages[0].content.as_ref().unwrap() {
        openai::MessageContent::Text(t) => t,
        _ => panic!("expected text"),
    };
    assert!(sys.contains("helpful assistant"));
    assert!(!sys.contains("Claude Code"));
    // User message preserved.
    assert_eq!(req.messages[1].role, "user");
}

#[test]
fn degraded_prompt_prepends_when_no_system() {
    let mut req = openai::OpenAIRequest {
        model: "m".to_string(),
        messages: vec![openai::Message {
            role: "user".to_string(),
            content: Some(openai::MessageContent::Text("hi".to_string())),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }],
        max_tokens: Some(1),
        max_completion_tokens: None,
        temperature: None,
        top_p: None,
        stop: None,
        stream: Some(false),
        stream_options: None,
        tools: None,
        tool_choice: None,
        extra: serde_json::Map::new(),
    };
    apply_degraded_prompt(&mut req);
    assert_eq!(req.messages.len(), 2);
    assert_eq!(req.messages[0].role, "system");
    assert_eq!(req.messages[1].role, "user");
}

#[derive(Debug)]
struct TestError;
impl fmt::Display for TestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "test error")
    }
}
// The SSE framer takes `std::error::Error` items so it can walk a real cause
// chain (`describe_error_chain`); the fake stream error has to satisfy the same
// bound.
impl std::error::Error for TestError {}

fn openai_chunk(
    id: &str,
    model: &str,
    content: Option<&str>,
    finish_reason: Option<&str>,
) -> String {
    let mut delta = json!({});
    if let Some(c) = content {
        delta["content"] = json!(c);
    }
    let mut choice = json!({ "index": 0, "delta": delta });
    if let Some(fr) = finish_reason {
        choice["finish_reason"] = json!(fr);
    }
    let chunk = json!({
        "id": id,
        "model": model,
        "choices": [choice],
    });
    format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap())
}

fn openai_chunk_with_reasoning(id: &str, model: &str, reasoning: &str) -> String {
    let chunk = json!({
        "id": id,
        "model": model,
        "choices": [{ "index": 0, "delta": { "reasoning": reasoning } }],
    });
    format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap())
}

fn openai_chunk_with_reasoning_content(id: &str, model: &str, reasoning: &str) -> String {
    let chunk = json!({
        "id": id,
        "model": model,
        "choices": [{ "index": 0, "delta": { "reasoning_content": reasoning } }],
    });
    format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap())
}

fn openai_chunk_with_tool_call(
    id: &str,
    model: &str,
    tool_id: Option<&str>,
    name: Option<&str>,
    args: Option<&str>,
    finish_reason: Option<&str>,
) -> String {
    let mut tc = json!({ "index": 0 });
    if let Some(tid) = tool_id {
        tc["id"] = json!(tid);
        tc["type"] = json!("function");
    }
    let mut func = json!({});
    if let Some(n) = name {
        func["name"] = json!(n);
    }
    if let Some(a) = args {
        func["arguments"] = json!(a);
    }
    if !func.as_object().unwrap().is_empty() {
        tc["function"] = func;
    }
    let mut choice = json!({ "index": 0, "delta": { "tool_calls": [tc] } });
    if let Some(fr) = finish_reason {
        choice["finish_reason"] = json!(fr);
    }
    let chunk = json!({
        "id": id,
        "model": model,
        "choices": [choice],
    });
    format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap())
}

fn openai_done() -> String {
    "data: [DONE]\n\n".to_string()
}

fn make_stream(
    chunks: Vec<String>,
) -> impl futures::Stream<Item = Result<Bytes, TestError>> + Send + 'static {
    stream::iter(chunks.into_iter().map(|c| Ok(Bytes::from(c))))
}

fn mock_stats() -> std::sync::Arc<crate::stats::StatsDb> {
    // Inline mode: writes are synchronous, so tests can assert on them
    // immediately without waiting on the writer thread.
    crate::stats::StatsDb::in_memory().unwrap()
}

async fn collect_events(chunks: Vec<String>, model: &str) -> Vec<Value> {
    let s = make_stream(chunks);
    let sse = create_sse_stream(
        s,
        model.to_string(),
        std::sync::Arc::new(crate::settings::LogBuffer::new(2000)),
        mock_stats(),
    );
    tokio::pin!(sse);

    let mut events = Vec::new();
    while let Some(Ok(bytes)) = sse.next().await {
        let text = String::from_utf8_lossy(&bytes);
        for segment in text.split("\n\n").filter(|s| !s.is_empty()) {
            if let Some(data_line) = segment.lines().find(|l| l.starts_with("data: ")) {
                let json_str = data_line.strip_prefix("data: ").unwrap();
                if let Ok(v) = serde_json::from_str::<Value>(json_str) {
                    events.push(v);
                }
            }
        }
    }
    events
}

use crate::config::Config;
use axum::http::HeaderMap;

fn make_x_api_key_header(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::HeaderName::from_static("x-api-key"),
        axum::http::HeaderValue::from_str(value).unwrap(),
    );
    headers
}

#[tokio::test]
async fn resolve_api_key_passthrough_extracts_x_api_key() {
    let config = Config {
        passthrough_api_key: true,
        api_key: None,
        ..Default::default()
    };
    let headers = make_x_api_key_header("sk-my-test-key");
    let key = super::resolve_api_key(&config, &headers);
    assert_eq!(key, Some("sk-my-test-key".to_string()));
}

#[tokio::test]
async fn resolve_api_key_passthrough_ignores_empty_header() {
    let config = Config {
        passthrough_api_key: true,
        api_key: None,
        ..Default::default()
    };
    // Empty header value returns None
    let key = super::resolve_api_key(&config, &HeaderMap::new());
    assert_eq!(key, None);

    // Explicitly empty value also returns None
    let headers = make_x_api_key_header("");
    let key = super::resolve_api_key(&config, &headers);
    assert_eq!(key, None);
}

#[tokio::test]
async fn resolve_api_key_passthrough_returns_none_when_missing() {
    let config = Config {
        passthrough_api_key: true,
        api_key: None,
        ..Default::default()
    };
    let headers = HeaderMap::new();
    let key = super::resolve_api_key(&config, &headers);
    assert_eq!(key, None);
}

#[tokio::test]
async fn resolve_api_key_static_key_when_passthrough_disabled() {
    let config = Config {
        passthrough_api_key: false,
        api_key: Some("sk-upstream".to_string()),
        ..Default::default()
    };
    // Even if x-api-key is present, static key wins when passthrough is off
    let headers = make_x_api_key_header("sk-ignored");
    let key = super::resolve_api_key(&config, &headers);
    assert_eq!(key, Some("sk-upstream".to_string()));
}

#[tokio::test]
async fn resolve_api_key_both_missing_returns_none() {
    let config = Config {
        passthrough_api_key: false,
        api_key: None,
        ..Default::default()
    };
    let headers = HeaderMap::new();
    let key = super::resolve_api_key(&config, &headers);
    assert_eq!(key, None);
}

#[tokio::test]
async fn resolve_responses_api_key_extracts_bearer_auth() {
    let config = Config {
        passthrough_api_key: true,
        api_key: None,
        ..Default::default()
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        axum::http::HeaderValue::from_static("Bearer sk-bearer-test"),
    );
    let key = super::resolve_responses_api_key(&config, &headers);
    assert_eq!(key, Some("sk-bearer-test".to_string()));
}

#[test]
fn switchable_error_covers_401_402_403_429_and_quota_codes() {
    assert!(super::is_switchable_error(401, "unauthorized"));
    assert!(super::is_switchable_error(402, "payment required"));
    assert!(super::is_switchable_error(403, "forbidden"));
    assert!(super::is_switchable_error(429, "too many requests"));
    assert!(super::is_switchable_error(
        200,
        r#"{"code":11105,"message":"quota exhausted"}"#
    ));

    assert!(!super::is_switchable_error(500, "internal server error"));
    assert!(!super::is_switchable_error(400, "bad request"));
}

#[tokio::test]
async fn text_stream_produces_message_start_content_block_and_stop() {
    let chunks = vec![
        openai_chunk("chatcmpl-1", "gpt-4o", Some("Hello"), None),
        openai_chunk("chatcmpl-1", "gpt-4o", Some(" world"), None),
        openai_chunk("chatcmpl-1", "gpt-4o", None, Some("stop")),
        openai_done(),
    ];

    let events = collect_events(chunks, "fallback").await;

    assert_eq!(events[0]["type"], "message_start");
    assert_eq!(events[0]["message"]["id"], "chatcmpl-1");
    assert_eq!(events[0]["message"]["model"], "gpt-4o");
    assert_eq!(events[0]["message"]["role"], "assistant");

    assert_eq!(events[1]["type"], "content_block_start");
    assert_eq!(events[1]["content_block"]["type"], "text");

    assert_eq!(events[2]["type"], "content_block_delta");
    assert_eq!(events[2]["delta"]["type"], "text_delta");
    assert_eq!(events[2]["delta"]["text"], "Hello");

    assert_eq!(events[3]["type"], "content_block_delta");
    assert_eq!(events[3]["delta"]["text"], " world");

    assert_eq!(events[4]["type"], "content_block_stop");

    assert_eq!(events[5]["type"], "message_delta");
    assert_eq!(events[5]["delta"]["stop_reason"], "end_turn");

    assert_eq!(events[6]["type"], "message_stop");
}

#[tokio::test]
async fn reasoning_is_suppressed_text_only() {
    let chunks = vec![
        openai_chunk_with_reasoning("chatcmpl-2", "gpt-4o", "Let me think..."),
        openai_chunk_with_reasoning("chatcmpl-2", "gpt-4o", " more thinking"),
        openai_chunk("chatcmpl-2", "gpt-4o", Some("The answer is 42"), None),
        openai_chunk("chatcmpl-2", "gpt-4o", None, Some("stop")),
        openai_done(),
    ];

    let events = collect_events(chunks, "fallback").await;

    // No `thinking` content block is emitted; reasoning is suppressed.
    assert_eq!(events[0]["type"], "message_start");
    assert_eq!(events[1]["type"], "content_block_start");
    assert_eq!(events[1]["content_block"]["type"], "text");
    assert_eq!(events[1]["index"], 0);
    assert_eq!(events[2]["delta"]["type"], "text_delta");
    assert_eq!(events[2]["delta"]["text"], "The answer is 42");
    assert_eq!(events[3]["type"], "content_block_stop");
    assert_eq!(events[4]["type"], "message_delta");
    assert_eq!(events[4]["delta"]["stop_reason"], "end_turn");
    assert_eq!(events[5]["type"], "message_stop");
}

#[tokio::test]
async fn reasoning_content_is_suppressed() {
    let chunks = vec![
        openai_chunk_with_reasoning_content("chatcmpl-2", "gpt-4o", "Let me think..."),
        openai_chunk("chatcmpl-2", "gpt-4o", Some("The answer is 42"), None),
        openai_chunk("chatcmpl-2", "gpt-4o", None, Some("stop")),
        openai_done(),
    ];

    let events = collect_events(chunks, "fallback").await;

    // reasoning_content must not become a `thinking` block.
    assert_eq!(events[1]["type"], "content_block_start");
    assert_eq!(events[1]["content_block"]["type"], "text");
    assert_eq!(events[2]["delta"]["type"], "text_delta");
    assert_eq!(events[2]["delta"]["text"], "The answer is 42");
}

#[tokio::test]
async fn tool_call_stream_produces_tool_use_block() {
    let chunks = vec![
        openai_chunk_with_tool_call(
            "chatcmpl-3",
            "gpt-4o",
            Some("call_abc"),
            Some("read_file"),
            None,
            None,
        ),
        openai_chunk_with_tool_call("chatcmpl-3", "gpt-4o", None, None, Some("{\"path\":"), None),
        openai_chunk_with_tool_call("chatcmpl-3", "gpt-4o", None, None, Some("\"/tmp\"}"), None),
        openai_chunk("chatcmpl-3", "gpt-4o", None, Some("tool_calls")),
        openai_done(),
    ];

    let events = collect_events(chunks, "fallback").await;
    assert_eq!(events[1]["content_block"]["type"], "tool_use");
    assert_eq!(events[1]["content_block"]["id"], "call_abc");
    assert_eq!(events[5]["delta"]["stop_reason"], "tool_use");
}

#[tokio::test]
async fn done_without_finish_reason_still_produces_message_stop() {
    let chunks = vec![
        openai_chunk("chatcmpl-4", "gpt-4o", Some("hi"), None),
        openai_done(),
    ];
    let events = collect_events(chunks, "fallback").await;
    assert_eq!(events.last().unwrap()["type"], "message_stop");
}

#[tokio::test]
async fn fallback_model_used_when_upstream_omits_model() {
    let chunk = json!({
        "choices": [{ "index": 0, "delta": { "content": "hey" } }],
    });
    let chunks = vec![
        format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap()),
        openai_chunk("id", "gpt-4o", None, Some("stop")),
        openai_done(),
    ];
    let events = collect_events(chunks, "my-fallback-model").await;
    assert_eq!(events[0]["message"]["model"], "my-fallback-model");
}

#[tokio::test]
async fn empty_content_chunks_are_not_emitted() {
    let chunks = vec![
        openai_chunk("chatcmpl-5", "gpt-4o", Some(""), None),
        openai_chunk("chatcmpl-5", "gpt-4o", Some("hello"), None),
        openai_chunk("chatcmpl-5", "gpt-4o", None, Some("stop")),
        openai_done(),
    ];
    let events = collect_events(chunks, "fallback").await;
    let text_deltas: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta")
        .collect();
    assert_eq!(text_deltas.len(), 1);
    assert_eq!(text_deltas[0]["delta"]["text"], "hello");
}

#[tokio::test]
async fn stream_error_produces_error_event_and_stops() {
    let items: Vec<Result<Bytes, TestError>> = vec![
        Ok(Bytes::from(openai_chunk(
            "chatcmpl-6",
            "gpt-4o",
            Some("start"),
            None,
        ))),
        Err(TestError),
    ];
    let s = stream::iter(items);
    let sse = create_sse_stream(
        s,
        "fallback".to_string(),
        std::sync::Arc::new(crate::settings::LogBuffer::new(2000)),
        mock_stats(),
    );
    tokio::pin!(sse);

    let mut events = Vec::new();
    while let Some(Ok(bytes)) = sse.next().await {
        let text = String::from_utf8_lossy(&bytes);
        for segment in text.split("\n\n").filter(|s| !s.is_empty()) {
            if let Some(data_line) = segment.lines().find(|l| l.starts_with("data: ")) {
                let json_str = data_line.strip_prefix("data: ").unwrap();
                if let Ok(v) = serde_json::from_str::<Value>(json_str) {
                    events.push(v);
                }
            }
        }
    }
    let error_events: Vec<_> = events.iter().filter(|e| e["type"] == "error").collect();
    assert_eq!(error_events.len(), 1);
}

/// An upstream that never emits a blank line used to grow the reassembly
/// buffer for the whole life of the stream. Past the cap the stream now
/// stops with an error instead of accumulating — reachable from any client
/// that opts into streaming.
#[tokio::test]
async fn an_unterminated_sse_frame_stops_the_stream_with_an_error() {
    // One chunk well past the cap, with no frame separator anywhere.
    let body: Vec<u8> = "data: "
        .bytes()
        .chain(std::iter::repeat_n(b'x', MAX_SSE_FRAME_BYTES + 1))
        .collect();
    let items: Vec<Result<Bytes, TestError>> = vec![Ok(Bytes::from(body))];
    let sse = create_sse_stream(
        stream::iter(items),
        "fallback".to_string(),
        std::sync::Arc::new(crate::settings::LogBuffer::new(2000)),
        mock_stats(),
    );
    tokio::pin!(sse);

    let mut events = Vec::new();
    while let Some(Ok(bytes)) = sse.next().await {
        let text = String::from_utf8_lossy(&bytes);
        for segment in text.split("\n\n").filter(|s| !s.is_empty()) {
            if let Some(data_line) = segment.lines().find(|l| l.starts_with("data: ")) {
                let json_str = data_line.strip_prefix("data: ").unwrap();
                if let Ok(v) = serde_json::from_str::<Value>(json_str) {
                    events.push(v);
                }
            }
        }
    }

    // The client is told, rather than seeing the stream just stop.
    let error_events: Vec<_> = events.iter().filter(|e| e["type"] == "error").collect();
    assert_eq!(error_events.len(), 1, "got: {events:?}");
}

/// Below the cap nothing changes: normal frames are still delivered, so the
/// guard did not disable the streaming it protects.
#[tokio::test]
async fn a_normal_sse_frame_below_the_cap_is_still_delivered() {
    let items: Vec<Result<Bytes, TestError>> = vec![Ok(Bytes::from(openai_chunk(
        "chatcmpl-cap",
        "gpt-4o",
        Some("hi"),
        None,
    )))];
    let sse = create_sse_stream(
        stream::iter(items),
        "fallback".to_string(),
        std::sync::Arc::new(crate::settings::LogBuffer::new(2000)),
        mock_stats(),
    );
    tokio::pin!(sse);

    let mut events = Vec::new();
    while let Some(Ok(bytes)) = sse.next().await {
        let text = String::from_utf8_lossy(&bytes);
        for segment in text.split("\n\n").filter(|s| !s.is_empty()) {
            if let Some(data_line) = segment.lines().find(|l| l.starts_with("data: ")) {
                let json_str = data_line.strip_prefix("data: ").unwrap();
                if let Ok(v) = serde_json::from_str::<Value>(json_str) {
                    events.push(v);
                }
            }
        }
    }

    assert!(
        events.iter().any(|e| e["type"] == "message_start"),
        "got: {events:?}"
    );
    assert!(
        !events.iter().any(|e| e["type"] == "error"),
        "a normal frame must not error: {events:?}"
    );
}

#[tokio::test]
async fn chunked_delivery_handles_split_sse_frames() {
    let full_chunk = openai_chunk("chatcmpl-7", "gpt-4o", Some("split"), None);
    let mid = full_chunk.len() / 2;
    let part1 = full_chunk[..mid].to_string();
    let part2 = format!(
        "{}{}{}",
        &full_chunk[mid..],
        openai_chunk("chatcmpl-7", "gpt-4o", None, Some("stop")),
        openai_done()
    );
    let events = collect_events(vec![part1, part2], "fallback").await;
    let text_deltas: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta")
        .collect();
    assert_eq!(text_deltas.len(), 1);
    assert_eq!(text_deltas[0]["delta"]["text"], "split");
}

#[tokio::test]
async fn text_then_tool_call_produces_two_blocks() {
    let chunks = vec![
        openai_chunk("chatcmpl-8", "gpt-4o", Some("I'll read that file."), None),
        openai_chunk_with_tool_call(
            "chatcmpl-8",
            "gpt-4o",
            Some("call_xyz"),
            Some("read_file"),
            None,
            None,
        ),
        openai_chunk_with_tool_call(
            "chatcmpl-8",
            "gpt-4o",
            None,
            None,
            Some("{\"path\":\"/etc\"}"),
            None,
        ),
        openai_chunk("chatcmpl-8", "gpt-4o", None, Some("tool_calls")),
        openai_done(),
    ];
    let events = collect_events(chunks, "fallback").await;
    let block_starts: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "content_block_start")
        .collect();
    assert_eq!(block_starts.len(), 2);
    assert_eq!(block_starts[0]["content_block"]["type"], "text");
    assert_eq!(block_starts[1]["content_block"]["type"], "tool_use");
}

#[tokio::test]
async fn responses_sse_stream_produces_valid_events() {
    let chunks = vec![
        openai_chunk("chatcmpl-resp", "gpt-4o", Some("Hello responses"), None),
        openai_chunk("chatcmpl-resp", "gpt-4o", None, Some("stop")),
        openai_done(),
    ];
    let stream = stream::iter(
        chunks
            .into_iter()
            .map(|s| Ok::<_, TestError>(Bytes::from(s))),
    );
    let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
    let sse = super::create_responses_sse_stream(stream, "gpt-4o".to_string(), logs, mock_stats());
    tokio::pin!(sse);
    let mut raw = String::new();
    while let Some(item) = sse.next().await {
        let bytes = item.unwrap();
        raw.push_str(&String::from_utf8_lossy(&bytes));
    }

    assert!(raw.contains("event: response.created"));
    assert!(raw.contains("event: response.output_item.added"));
    assert!(raw.contains("event: response.output_text.delta"));
    assert!(raw.contains("event: response.completed"));
    assert!(raw.contains("data: [DONE]"));
}

/// A Responses client stops reading as soon as the terminal
/// `response.completed` event arrives — it has no reason to wait for the
/// trailing `data: [DONE]` sentinel, which only exists for SDKs that read
/// until the stream closes.
///
/// The recording block sits *after* the last `yield`, so dropping the
/// response body at that point skips it and the request never reaches the
/// request log. This is the regression: `/v1/responses` traffic that
/// succeeds but writes no row.
#[tokio::test]
async fn responses_client_stopping_at_completed_still_logs_the_request() {
    let chunks = vec![
        openai_chunk("chatcmpl-resp", "gpt-4o", Some("Hello"), None),
        openai_chunk("chatcmpl-resp", "gpt-4o", None, Some("stop")),
        openai_done(),
    ];
    let s = make_stream(chunks);
    let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
    let stats = mock_stats();
    let sse = super::create_responses_sse_stream(s, "gpt-4o".to_string(), logs, stats.clone());
    // Box it, as `Body::from_stream` does for the real response: the guard
    // has to fire when the *body* is dropped, not when the local is.
    let mut sse = Box::pin(sse);

    // Read only up to and including the `response.completed` event, then
    // drop the body — exactly what an SDK doing
    // `stream.until_completed()` leaves behind.
    let mut raw = String::new();
    while let Some(item) = sse.next().await {
        raw.push_str(&String::from_utf8_lossy(&item.unwrap()));
        if raw.contains("event: response.completed") {
            break;
        }
    }
    drop(sse);

    let rows = stats
        .query_request_logs(&crate::stats::RequestLogFilter::default())
        .unwrap()
        .items;
    assert_eq!(
        rows.len(),
        1,
        "a completed /v1/responses stream must leave exactly one log row, found {}",
        rows.len()
    );
    assert_eq!(rows[0].route, "/v1/responses");
    assert_eq!(rows[0].status, 200);
}

/// Upstreams such as WorkBuddy send `"finish_reason": ""` on every chunk.
/// The stream must stay open through those, and only close on the real
/// reason — otherwise the client gets an empty completed response.
#[tokio::test]
async fn responses_stream_with_empty_finish_reason_still_delivers_text() {
    let chunks = vec![
        openai_chunk("cmb-1", "glm-5.3-flash", Some("OK"), Some("")),
        openai_chunk("cmb-1", "glm-5.3-flash", Some("!"), Some("")),
        openai_chunk("cmb-1", "glm-5.3-flash", None, Some("stop")),
        openai_done(),
    ];
    let s = stream::iter(
        chunks
            .into_iter()
            .map(|c| Ok::<_, TestError>(Bytes::from(c))),
    );
    let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
    let sse =
        super::create_responses_sse_stream(s, "glm-5.3-flash".to_string(), logs, mock_stats());
    tokio::pin!(sse);
    let mut raw = String::new();
    while let Some(item) = sse.next().await {
        raw.push_str(&String::from_utf8_lossy(&item.unwrap()));
    }

    // `completed` must come last and carry the accumulated text — not
    // arrive first with an empty `output` array.
    let first_done = raw.find("response.output_text.done").unwrap();
    let completed = raw.find("response.completed").unwrap();
    assert!(
        first_done < completed,
        "text must be closed before the response completes: {raw}"
    );
    // The completed event itself must carry the accumulated text, not an
    // empty `output` array. (`response.created` legitimately has one.)
    let completed_body = &raw[completed..];
    assert!(
        completed_body.contains("OK!"),
        "completed response must carry the text: {raw}"
    );
    assert!(
        completed_body.contains("\"status\":\"completed\""),
        "completed event must be the terminal one: {raw}"
    );
}

#[tokio::test]
async fn responses_handler_end_to_end_non_streaming() {
    use axum::routing::post;
    use axum::Json;
    let mock_upstream = axum::Router::new().route(
        "/v1/chat/completions",
        post(|Json(req): Json<openai::OpenAIRequest>| async move {
            assert_eq!(req.messages.len(), 1);
            assert_eq!(req.messages[0].role, "user");
            Json(openai::OpenAIResponse {
                id: Some("chatcmpl-mock".to_string()),
                object: Some("chat.completion".to_string()),
                created: Some(1712345678),
                model: Some("gpt-4o".to_string()),
                choices: vec![openai::Choice {
                    index: 0,
                    message: openai::ChoiceMessage {
                        role: "assistant".to_string(),
                        content: Some("Response from mock".to_string()),
                        reasoning_content: None,
                        refusal: None,
                        tool_calls: None,
                    },
                    logprobs: None,
                    finish_reason: Some("stop".to_string()),
                }],
                usage: openai::Usage {
                    prompt_tokens: 12,
                    completion_tokens: 6,
                    total_tokens: 18,
                    prompt_tokens_details: None,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    ..Default::default()
                },
                system_fingerprint: None,
            })
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, mock_upstream).await.unwrap();
    });

    let config = std::sync::Arc::new(Config {
        upstream_urls: vec![format!("http://127.0.0.1:{}", port)],
        ..Default::default()
    });
    let client = reqwest::Client::new();
    let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
    let service = crate::service::ServiceController::new(false);
    service.mark_running();
    // The real Codex header set, reduced to the fields the session
    // resolver reads, so this test also proves the end-to-end attribution.
    let mut headers = HeaderMap::new();
    headers.insert("originator", "Codex".parse().unwrap());
    headers.insert(
        "session-id",
        "01a0cc30-3318-74d2-b045-650a0b0c2e1c".parse().unwrap(),
    );
    headers.insert(
        "x-codex-turn-metadata",
        r#"{"session_id":"01a0cc30-3318-74d2-b045-650a0b0c2e1c","turn_id":"01a0cc31-bc18-77c3-85d8-de3c452f781c"}"#
            .parse()
            .unwrap(),
    );
    let stats_db = mock_stats();
    let req = responses::ResponsesRequest {
        model: "gpt-4o".to_string(),
        input: responses::ResponsesInput::Text("Hello proxy".to_string()),
        instructions: None,
        tools: None,
        tool_choice: None,
        temperature: None,
        top_p: None,
        max_output_tokens: None,
        max_tokens: None,
        stream: Some(false),
        parallel_tool_calls: None,
        reasoning: None,
        store: None,
        include: None,
        prompt_cache_key: None,
    };

    let response = super::responses_proxy_handler(
        axum::Extension(config),
        axum::Extension(client),
        axum::Extension(logs.clone()),
        axum::Extension(service),
        axum::Extension(stats_db.clone()),
        axum::Extension(std::sync::Arc::new(
            crate::session_pool::CredentialPool::empty(),
        )),
        headers,
        json_request(&req),
    )
    .await
    .unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::OK);

    // The session must reach the log line, the request-log row and the
    // stats filter, which is the entire point of the change.
    let lines = logs.snapshot().await;
    let headers_line = lines
        .iter()
        .find(|l| l.message.starts_with("POST /v1/responses client=codex"))
        .expect("headers line carries the resolved session");
    assert!(
        headers_line
            .message
            .contains("session_id=codex:01a0cc30-3318-74d2-b045-650a0b0c2e1c"),
        "unexpected log line: {}",
        headers_line.message
    );
    assert!(
        headers_line
            .message
            .contains("turn=01a0cc31-bc18-77c3-85d8-de3c452f781c"),
        "unexpected log line: {}",
        headers_line.message
    );

    let rows = stats_db
        .query_request_logs(&crate::stats::RequestLogFilter {
            session_id: Some("codex:01a0cc30-3318-74d2-b045-650a0b0c2e1c".to_string()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(rows.items.len(), 1, "row is filterable by session id");
    assert_eq!(rows.items[0].client, "codex");
    assert_eq!(rows.clients, vec!["codex".to_string()]);

    // A different session must not match.
    let other = stats_db
        .query_request_logs(&crate::stats::RequestLogFilter {
            session_id: Some("codex:00000000-0000-7000-8000-000000000000".to_string()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(other.items.len(), 0);
}

#[tokio::test]
async fn responses_handler_end_to_end_streaming() {
    use axum::routing::post;
    use axum::Json;
    let mock_upstream = axum::Router::new().route(
        "/v1/chat/completions",
        post(|Json(_req): Json<openai::OpenAIRequest>| async move {
            let stream = futures::stream::iter(vec![
                Ok::<_, std::io::Error>(Bytes::from(openai_chunk(
                    "chatcmpl-stream-mock",
                    "gpt-4o",
                    Some("Streamed response"),
                    None,
                ))),
                Ok::<_, std::io::Error>(Bytes::from(openai_chunk(
                    "chatcmpl-stream-mock",
                    "gpt-4o",
                    None,
                    Some("stop"),
                ))),
                Ok::<_, std::io::Error>(Bytes::from(openai_done())),
            ]);
            let mut headers = HeaderMap::new();
            headers.insert(
                "Content-Type",
                axum::http::HeaderValue::from_static("text/event-stream"),
            );
            (headers, axum::body::Body::from_stream(stream)).into_response()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, mock_upstream).await.unwrap();
    });

    let config = std::sync::Arc::new(Config {
        upstream_urls: vec![format!("http://127.0.0.1:{}", port)],
        ..Default::default()
    });
    let client = reqwest::Client::new();
    let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
    let service = crate::service::ServiceController::new(false);
    service.mark_running();
    let headers = HeaderMap::new();
    let req = responses::ResponsesRequest {
        model: "gpt-4o".to_string(),
        input: responses::ResponsesInput::Text("Hello stream".to_string()),
        instructions: None,
        tools: None,
        tool_choice: None,
        temperature: None,
        top_p: None,
        max_output_tokens: None,
        max_tokens: None,
        stream: Some(true),
        parallel_tool_calls: None,
        reasoning: None,
        store: None,
        include: None,
        prompt_cache_key: None,
    };

    let response = super::responses_proxy_handler(
        axum::Extension(config),
        axum::Extension(client),
        axum::Extension(logs),
        axum::Extension(service),
        axum::Extension(mock_stats()),
        axum::Extension(std::sync::Arc::new(
            crate::session_pool::CredentialPool::empty(),
        )),
        headers,
        json_request(&req),
    )
    .await
    .unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
}

#[test]
fn test_usage_to_token_record_with_cache_variants() {
    // 1. OpenAI format with prompt_tokens_details.cached_tokens
    let usage_openai = openai::Usage {
        prompt_tokens: 100,
        completion_tokens: 30,
        total_tokens: 130,
        prompt_tokens_details: Some(openai::PromptTokensDetails {
            cached_tokens: 80,
            audio_tokens: 0,
        }),
        ..Default::default()
    };
    let rec = usage_openai.to_token_record();
    assert_eq!(rec.input, 20);
    assert_eq!(rec.cache_read, 80);
    assert_eq!(rec.cache_write, 0);
    assert_eq!(rec.output, 30);

    // 2. DeepSeek format with prompt_cache_hit_tokens
    let usage_deepseek = openai::Usage {
        prompt_tokens: 100,
        completion_tokens: 25,
        total_tokens: 125,
        prompt_cache_hit_tokens: Some(70),
        prompt_cache_miss_tokens: Some(30),
        ..Default::default()
    };
    let rec = usage_deepseek.to_token_record();
    assert_eq!(rec.input, 30);
    assert_eq!(rec.cache_read, 70);
    assert_eq!(rec.output, 25);

    // 3. Anthropic format with cache_read_input_tokens & cache_creation_input_tokens
    let usage_anthropic = openai::Usage {
        prompt_tokens: 150,
        completion_tokens: 50,
        total_tokens: 200,
        cache_read_input_tokens: Some(100),
        cache_creation_input_tokens: Some(30),
        ..Default::default()
    };
    let rec = usage_anthropic.to_token_record();
    assert_eq!(rec.input, 20); // 150 - 100 - 30
    assert_eq!(rec.cache_read, 100);
    assert_eq!(rec.cache_write, 30);
    assert_eq!(rec.output, 50);

    // 4. Top-level cached_tokens
    let usage_toplevel = openai::Usage {
        prompt_tokens: 80,
        completion_tokens: 10,
        total_tokens: 90,
        cached_tokens: Some(50),
        ..Default::default()
    };
    let rec = usage_toplevel.to_token_record();
    assert_eq!(rec.input, 30);
    assert_eq!(rec.cache_read, 50);
    assert_eq!(rec.output, 10);

    // 5. WorkBuddy shape: Anthropic-style fields present but zeroed, with
    //    the real hit count in the DeepSeek field. The zero must not win.
    let usage_workbuddy = openai::Usage {
        prompt_tokens: 1337,
        completion_tokens: 10,
        total_tokens: 1347,
        cache_read_input_tokens: Some(0),
        cache_creation_input_tokens: Some(0),
        prompt_cache_hit_tokens: Some(1216),
        prompt_cache_miss_tokens: Some(121),
        prompt_tokens_details: Some(openai::PromptTokensDetails {
            cached_tokens: 1216,
            audio_tokens: 0,
        }),
        ..Default::default()
    };
    let rec = usage_workbuddy.to_token_record();
    assert_eq!(
        rec.cache_read, 1216,
        "zero-valued fields must not mask a real hit"
    );
    assert_eq!(
        rec.input, 121,
        "uncached input should be prompt_tokens - hits"
    );
    assert_eq!(rec.output, 10);
}

/// A response with no cache information reports zero rather than inventing
/// a hit from an unrelated field.
#[test]
fn test_usage_cache_read_zero_when_absent() {
    let usage = openai::Usage {
        prompt_tokens: 500,
        completion_tokens: 20,
        total_tokens: 520,
        cache_read_input_tokens: Some(0),
        ..Default::default()
    };
    let rec = usage.to_token_record();
    assert_eq!(rec.cache_read, 0);
    assert_eq!(rec.input, 500);
}

/// Explicit cache fields are still honored when they carry the real value.
#[test]
fn test_usage_explicit_zero_is_not_overridden_by_details() {
    // Anthropic-style reads win only when positive; a zero reads as "no
    // cache" and the remaining sources are consulted instead.
    let usage = openai::Usage {
        prompt_tokens: 200,
        completion_tokens: 5,
        total_tokens: 205,
        cache_read_input_tokens: Some(0),
        cached_tokens: Some(150),
        ..Default::default()
    };
    assert_eq!(usage.cache_read_tokens(), 150);
}

#[tokio::test]
async fn chat_completions_handler_end_to_end_non_streaming() {
    use axum::routing::post;
    use axum::Json;

    let mock_upstream = axum::Router::new().route(
        "/v1/chat/completions",
        post(|Json(req): Json<openai::OpenAIRequest>| async move {
            // Verify model was remapped by proxy
            assert_eq!(req.model, "gpt-4o-upstream");
            // Verify extra parameter was preserved
            assert_eq!(
                req.extra.get("reasoning_effort").and_then(|v| v.as_str()),
                Some("low")
            );

            let resp = openai::OpenAIResponse {
                id: Some("chatcmpl-test-123".to_string()),
                object: Some("chat.completion".to_string()),
                created: Some(1700000000),
                model: Some("gpt-4o-upstream".to_string()),
                choices: vec![openai::Choice {
                    index: 0,
                    message: openai::ChoiceMessage {
                        role: "assistant".to_string(),
                        content: Some("Hello from OpenAI mock!".to_string()),
                        reasoning_content: None,
                        refusal: None,
                        tool_calls: None,
                    },
                    logprobs: None,
                    finish_reason: Some("stop".to_string()),
                }],
                usage: openai::Usage {
                    prompt_tokens: 100,
                    completion_tokens: 40,
                    total_tokens: 140,
                    prompt_tokens_details: Some(openai::PromptTokensDetails {
                        cached_tokens: 60,
                        audio_tokens: 0,
                    }),
                    ..Default::default()
                },
                system_fingerprint: None,
            };
            Json(resp).into_response()
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, mock_upstream).await.unwrap();
    });

    let mut model_map = std::collections::BTreeMap::new();
    model_map.insert("gpt-alias".to_string(), "gpt-4o-upstream".to_string());

    let config = std::sync::Arc::new(Config {
        upstream_urls: vec![format!("http://127.0.0.1:{}", port)],
        model_map,
        ..Default::default()
    });
    let client = reqwest::Client::new();
    let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
    let service = crate::service::ServiceController::new(false);
    service.mark_running();
    let stats_db = mock_stats();

    let mut extra = serde_json::Map::new();
    extra.insert("reasoning_effort".to_string(), json!("low"));

    let req = openai::OpenAIRequest {
        model: "gpt-alias".to_string(),
        messages: vec![openai::Message {
            role: "user".to_string(),
            content: Some(openai::MessageContent::Text("Hi".to_string())),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }],
        max_tokens: Some(100),
        max_completion_tokens: None,
        temperature: None,
        top_p: None,
        stop: None,
        stream: Some(false),
        stream_options: None,
        tools: None,
        tool_choice: None,
        extra,
    };

    let response = super::chat_completions_proxy_handler(
        axum::Extension(config),
        axum::Extension(client),
        axum::Extension(logs),
        axum::Extension(service),
        axum::Extension(stats_db.clone()),
        axum::Extension(std::sync::Arc::new(
            crate::session_pool::CredentialPool::empty(),
        )),
        HeaderMap::new(),
        json_request(&req),
    )
    .await
    .unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::OK);

    // Verify stats in SQLite
    let today = stats_db.query_today().unwrap();
    assert_eq!(today.requests_success, 1);
    assert_eq!(today.tokens_input, 40); // 100 - 60 cached
    assert_eq!(today.tokens_cache_read, 60);
    assert_eq!(today.tokens_output, 40);
    assert_eq!(today.cache_hit_pct(), 60);
}

#[tokio::test]
async fn chat_completions_handler_end_to_end_streaming() {
    use axum::routing::post;
    use axum::Json;

    let mock_upstream = axum::Router::new().route(
        "/v1/chat/completions",
        post(|Json(req): Json<openai::OpenAIRequest>| async move {
            // Verify include_usage was set to true for stream
            assert!(req.stream_options.map(|s| s.include_usage).unwrap_or(false));

            let chunk1 = json!({
                "id": "chatcmpl-stream-1",
                "object": "chat.completion.chunk",
                "created": 1700000000,
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "delta": { "content": "Streamed " },
                    "finish_reason": null
                }]
            });

            let chunk2 = json!({
                "id": "chatcmpl-stream-1",
                "object": "chat.completion.chunk",
                "created": 1700000000,
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "delta": { "content": "chat!" },
                    "finish_reason": "stop"
                }]
            });

            let chunk_usage = json!({
                "id": "chatcmpl-stream-1",
                "object": "chat.completion.chunk",
                "created": 1700000000,
                "model": "gpt-4o",
                "choices": [],
                "usage": {
                    "prompt_tokens": 100,
                    "completion_tokens": 20,
                    "total_tokens": 120,
                    "prompt_cache_hit_tokens": 80,
                    "prompt_cache_miss_tokens": 20
                }
            });

            let stream = futures::stream::iter(vec![
                Ok::<_, std::io::Error>(Bytes::from(format!(
                    "data: {}\n\n",
                    serde_json::to_string(&chunk1).unwrap()
                ))),
                Ok::<_, std::io::Error>(Bytes::from(format!(
                    "data: {}\n\n",
                    serde_json::to_string(&chunk2).unwrap()
                ))),
                Ok::<_, std::io::Error>(Bytes::from(format!(
                    "data: {}\n\n",
                    serde_json::to_string(&chunk_usage).unwrap()
                ))),
                Ok::<_, std::io::Error>(Bytes::from("data: [DONE]\n\n")),
            ]);

            let mut headers = HeaderMap::new();
            headers.insert(
                "Content-Type",
                axum::http::HeaderValue::from_static("text/event-stream"),
            );
            (headers, axum::body::Body::from_stream(stream)).into_response()
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, mock_upstream).await.unwrap();
    });

    let config = std::sync::Arc::new(Config {
        upstream_urls: vec![format!("http://127.0.0.1:{}", port)],
        ..Default::default()
    });
    let client = reqwest::Client::new();
    let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
    let service = crate::service::ServiceController::new(false);
    service.mark_running();
    let stats_db = mock_stats();

    let req = openai::OpenAIRequest {
        model: "gpt-4o".to_string(),
        messages: vec![openai::Message {
            role: "user".to_string(),
            content: Some(openai::MessageContent::Text("Stream test".to_string())),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }],
        max_tokens: None,
        max_completion_tokens: None,
        temperature: None,
        top_p: None,
        stop: None,
        stream: Some(true),
        stream_options: None,
        tools: None,
        tool_choice: None,
        extra: serde_json::Map::new(),
    };

    let response = super::chat_completions_proxy_handler(
        axum::Extension(config),
        axum::Extension(client),
        axum::Extension(logs),
        axum::Extension(service),
        axum::Extension(stats_db.clone()),
        axum::Extension(std::sync::Arc::new(
            crate::session_pool::CredentialPool::empty(),
        )),
        HeaderMap::new(),
        json_request(&req),
    )
    .await
    .unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );

    // Read the stream to completion so create_chat_sse_stream finishes
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    // Verify stats in SQLite
    let today = stats_db.query_today().unwrap();
    assert_eq!(today.requests_success, 1);
    assert_eq!(today.tokens_input, 20); // 100 - 80 cached
    assert_eq!(today.tokens_cache_read, 80);
    assert_eq!(today.tokens_output, 20);
    assert_eq!(today.cache_hit_pct(), 80);
}

/// A provider that only serves streaming bodies (`force_stream`): the
/// non-streaming client still gets one plain JSON body, and the upstream
/// sees exactly one request, with `stream:true` on it.
#[tokio::test]
async fn force_stream_provider_serves_a_non_streaming_client_one_json_body() {
    use axum::routing::post;
    use axum::Json;

    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls_for_handler = calls.clone();

    let mock_upstream = axum::Router::new().route(
        "/v1/chat/completions",
        post(move |Json(req): Json<openai::OpenAIRequest>| {
            let calls = calls_for_handler.clone();
            async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                // A plain body is refused outright, exactly as WorkBuddy
                // does. If the proxy ever stops upgrading, this fires.
                if req.stream != Some(true) {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(json!({
                            "code": 11101,
                            "msg": "Non-stream chat request is currently not supported"
                        })),
                    )
                        .into_response();
                }
                assert!(
                    req.stream_options.map(|s| s.include_usage).unwrap_or(false),
                    "an upgraded request must ask for usage"
                );

                let chunk1 = json!({
                    "id": "cmb-1", "object": "chat.completion.chunk",
                    "created": 1700000000, "model": "glm",
                    "choices": [{"index": 0, "delta": {"content": "Hello "}}]
                });
                let chunk2 = json!({
                    "id": "cmb-1", "object": "chat.completion.chunk",
                    "created": 1700000000, "model": "glm",
                    "choices": [{"index": 0, "delta": {"content": "world"}, "finish_reason": "stop"}]
                });
                let chunk_usage = json!({
                    "id": "cmb-1", "object": "chat.completion.chunk",
                    "created": 1700000000, "model": "glm", "choices": [],
                    "usage": {"prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 9}
                });

                let stream = futures::stream::iter(
                    [chunk1, chunk2, chunk_usage]
                        .into_iter()
                        .map(|c| {
                            Ok::<_, std::io::Error>(Bytes::from(format!(
                                "data: {}\n\n",
                                serde_json::to_string(&c).unwrap()
                            )))
                        })
                        .chain(std::iter::once(Ok::<_, std::io::Error>(Bytes::from(
                            "data: [DONE]\n\n",
                        )))),
                );

                let mut headers = HeaderMap::new();
                headers.insert(
                    "Content-Type",
                    axum::http::HeaderValue::from_static("text/event-stream"),
                );
                (headers, axum::body::Body::from_stream(stream)).into_response()
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, mock_upstream).await.unwrap();
    });

    let config = std::sync::Arc::new(Config {
        upstream_urls: vec![format!("http://127.0.0.1:{}", port)],
        force_stream_upstream: true,
        ..Default::default()
    });
    let service = crate::service::ServiceController::new(false);
    service.mark_running();

    let req = openai::OpenAIRequest {
        model: "glm".to_string(),
        messages: vec![openai::Message {
            role: "user".to_string(),
            content: Some(openai::MessageContent::Text("Hi".to_string())),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }],
        max_tokens: None,
        max_completion_tokens: None,
        temperature: None,
        top_p: None,
        stop: None,
        // The client explicitly asked for a single body, not a stream.
        stream: Some(false),
        stream_options: None,
        tools: None,
        tool_choice: None,
        extra: serde_json::Map::new(),
    };

    let response = super::chat_completions_proxy_handler(
        axum::Extension(config),
        axum::Extension(reqwest::Client::new()),
        axum::Extension(std::sync::Arc::new(crate::settings::LogBuffer::new(10))),
        axum::Extension(service),
        axum::Extension(mock_stats()),
        axum::Extension(std::sync::Arc::new(
            crate::session_pool::CredentialPool::empty(),
        )),
        HeaderMap::new(),
        json_request(&req),
    )
    .await
    .unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json",
        "a non-streaming client must not be handed an event stream"
    );

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let parsed: openai::OpenAIResponse = serde_json::from_slice(&body).unwrap();

    // The two SSE chunks were aggregated back into one assistant message.
    assert_eq!(parsed.choices.len(), 1);
    assert_eq!(
        parsed.choices[0].message.content.as_deref(),
        Some("Hello world")
    );
    assert_eq!(parsed.choices[0].finish_reason.as_deref(), Some("stop"));
    assert_eq!(parsed.usage.prompt_tokens, 7);
    assert_eq!(parsed.usage.completion_tokens, 2);

    // And it took a single upstream call: the upgrade happened up front,
    // rather than as a retry after the 11101 refusal.
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// Feeds SSE text through `collect_stream_into_response` by serving it
/// from a real HTTP socket, so the bytes arrive as a genuine response body.
async fn aggregate(sse: &str) -> openai::OpenAIResponse {
    aggregate_timed(sse).await.0
}

/// As [`aggregate`], but also exposing the observed upstream timings.
async fn aggregate_timed(sse: &str) -> (openai::OpenAIResponse, UpstreamTiming) {
    let body = sse.to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let payload = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            use tokio::io::AsyncWriteExt;
            let _ = sock.write_all(payload.as_bytes()).await;
            let _ = sock.flush().await;
        }
    });

    let resp = reqwest::get(format!("http://{}/", addr)).await.unwrap();
    super::collect_stream_into_response(resp, Instant::now())
        .await
        .unwrap()
}

fn text_chunk(content: &str, finish: Option<&str>) -> String {
    let delta = json!({"content": content});
    let mut choice = json!({"index": 0, "delta": delta});
    if let Some(f) = finish {
        choice["finish_reason"] = json!(f);
    }
    format!(
        "data: {}\n\n",
        serde_json::to_string(&json!({
            "id": "cmb-1", "model": "glm", "choices": [choice]
        }))
        .unwrap()
    )
}

#[tokio::test]
async fn aggregate_merges_text_chunks() {
    let sse = format!(
        "{}{}{}",
        text_chunk("Hello", Some("")),
        text_chunk(" world", Some("")),
        text_chunk("", Some("stop")),
    );
    let resp = aggregate(&sse).await;
    assert_eq!(resp.choices.len(), 1);
    assert_eq!(
        resp.choices[0].message.content.as_deref(),
        Some("Hello world")
    );
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
    assert_eq!(resp.choices[0].message.role, "assistant");
}

#[tokio::test]
async fn aggregate_merges_split_tool_call_arguments() {
    // Arguments arrive in fragments across chunks, keyed by index.
    let mk = |id: Option<&str>, name: Option<&str>, args: Option<&str>| {
        let mut func = json!({});
        if let Some(n) = name {
            func["name"] = json!(n);
        }
        if let Some(a) = args {
            func["arguments"] = json!(a);
        }
        let mut tc = json!({"index": 0, "function": func});
        if let Some(i) = id {
            tc["id"] = json!(i);
            tc["type"] = json!("function");
        }
        format!(
            "data: {}\n\n",
            serde_json::to_string(&json!({
                "id": "cmb-1", "model": "glm",
                "choices": [{"index": 0, "delta": {"tool_calls": [tc]}}]
            }))
            .unwrap()
        )
    };

    let sse = format!(
        "{}{}{}{}",
        mk(Some("c1"), Some("exec_command"), None),
        mk(None, None, Some(r#"{\"cmd\""#)),
        mk(None, None, Some(r#":\"ls\"}"#)),
        text_chunk("", Some("tool_calls")),
    );
    let resp = aggregate(&sse).await;
    let calls = resp.choices[0]
        .message
        .tool_calls
        .as_ref()
        .expect("tool call");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "c1");
    assert_eq!(calls[0].function.name, "exec_command");
    assert_eq!(calls[0].function.arguments, r#"{\"cmd\":\"ls\"}"#);
}

#[tokio::test]
async fn aggregate_captures_usage_and_drops_half_open_calls() {
    // A tool call that never received an id would be unanswerable, so it
    // must not reach the client.
    let usage_chunk = format!(
        "data: {}\n\n",
        serde_json::to_string(&json!({
            "id": "cmb-1", "model": "glm",
            "choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "function": {"name": "orphan"}}
            ]}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14}
        }))
        .unwrap()
    );
    let sse = format!("{}{}", text_chunk("hi", Some("")), usage_chunk);
    let resp = aggregate(&sse).await;
    assert_eq!(resp.usage.prompt_tokens, 10);
    assert_eq!(resp.usage.completion_tokens, 4);
    assert!(
        resp.choices[0].message.tool_calls.is_none(),
        "half-open tool call must be dropped"
    );
}

#[tokio::test]
async fn non_streaming_request_is_upgraded_when_provider_requires_stream() {
    // WorkBuddy rejects a plain body (11101), so the outbound request must
    // ask for a stream even though the client did not.
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let seen_for_handler = seen.clone();
    let mock_upstream = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(
            |axum::Json(req): axum::Json<openai::OpenAIRequest>| async move {
                *seen_for_handler.lock().unwrap() =
                    Some((req.stream, req.stream_options.is_some()));
                axum::Json(openai::OpenAIResponse {
                    id: Some("cmb-1".into()),
                    object: Some("chat.completion".into()),
                    created: Some(1),
                    model: Some("glm".into()),
                    choices: vec![openai::Choice {
                        index: 0,
                        message: openai::ChoiceMessage {
                            role: "assistant".into(),
                            content: Some("direct".into()),
                            reasoning_content: None,
                            refusal: None,
                            tool_calls: None,
                        },
                        logprobs: None,
                        finish_reason: Some("stop".into()),
                    }],
                    usage: openai::Usage::default(),
                    system_fingerprint: None,
                })
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, mock_upstream).await.unwrap() });

    let config = std::sync::Arc::new(Config {
        upstream_urls: vec![format!("http://{}/v1/chat/completions", addr)],
        force_stream_upstream: true,
        ..Default::default()
    });
    let req = openai::OpenAIRequest {
        model: "glm".into(),
        messages: vec![openai::Message {
            role: "user".into(),
            content: Some(openai::MessageContent::Text("hi".into())),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }],
        max_tokens: None,
        max_completion_tokens: None,
        temperature: None,
        top_p: None,
        stop: None,
        stream: None,
        stream_options: None,
        tools: None,
        tool_choice: None,
        extra: serde_json::Map::new(),
    };

    let resp = super::forward_request(
        config,
        reqwest::Client::new(),
        req,
        None,
        std::sync::Arc::new(crate::settings::LogBuffer::new(10)),
        "glm".to_string(),
        mock_stats(),
        super::ApiFlavor::Chat,
        false,
        "/v1/chat/completions",
        std::time::Instant::now(),
        SessionInfo::unknown(),
        "client=unknown".to_string(),
        Default::default(),
        std::sync::Arc::new(crate::session_pool::CredentialPool::empty()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let (stream, has_opts) = seen.lock().unwrap().unwrap();
    assert_eq!(stream, Some(true), "must upgrade to stream upstream");
    assert!(has_opts, "must request usage in the stream");
}

// ── Truncated upstream body: the 2026-10-07 "error decoding response body" 500 ──

/// Serve one HTTP response whose `Content-Length` promises `declared` bytes
/// while only `body` is sent, then drop the socket.
///
/// This is the real shape of the failure recorded on 2026-10-07 14:59:47: the
/// upstream declared a length it never finished sending, so the client read the
/// frames that did arrive and then hit EOF early.
async fn serve_truncated(body: &'static str, declared: usize) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let payload = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n{body}"
            );
            use tokio::io::AsyncWriteExt;
            let _ = sock.write_all(payload.as_bytes()).await;
            let _ = sock.flush().await;
            drop(sock);
        }
    });
    format!("http://{addr}/")
}

/// Drive one flavor's framer to exhaustion over a real socket, returning every
/// byte a client would have received.
async fn drain_flavor(
    flavor: super::ApiFlavor,
    route: &'static str,
    url: String,
    logs: std::sync::Arc<crate::settings::LogBuffer>,
    stats: std::sync::Arc<crate::stats::StatsDb>,
) -> String {
    let response = reqwest::get(url).await.expect("upstream reachable");
    let sse = super::create_flavor_sse_stream(
        response.bytes_stream(),
        flavor,
        "deepseek-v4.1-flash".to_string(),
        route,
        Instant::now(),
        Instant::now(),
        logs,
        stats,
        SessionInfo::unknown(),
        "client=unknown".to_string(),
        Default::default(),
    );
    tokio::pin!(sse);

    let mut out = String::new();
    while let Some(item) = sse.next().await {
        match item {
            Ok(bytes) => out.push_str(&String::from_utf8_lossy(&bytes)),
            Err(err) => panic!("the framer must not propagate transport errors: {err}"),
        }
    }
    out
}

/// A truncated Chat stream must end with a parseable `error` frame.
///
/// Before this, the `ApiFlavor::Chat` arm only logged: the client received HTTP
/// 200 and a body that simply stopped, which looks like a half-finished answer
/// rather than a failure. Chat's schema carries errors as a `data:` frame with
/// an `error` object — what the OpenAI SDKs throw on — so that is what is sent.
#[tokio::test]
async fn a_truncated_chat_stream_reports_an_error_frame() {
    let good =
        "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n";
    let url = serve_truncated(good, 100_000).await;

    let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(100));
    let stats = mock_stats();
    let out = drain_flavor(
        super::ApiFlavor::Chat,
        "/v1/chat/completions",
        url,
        logs,
        stats,
    )
    .await;

    assert!(
        out.contains("\"content\":\"partial\""),
        "the chunks that arrived must still be forwarded: {out}"
    );
    assert!(
        out.contains("\"error\""),
        "a truncated Chat stream must end with an error frame, got: {out}"
    );
    assert!(
        out.contains("truncated"),
        "the frame must say what happened, got: {out}"
    );
}

/// The request row and the GUI log must name the truncation instead of
/// repeating reqwest's opaque `error decoding response body`.
#[tokio::test]
async fn a_truncated_stream_records_the_real_cause() {
    let good =
        "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n";
    let url = serve_truncated(good, 100_000).await;

    let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(100));
    let stats = mock_stats();
    let out = drain_flavor(
        super::ApiFlavor::Chat,
        "/v1/chat/completions",
        url,
        logs.clone(),
        stats.clone(),
    )
    .await;
    assert!(
        out.contains("\"error\""),
        "the stream ends in an error: {out}"
    );

    let rows = stats
        .query_request_logs(&crate::stats::RequestLogFilter::default())
        .unwrap()
        .items;
    assert_eq!(rows.len(), 1, "one truncated stream is one row: {rows:?}");
    assert_eq!(rows[0].status, 500, "a failed stream row is a 500");

    let error = rows[0].error.as_deref().unwrap_or_default();
    assert!(
        error.contains("truncated"),
        "the row must name the failure, got: {error:?}"
    );
    // The cause chain is the actionable part; the opaque outer message alone is
    // exactly what made the original row useless.
    assert!(
        error.len() > "error decoding response body".len(),
        "the row must carry the cause chain, got: {error:?}"
    );

    let logged = logs
        .snapshot()
        .await
        .iter()
        .map(|e| e.message.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        logged.contains("truncated"),
        "the GUI log must name the failure, got: {logged}"
    );
}

/// The control: a stream that ends normally keeps `[DONE]` and records a 200, so
/// the new error path did not disturb the healthy case.
#[tokio::test]
async fn a_complete_chat_stream_is_unaffected() {
    let body = "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let payload_len = body.len();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let payload = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {payload_len}\r\nConnection: close\r\n\r\n{body}"
            );
            use tokio::io::AsyncWriteExt;
            let _ = sock.write_all(payload.as_bytes()).await;
            let _ = sock.flush().await;
            drop(sock);
        }
    });

    let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(100));
    let stats = mock_stats();
    let out = drain_flavor(
        super::ApiFlavor::Chat,
        "/v1/chat/completions",
        format!("http://{addr}/"),
        logs,
        stats.clone(),
    )
    .await;

    assert!(
        out.contains("[DONE]"),
        "a healthy stream keeps its terminator"
    );
    assert!(
        !out.contains("\"error\""),
        "no error frame when nothing failed"
    );

    let rows = stats
        .query_request_logs(&crate::stats::RequestLogFilter::default())
        .unwrap()
        .items;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, 200, "a complete stream is a success row");
    assert_eq!(rows[0].error, None);
}

/// A truncated *non-streaming* upstream body is retried on the next URL instead
/// of being reported to the client.
///
/// The original failure was a stream, but the same reqwest error surfaces on the
/// aggregation path: a provider that only serves streams is asked for one and
/// the answer is cut short. That is a transport failure worth a second attempt,
/// whereas a body that arrived whole and failed to deserialize would fail
/// identically everywhere.
#[tokio::test]
async fn a_truncated_body_is_retried_on_the_next_url() {
    // First upstream: declares more than it sends, so the body is cut short.
    let truncating = "{\"id\":\"x\",\"choices\":[";
    let bad_url = serve_truncated(truncating, 100_000).await;

    // Second upstream: a complete, valid non-streaming response.
    let upstream = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(
            |axum::Json(_req): axum::Json<openai::OpenAIRequest>| async move {
                axum::Json(json!({
                    "id": "chatcmpl-recovered",
                    "object": "chat.completion",
                    "created": 1700000000,
                    "model": "gpt-4o",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "recovered"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
                }))
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });

    let config = std::sync::Arc::new(Config {
        // The bad URL is tried first, so only the retry can produce a 200.
        upstream_urls: vec![bad_url, format!("http://127.0.0.1:{port}")],
        ..Default::default()
    });
    let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(50));

    let req = openai::OpenAIRequest {
        model: "gpt-4o".to_string(),
        messages: vec![openai::Message {
            role: "user".to_string(),
            content: Some(openai::MessageContent::Text("hi".to_string())),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }],
        max_tokens: None,
        max_completion_tokens: None,
        temperature: None,
        top_p: None,
        stop: None,
        stream: Some(false),
        stream_options: None,
        tools: None,
        tool_choice: None,
        extra: serde_json::Map::new(),
    };

    let resp = super::forward_request(
        config,
        reqwest::Client::new(),
        req,
        None,
        logs.clone(),
        "gpt-4o".to_string(),
        mock_stats(),
        super::ApiFlavor::Chat,
        false,
        "/v1/chat/completions",
        std::time::Instant::now(),
        SessionInfo::unknown(),
        "client=unknown".to_string(),
        Default::default(),
        std::sync::Arc::new(crate::session_pool::CredentialPool::empty()),
        None,
    )
    .await
    .expect("a truncated body must not fail the request while a retry can succeed");

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("recovered"),
        "the retry's answer must reach the client: {text}"
    );

    let logged = logs
        .snapshot()
        .await
        .iter()
        .map(|e| e.message.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        logged.contains("FAILED MID-TRANSFER") && logged.contains("truncated"),
        "the truncated body must be named in the log: {logged}"
    );
}

/// A body that arrived intact but is not valid JSON is *not* retried: it would
/// fail identically on every upstream, so the error is reported directly.
#[tokio::test]
async fn an_unparseable_body_is_reported_rather_than_retried() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let body = "this is not json";
            let payload = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            use tokio::io::AsyncWriteExt;
            let _ = sock.write_all(payload.as_bytes()).await;
            let _ = sock.flush().await;
            drop(sock);
        }
    });

    let config = std::sync::Arc::new(Config {
        upstream_urls: vec![format!("http://{addr}")],
        ..Default::default()
    });
    let req = openai::OpenAIRequest {
        model: "gpt-4o".to_string(),
        messages: vec![openai::Message {
            role: "user".to_string(),
            content: Some(openai::MessageContent::Text("hi".to_string())),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }],
        max_tokens: None,
        max_completion_tokens: None,
        temperature: None,
        top_p: None,
        stop: None,
        stream: Some(false),
        stream_options: None,
        tools: None,
        tool_choice: None,
        extra: serde_json::Map::new(),
    };

    let err = super::forward_request(
        config,
        reqwest::Client::new(),
        req,
        None,
        std::sync::Arc::new(crate::settings::LogBuffer::new(50)),
        "gpt-4o".to_string(),
        mock_stats(),
        super::ApiFlavor::Chat,
        false,
        "/v1/chat/completions",
        std::time::Instant::now(),
        SessionInfo::unknown(),
        "client=unknown".to_string(),
        Default::default(),
        std::sync::Arc::new(crate::session_pool::CredentialPool::empty()),
        None,
    )
    .await
    .expect_err("malformed JSON is a real failure");

    // The rendered message keeps the parse detail rather than the bare reqwest
    // phrase, so the row explains itself.
    let text = err.to_string();
    assert!(
        text.contains("expected") || text.contains("line"),
        "the parse failure must be named: {text}"
    );
    server.abort();
}

/// A stalled upstream must not be described as a truncated body (and vice
/// versa): both are retriable transport failures but they point at different
/// causes, and the log is read to decide which one to chase.
#[tokio::test]
async fn a_stalled_stream_is_described_as_a_timeout_not_a_truncation() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            use tokio::io::AsyncWriteExt;
            // A partial chunked body, then silence: the client's read timeout is
            // what must fire, not an EOF.
            let _ = sock
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n8\r\ndata: {\"",
                )
                .await;
            let _ = sock.flush().await;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });

    let client = reqwest::Client::builder()
        .read_timeout(std::time::Duration::from_millis(200))
        .build()
        .unwrap();
    let response = client.get(format!("http://{addr}/")).send().await.unwrap();
    let mut stream = response.bytes_stream();
    let error = loop {
        match stream.next().await {
            Some(Ok(_)) => continue,
            Some(Err(e)) => break e,
            None => panic!("the stalled stream must end in an error"),
        }
    };

    let summary = super::upstream_read_error(&error);
    assert!(
        summary.contains("timed out"),
        "a stall must be named as a stall, got: {summary}"
    );
    assert!(
        summary.contains("operation timed out"),
        "the underlying timeout cause must survive, got: {summary}"
    );
    assert!(
        !summary.contains("truncated"),
        "a stall is not a truncation, got: {summary}"
    );
}
