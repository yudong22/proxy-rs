use serde::{Deserialize, Serialize};
use serde_json::Value;

/// OpenAI Responses API request format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    pub input: ResponsesInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponseTool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    /// Stable key identifying a reusable prompt prefix.
    ///
    /// Codex sends the session/thread id here. Upstreams use it to route
    /// requests that share a prefix to the same prompt cache, so dropping it
    /// silently removes cache reuse across turns of one conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesInput {
    Text(String),
    Items(Vec<ResponseInputItem>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseInputItem {
    FunctionCallOutput {
        #[serde(rename = "type")]
        item_type: String, // "function_call_output"
        call_id: String,
        output: Value,
    },
    FunctionCall {
        #[serde(rename = "type")]
        item_type: String, // "function_call"
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        call_id: Option<String>,
        name: String,
        arguments: String,
    },
    Message {
        #[serde(rename = "type", default)]
        item_type: Option<String>,
        #[serde(default)]
        id: Option<String>,
        role: String,
        content: ResponseMessageContent,
    },
    Raw(Value),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseMessageContent {
    Text(String),
    Parts(Vec<ResponseContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseContentPart {
    #[serde(rename = "type", default = "default_text_part_type")]
    pub part_type: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub image_url: Option<Value>,
}

fn default_text_part_type() -> String {
    "input_text".to_string()
}

fn default_function_type() -> String {
    "function".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseTool {
    #[serde(rename = "type", default = "default_function_type")]
    pub tool_type: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<Value>,
    #[serde(default)]
    pub function: Option<crate::models::openai::Function>,
    /// Nested tools carried by `namespace` groups (e.g. Codex `multi_agent_v1`).
    #[serde(default)]
    pub tools: Option<Vec<ResponseTool>>,
}

/// Non-streaming response format for OpenAI Responses API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesResponse {
    pub id: String,
    pub object: String,
    pub created_at: i64,
    pub status: String,
    pub model: String,
    pub output: Vec<OutputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResponsesUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItem {
    Message {
        id: String,
        status: String,
        role: String,
        content: Vec<OutputContentPart>,
    },
    FunctionCall {
        id: String,
        call_id: String,
        status: String,
        name: String,
        arguments: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputContentPart {
    OutputText { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ResponseContentItem>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseContentItem {
    #[serde(rename = "type")]
    pub content_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesUsage {
    pub total_tokens: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<crate::models::openai::PromptTokensDetails>,
}

/// Streaming event envelope for Server-Sent Events (SSE)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponsesStreamEvent {
    #[serde(rename = "response.created")]
    Created { response: ResponseStreamMetadata },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        response_id: String,
        output_index: usize,
        item: OutputItem,
    },
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded {
        response_id: String,
        item_id: String,
        output_index: usize,
        content_index: usize,
        part: OutputContentPart,
    },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        response_id: String,
        item_id: String,
        output_index: usize,
        content_index: usize,
        delta: String,
    },
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        response_id: String,
        item_id: String,
        output_index: usize,
        content_index: usize,
        text: String,
    },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        response_id: String,
        item_id: String,
        output_index: usize,
        call_id: String,
        delta: String,
    },
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {
        response_id: String,
        item_id: String,
        output_index: usize,
        call_id: String,
        arguments: String,
    },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        response_id: String,
        output_index: usize,
        item: OutputItem,
    },
    #[serde(rename = "response.completed")]
    Completed { response: ResponsesResponse },
    #[serde(rename = "response.failed")]
    Failed {
        response_id: String,
        error: StreamErrorDetail,
    },
}

impl ResponsesStreamEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Created { .. } => "response.created",
            Self::OutputItemAdded { .. } => "response.output_item.added",
            Self::ContentPartAdded { .. } => "response.content_part.added",
            Self::OutputTextDelta { .. } => "response.output_text.delta",
            Self::OutputTextDone { .. } => "response.output_text.done",
            Self::FunctionCallArgumentsDelta { .. } => "response.function_call_arguments.delta",
            Self::FunctionCallArgumentsDone { .. } => "response.function_call_arguments.done",
            Self::OutputItemDone { .. } => "response.output_item.done",
            Self::Completed { .. } => "response.completed",
            Self::Failed { .. } => "response.failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseStreamMetadata {
    pub id: String,
    pub object: String,
    pub status: String,
    pub model: String,
    pub output: Vec<OutputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResponsesUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamErrorDetail {
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}
