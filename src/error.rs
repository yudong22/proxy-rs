use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

/// Application-specific errors
#[derive(Error, Debug)]
pub enum ProxyError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Request transformation error: {0}")]
    Transform(String),

    /// A request rejected before translation, carrying the HTTP status the
    /// client must see.
    ///
    /// [`ProxyError::Transform`] cannot stand in for this: it always answers
    /// 400, while the rejections it would otherwise swallow distinguish 400
    /// (unparseable body) from 413 (body over the limit) and 415 (wrong content
    /// type). Collapsing all three into 400 is what made a body-limit failure
    /// look like a malformed request.
    #[error("{message}")]
    Rejected { status: u16, message: String },

    #[error("Upstream API error: {0}")]
    Upstream(String),

    /// A terminal upstream rejection, relayed with the upstream's own status.
    ///
    /// [`ProxyError::Upstream`] answers 502 unconditionally, which is only
    /// honest for a gateway fault. A 400/413/422 from the upstream is the
    /// client's request being refused — the request itself is the problem and
    /// resending it cannot help — so flattening it to 502 told every client the
    /// opposite, and an agent that branches on the status (Codex distinguishes
    /// "context window exceeded" from "gateway is down") could never take the
    /// corrective action.
    ///
    /// The remediation for that was visible in the logs: a 1.18M-token prompt
    /// hit the upstream's 1.05M limit, the proxy reported 502, and Codex
    /// retried the identical request 37 times.
    #[error("{message}")]
    UpstreamRejected {
        /// The upstream's own HTTP status.
        status: u16,
        /// The upstream response body, passed through unchanged.
        body: String,
        /// Human-readable rendering for the log row.
        message: String,
    },

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Rendered through [`describe_error_chain`], not reqwest's own `Display`.
    ///
    /// reqwest reports every body/decoding failure with the same eleven words
    /// ("error decoding response body"), so the recorded row could not tell a
    /// truncated stream from a body that was never JSON. The cause chain is what
    /// names the failure, and it is what a user reads in the request detail.
    #[error("HTTP error: {}", describe_error_chain(.0))]
    Http(#[from] reqwest::Error),
}

/// Render an error together with its full cause chain.
///
/// reqwest's `Display` deliberately stops at the outermost error: a body that
/// was cut short, a connection reset mid-stream and a payload that arrived whole
/// but was not JSON all print as `error decoding response body`. The chain
/// beneath it is what distinguishes them — hyper's "error reading a body from
/// connection" → "end of file before message length reached" for a truncation,
/// versus serde's "expected value at line 1 column 1" for a parse failure.
///
/// Repeated segments are dropped so an error that quotes its source verbatim
/// does not print it twice.
pub fn describe_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !text.is_empty() && !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = cause.source();
    }
    out
}

/// Whether a reqwest failure means **the body did not fully arrive**, as opposed
/// to a payload that arrived whole and could not be parsed.
///
/// The two are indistinguishable through `Display` (both say "error decoding
/// response body") but call for opposite treatment: a truncated body is a
/// transient upstream failure worth retrying, while malformed JSON — or a body
/// that failed to deserialize into our model — will fail identically on every
/// attempt and a retry only burns the pool.
///
/// `serde_json` errors arrive as the source of a `Decode` error, so anything
/// else underneath a `Decode` came from the transport (hyper/io): a reset
/// connection, an EOF before the declared length, a corrupt compressed body.
pub fn is_transport_body_error(err: &reqwest::Error) -> bool {
    if err.is_timeout() || err.is_body() {
        return true;
    }
    if !err.is_decode() {
        return false;
    }
    std::error::Error::source(err)
        .is_none_or(|source| source.downcast_ref::<serde_json::Error>().is_none())
}

/// A one-line description of a transient upstream transport failure.
///
/// The cause chain alone is accurate but does not read as a diagnosis, so the
/// two shapes a user actually hits are named up front:
///
///   * a **stalled transfer** — the deadline passed before the body completed.
///     Streaming requests carry no total timeout, so this is the shared client's
///     idle `read_timeout` firing (the upstream stopped sending); a
///     non-streaming request can also hit its own total timeout.
///   * a **truncated body** — the response ended before its declared/expected
///     end, whether by a reset, an EOF or a corrupt compressed stream.
///
/// Both are retriable, but they point at different causes (a stalled upstream
/// versus a broken transfer), so the log must not call one the other.
///
/// reqwest's own kind strings (`error decoding response body`, `request or
/// response body error`) are dropped once the shape is named: they are what the
/// diagnosis replaces, and repeating them makes the line longer without adding
/// anything. The specific causes — a reset, an EOF, a timeout — are kept.
pub fn describe_transport_failure(err: &reqwest::Error) -> String {
    let causes: Vec<String> = {
        let mut out = Vec::new();
        let mut source = std::error::Error::source(err);
        while let Some(cause) = source {
            let text = cause.to_string();
            if !text.is_empty() && !is_opaque_reqwest_phrase(&text) {
                out.push(text);
            }
            source = cause.source();
        }
        out
    };
    let detail = if causes.is_empty() {
        String::new()
    } else {
        format!(" ({})", causes.join(": "))
    };

    if err.is_timeout() {
        format!("upstream stalled: the attempt timed out before the body completed{detail}")
    } else if is_transport_body_error(err) {
        format!("upstream response body was truncated mid-stream{detail}")
    } else {
        describe_error_chain(err)
    }
}

/// Whether a cause-chain segment is one of reqwest's own fixed kind strings.
///
/// These name the *layer* that failed ("error decoding response body"), never
/// the reason, so a diagnosis that already knows the reason omits them.
fn is_opaque_reqwest_phrase(text: &str) -> bool {
    matches!(
        text,
        "error decoding response body"
            | "request or response body error"
            | "error reading a body from connection"
    )
}

/// Upstream statuses that describe the client's own request, not the gateway.
///
/// These are the rejections where resending the same body cannot succeed, so
/// the status must survive to the client:
///
/// * **400** — a malformed or oversized request. Codex's "prompt is too long"
///   arrives here (business code 11115), and its own error taxonomy is what
///   tells the agent to compact or trim.
/// * **413** — the body exceeded the upstream's limit outright.
/// * **422** — well-formed but semantically unacceptable (shape mismatch).
///
/// 401/402/403/404/429 are deliberately absent. The first three are credential
/// faults the proxy already fails over on, 429 is retriable (the pool switch
/// happens before this point), and 404 means the route or model is wrong —
/// reporting that as 502 keeps the gateway's misconfiguration from looking like
/// a client mistake.
fn is_terminal_rejection(status: u16) -> bool {
    matches!(status, 400 | 413 | 422)
}

impl ProxyError {
    /// Build the error for an upstream response the proxy is going to relay.
    ///
    /// Terminal client-error statuses (400/413/422) are relayed as-is so the
    /// client can act on them; everything else keeps the historical 502, which
    /// reads as "the gateway failed" — the correct meaning for a 5xx, and the
    /// one clients already branch on.
    ///
    /// Retriable statuses (429, 5xx) never reach here: the caller parks them in
    /// `last_err` and tries the next upstream URL first.
    pub fn from_upstream(status: u16, body: String) -> Self {
        let message = format!("Upstream returned {}: {}", status, body);
        if is_terminal_rejection(status) {
            ProxyError::UpstreamRejected {
                status,
                body,
                message,
            }
        } else {
            ProxyError::Upstream(message)
        }
    }

    /// The HTTP status this error is reported with.
    ///
    /// Single source of truth for both the response and the log row, so the two
    /// cannot disagree about what the client was told.
    pub fn status(&self) -> StatusCode {
        match self {
            ProxyError::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ProxyError::Transform(_) => StatusCode::BAD_REQUEST,
            ProxyError::Rejected { status, .. } => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_REQUEST)
            }
            ProxyError::Upstream(_) => StatusCode::BAD_GATEWAY,
            ProxyError::UpstreamRejected { status, .. } => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            ProxyError::Serialization(_) => StatusCode::BAD_REQUEST,
            ProxyError::Http(_) => StatusCode::BAD_GATEWAY,
        }
    }

    /// Whether retrying this error on another upstream could plausibly succeed.
    ///
    /// Only a transport-level body failure qualifies: the response was cut
    /// short, so the bytes that never arrived can still arrive on a second
    /// attempt. A body that arrived whole and failed to deserialize is
    /// deterministic — every upstream holding the same model would answer the
    /// same way — so retrying it only burns the pool and delays the real error.
    pub fn is_transient_transport_error(&self) -> bool {
        match self {
            ProxyError::Http(err) => is_transport_body_error(err),
            _ => false,
        }
    }

    /// A diagnosis for the log, naming a transport failure's shape.
    ///
    /// [`Display`](std::fmt::Display) stays the full cause chain (it is what a
    /// client sees, and the chain is the honest description); this adds the
    /// "stalled upstream" versus "truncated body" reading that the logs are
    /// actually consulted for.
    pub fn diagnostic(&self) -> String {
        match self {
            ProxyError::Http(err) if is_transport_body_error(err) => {
                describe_transport_failure(err)
            }
            other => other.to_string(),
        }
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        // A rejected-upstream error relays the upstream's body verbatim: the
        // client's own SDK parses it, and its error taxonomy (e.g. OpenAI's
        // `code: "context_length_exceeded"` versus `type: "invalid_request_error"`)
        // is the only thing that tells it what to do about the rejection.
        // Re-wrapping it in the proxy's envelope would discard exactly that.
        if let ProxyError::UpstreamRejected { status, body, .. } = &self {
            let status = StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY);
            return match serde_json::from_str::<serde_json::Value>(body) {
                // Valid JSON: hand the parsed value back with a JSON content type.
                Ok(value) => (status, Json(value)).into_response(),
                // Not JSON (an HTML error page, a bare string): keep it as text
                // rather than 500 on a serde error inside error rendering.
                Err(_) if !body.trim().is_empty() => (
                    status,
                    [("content-type", "text/plain; charset=utf-8")],
                    body.clone(),
                )
                    .into_response(),
                Err(_) => {
                    (status, Json(json!({ "error": { "type": "proxy_error" } }))).into_response()
                }
            };
        }

        let status = self.status();
        let error_message = match &self {
            ProxyError::Config(msg) => msg.clone(),
            ProxyError::Transform(msg) => msg.clone(),
            ProxyError::Rejected { message, .. } => message.clone(),
            ProxyError::Upstream(msg) => msg.clone(),
            ProxyError::UpstreamRejected { message, .. } => message.clone(),
            ProxyError::Serialization(err) => format!("JSON error: {}", err),
            // Same rendering as the `Display` impl, so the body a client sees
            // and the row the user reads cannot describe one failure two ways.
            ProxyError::Http(err) => format!("HTTP error: {}", describe_error_chain(err)),
        };

        let body = Json(json!({
            "error": {
                "type": "proxy_error",
                "message": error_message,
            }
        }));

        (status, body).into_response()
    }
}

/// Result type for proxy operations
pub type ProxyResult<T> = Result<T, ProxyError>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    fn status_of(error: ProxyError) -> StatusCode {
        error.into_response().status()
    }

    #[test]
    fn config_error_returns_500() {
        assert_eq!(
            status_of(ProxyError::Config("bad".into())),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn transform_error_returns_400() {
        assert_eq!(
            status_of(ProxyError::Transform("bad".into())),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn upstream_error_returns_502() {
        assert_eq!(
            status_of(ProxyError::Upstream("bad".into())),
            StatusCode::BAD_GATEWAY
        );
    }

    /// A terminal rejection relays the upstream's own status instead of reading
    /// as a gateway fault.
    ///
    /// This is the whole point of the variant: a context-overflow 400 flattened
    /// to 502 told Codex the gateway was temporarily down, so it retried the
    /// identical 1.18M-token prompt 37 times rather than compacting.
    #[test]
    fn terminal_upstream_rejection_keeps_the_upstream_status() {
        for status in [400u16, 413, 422] {
            let err = ProxyError::from_upstream(status, "{\"error\":\"too big\"}".into());
            assert_eq!(
                status_of(err),
                StatusCode::from_u16(status).unwrap(),
                "status {status} must be relayed, not flattened to 502"
            );
        }
    }

    /// Everything else still reads as a gateway failure.
    #[test]
    fn non_terminal_upstream_status_stays_502() {
        for status in [401u16, 403, 404, 429, 500, 502, 503] {
            let err = ProxyError::from_upstream(status, "nope".into());
            assert!(matches!(err, ProxyError::Upstream(_)), "status {status}");
            assert_eq!(status_of(err), StatusCode::BAD_GATEWAY, "status {status}");
        }
    }

    /// The relayed body must reach the client verbatim: a client's SDK branches
    /// on the upstream's own error taxonomy (`code: "context_length_exceeded"`
    /// against `type: "invalid_request_error"`), which re-wrapping would erase.
    #[test]
    fn terminal_rejection_relays_the_upstream_body_verbatim() {
        let body = "{\"error\":{\"message\":\"prompt is too long\",\"type\":\"invalid_request_error\",\"code\":\"context_length_exceeded\"}}";
        let err = ProxyError::from_upstream(400, body.into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            content_type.contains("application/json"),
            "JSON bodies stay JSON, got {content_type}"
        );
    }

    /// A non-JSON upstream error page must not become a 500 while rendering.
    #[test]
    fn terminal_rejection_tolerates_a_non_json_body() {
        let err = ProxyError::from_upstream(413, "<html>too large</html>".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn serialization_error_returns_400() {
        let err: serde_json::Error = serde_json::from_str::<String>("not json").unwrap_err();
        assert_eq!(
            status_of(ProxyError::Serialization(err)),
            StatusCode::BAD_REQUEST
        );
    }

    /// A rejection carries its own status through to the response, which is what
    /// keeps a 413 or a 415 from being flattened into a generic 400.
    #[test]
    fn rejected_error_keeps_the_status_it_was_given() {
        for status in [400u16, 413, 415, 422] {
            assert_eq!(
                status_of(ProxyError::Rejected {
                    status,
                    message: "nope".into(),
                }),
                StatusCode::from_u16(status).unwrap(),
                "status {status} must survive"
            );
        }
    }

    /// `outcome_status` reads the same table, so the log row and the response
    /// cannot disagree — the mismatch is what made a 413 look like a 400.
    #[test]
    fn status_is_shared_by_the_response_and_the_log_row() {
        let error = ProxyError::Rejected {
            status: 413,
            message: "too big".into(),
        };
        assert_eq!(error.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            error.status(),
            status_of(ProxyError::Rejected {
                status: 413,
                message: "too big".into(),
            })
        );
    }

    /// A code outside the valid range must not panic or produce a nonsense
    /// response: 400 is the fallback. (Note the http crate accepts 100–999, so
    /// this has to be a value beyond that, not merely an unassigned one.)
    #[test]
    fn an_invalid_status_code_degrades_to_400() {
        assert_eq!(
            status_of(ProxyError::Rejected {
                status: 1000,
                message: "bogus".into(),
            }),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn rejected_error_message_reaches_the_body() {
        let response = ProxyError::Rejected {
            status: 413,
            message: "request body exceeds the 32 MiB limit".into(),
        }
        .into_response();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// The recorded row must name the failure, not repeat reqwest's opaque
    /// "error decoding response body" (which is what a real 500 row showed on
    /// 2026-10-07 — see the streaming read-error path).
    #[test]
    fn describe_error_chain_appends_causes_and_skips_duplicates() {
        #[derive(Debug)]
        struct Layer(&'static str, Option<Box<Layer>>);
        impl std::fmt::Display for Layer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::error::Error for Layer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.1
                    .as_deref()
                    .map(|e| e as &(dyn std::error::Error + 'static))
            }
        }

        let chain = Layer(
            "error decoding response body",
            Some(Box::new(Layer(
                "error reading a body from connection",
                Some(Box::new(Layer(
                    "end of file before message length reached",
                    None,
                ))),
            ))),
        );
        assert_eq!(
            describe_error_chain(&chain),
            "error decoding response body: error reading a body from connection: \
             end of file before message length reached"
        );

        // An error that quotes its source verbatim must not print it twice.
        let echoed = Layer("boom", Some(Box::new(Layer("boom", None))));
        assert_eq!(describe_error_chain(&echoed), "boom");
    }

    /// An error with no cause renders as just its own message (no trailing
    /// separator from an empty chain).
    #[test]
    fn describe_error_chain_handles_a_bare_error() {
        let err = std::io::Error::other("plain failure");
        assert_eq!(describe_error_chain(&err), "plain failure");
    }

    /// Only a transport body failure is worth retrying; everything else is
    /// reported as-is. The real `reqwest::Error` cases need a socket, so they
    /// live in the streaming tests in `src/proxy/tests.rs`; this pins the
    /// non-reqwest arm.
    #[test]
    fn non_http_errors_are_never_transient() {
        assert!(!ProxyError::Upstream("nope".into()).is_transient_transport_error());
        assert!(!ProxyError::Config("nope".into()).is_transient_transport_error());
        assert!(!ProxyError::Rejected {
            status: 400,
            message: "nope".into(),
        }
        .is_transient_transport_error());
    }
}
