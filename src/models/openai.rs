use serde::{Deserialize, Serialize};
use serde_json::Value;

/// OpenAI API request structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Option<MessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: Function,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
}

/// Deserialize `T`, treating an explicit JSON `null` as `T::default()`.
///
/// `#[serde(default)]` only covers a *missing* key. An upstream that sends
/// `"usage": null` still fails the field, which on the non-streaming path
/// becomes a 500 for an otherwise valid response.
fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// OpenAI API response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIResponse {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub created: Option<u64>,
    #[serde(default)]
    pub model: Option<String>,
    pub choices: Vec<Choice>,
    #[serde(default, deserialize_with = "null_to_default")]
    pub usage: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: usize,
    pub message: ChoiceMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    // Providers omit token fields freely (some send only `total_tokens`).
    // Without a default, a single missing field fails the whole response: on the
    // non-streaming path that becomes a 500, and on the streaming path
    // `serde_json::from_str::<StreamChunk>` returns Err, which is swallowed —
    // the usage chunk is dropped and the request is recorded with zero tokens.
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    /// OpenAI-compatible breakout of cached prompt tokens, if the upstream
    /// reports it. Mapped to Anthropic `cache_read_input_tokens`.
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// Some providers report cache breakouts at the top level.
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u32>,
    /// DeepSeek official format: prompt_cache_hit_tokens
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u32>,
    /// DeepSeek official format: prompt_cache_miss_tokens
    #[serde(default)]
    pub prompt_cache_miss_tokens: Option<u32>,
    /// Top-level cached_tokens reported by some providers
    #[serde(default)]
    pub cached_tokens: Option<u32>,
}

impl Usage {
    /// Cached prompt tokens.
    ///
    /// Providers fill whichever field their lineage uses and often set the
    /// others to a literal `0` rather than omitting them — WorkBuddy returns
    /// `cache_read_input_tokens: 0` beside the real `prompt_cache_hit_tokens`.
    /// Taking the first `Some` would latch onto that zero and report a 0% cache
    /// hit rate even on a 91% hit, so zero is treated as absent.
    pub fn cache_read_tokens(&self) -> i64 {
        [
            self.cache_read_input_tokens,
            self.prompt_cache_hit_tokens,
            self.cached_tokens,
            self.prompt_tokens_details.as_ref().map(|d| d.cached_tokens),
        ]
        .into_iter()
        .flatten()
        .filter(|v| *v > 0)
        .max()
        .unwrap_or(0) as i64
    }

    pub fn cache_write_tokens(&self) -> i64 {
        self.cache_creation_input_tokens.unwrap_or(0) as i64
    }

    pub fn uncached_input_tokens(&self) -> i64 {
        let read = self.cache_read_tokens();
        let write = self.cache_write_tokens();
        (self.prompt_tokens as i64)
            .saturating_sub(read)
            .saturating_sub(write)
            .max(0)
    }

    pub fn to_token_record(&self) -> crate::stats::TokenRecord {
        crate::stats::TokenRecord {
            input: self.uncached_input_tokens(),
            cache_read: self.cache_read_tokens(),
            cache_write: self.cache_write_tokens(),
            output: self.completion_tokens as i64,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u32,
    #[serde(default)]
    pub audio_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsListResponse {
    #[serde(default)]
    pub object: Option<String>,
    pub data: Vec<ModelInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub created: Option<u64>,
    #[serde(default)]
    pub owned_by: Option<String>,
}

/// Streaming chunk structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunk {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub created: Option<u64>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub choices: Vec<StreamChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChoice {
    pub index: usize,
    pub delta: Delta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<DeltaToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaToolCall {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub call_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<DeltaFunctionCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaFunctionCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    #[test]
    fn response_accepts_a_usage_object_that_omits_token_fields() {
        // Some providers send only `total_tokens`. This must not fail the whole
        // response — it used to become a 500 on the non-streaming path.
        let json = r#"{
            "choices": [{"index":0,"message":{"role":"assistant","content":"hi"}}],
            "usage": {"total_tokens": 42}
        }"#;
        let parsed: OpenAIResponse = serde_json::from_str(json).expect("usage must deserialize");
        assert_eq!(parsed.usage.total_tokens, 42);
        assert_eq!(parsed.usage.prompt_tokens, 0);
        assert_eq!(parsed.usage.completion_tokens, 0);
    }

    #[test]
    fn response_accepts_an_explicit_null_usage() {
        let json = r#"{
            "choices": [{"index":0,"message":{"role":"assistant","content":"hi"}}],
            "usage": null
        }"#;
        let parsed: OpenAIResponse =
            serde_json::from_str(json).expect("explicit null usage must deserialize");
        assert_eq!(parsed.usage.total_tokens, 0);
    }

    #[test]
    fn response_accepts_a_missing_usage_key() {
        let json = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"hi"}}]}"#;
        let parsed: OpenAIResponse = serde_json::from_str(json).expect("missing usage is fine");
        assert_eq!(parsed.usage.prompt_tokens, 0);
    }

    #[test]
    fn stream_chunk_usage_survives_a_partial_object() {
        // The streaming path parses StreamChunk; a usage frame missing
        // `prompt_tokens` used to be dropped silently, recording zero tokens.
        let json = r#"{"choices":[],"usage":{"completion_tokens":5}}"#;
        let chunk: StreamChunk = serde_json::from_str(json).expect("partial usage must parse");
        let usage = chunk.usage.expect("usage present");
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.prompt_tokens, 0);
    }
}
