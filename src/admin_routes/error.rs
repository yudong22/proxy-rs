//! ApiError — the one error type every handler returns.

use super::*;

/// A typed error that becomes the same JSON envelope `/api/*` uses.
pub struct ApiError {
    status: StatusCode,
    kind: &'static str,
    message: String,
}

impl ApiError {
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            kind: "internal",
            message: message.into(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            kind: "invalid_request",
            message: message.into(),
        }
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            kind: "not_found",
            message: message.into(),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        // The store layer's errors are mostly "not found" and "invalid input".
        // Matching on the message would be fragile; treating everything as 422
        // would hide real failures. 400 with the real text is the honest middle:
        // the operator sees exactly what went wrong and can act on it.
        Self {
            status: StatusCode::BAD_REQUEST,
            kind: "invalid_request",
            message: e.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": {
                    "type": self.kind,
                    "message": self.message,
                    "code": self.kind,
                }
            })),
        )
            .into_response()
    }
}
