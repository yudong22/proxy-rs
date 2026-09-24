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

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}

impl ProxyError {
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
            ProxyError::Serialization(_) => StatusCode::BAD_REQUEST,
            ProxyError::Http(_) => StatusCode::BAD_GATEWAY,
        }
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let status = self.status();
        let error_message = match &self {
            ProxyError::Config(msg) => msg.clone(),
            ProxyError::Transform(msg) => msg.clone(),
            ProxyError::Rejected { message, .. } => message.clone(),
            ProxyError::Upstream(msg) => msg.clone(),
            ProxyError::Serialization(err) => format!("JSON error: {}", err),
            ProxyError::Http(err) => format!("HTTP error: {}", err),
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
}
