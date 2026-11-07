//! Which wire protocol a request arrived on.

/// Which wire protocol a request was accepted on. The three supported APIs
/// share the entire upstream send/retry path and differ only in how the request
/// was translated and how the response is rendered back.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiFlavor {
    /// Anthropic Messages API (`/v1/messages`).
    Anthropic,
    /// OpenAI Responses API (`/v1/responses`).
    Responses,
    /// OpenAI Chat Completions passthrough (`/v1/chat/completions`).
    Chat,
}

impl ApiFlavor {
    /// Log stage tag used for upstream failures.
    pub(crate) fn stage(self) -> &'static str {
        match self {
            ApiFlavor::Anthropic | ApiFlavor::Chat => "chat/completions",
            ApiFlavor::Responses => "responses/chat_completions",
        }
    }

    /// Human-readable name used in `tracing` debug lines.
    pub(crate) fn label(self) -> &'static str {
        match self {
            ApiFlavor::Anthropic => "",
            ApiFlavor::Responses => "Responses API ",
            ApiFlavor::Chat => "Chat Completions ",
        }
    }

    /// Whether SSE responses advertise CORS. The Anthropic endpoint is consumed
    /// by CLI clients; the OpenAI-compatible ones are also called from browsers.
    pub(crate) fn sse_allows_cors(self) -> bool {
        !matches!(self, ApiFlavor::Anthropic)
    }
}
