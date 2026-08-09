use serde::{Deserialize, Serialize};

/// OpenAI-style chat completion request.
///
/// Fields such as `temperature` and `max_tokens` are part of the OpenAI API
/// contract and may be sent by clients even when the current provider ignores
/// them.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub stream: bool,
    /// Optional DeepSeek session URL returned by a previous completion.
    /// When supplied, the gateway reuses the underlying DeepSeek chat session
    /// instead of creating a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_url: Option<String>,
    /// Enable DeepThink chain-of-thought reasoning.
    #[serde(default)]
    pub thinking: Option<bool>,
    /// Enable web search.
    #[serde(default)]
    pub search: Option<bool>,
    /// Tools the model may call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    /// Control which tool (if any) the model calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub stop: Option<Vec<String>>,
    #[serde(default)]
    pub presence_penalty: Option<f32>,
    #[serde(default)]
    pub frequency_penalty: Option<f32>,
    #[serde(default)]
    pub user: Option<String>,
    /// Structured-output mode. `Some(ResponseFormat { r#type: "json_object" })`
    /// asks the model to answer with a single valid JSON object. The web-UI
    /// gateway providers have no native JSON channel, so their
    /// `validate_request` rejects `json_object` outright (fail closed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
}

/// OpenAI-style `response_format` value.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResponseFormat {
    /// `"text"` (default) or `"json_object"`.
    #[serde(rename = "type")]
    pub r#type: String,
}

/// Message content can be a plain string or an OpenAI-style multimodal array.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatContent {
    String(String),
    Array(Vec<ContentPart>),
}

/// Deserialize message content, treating an explicit JSON `null` as an empty
/// string. OpenAI's chat API sends `"content": null` for assistant messages
/// that carry only `tool_calls`; the `ChatContent` untagged enum rejects
/// `null`, which broke the tool-call round-trip.
///
/// Semantically `null` means "no text content", which is exactly what an empty
/// `String` represents downstream, so this is lossless for text and
/// multimodal handling.
fn deserialize_content<'de, D>(deserializer: D) -> Result<ChatContent, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if let serde_json::Value::Null = value {
        Ok(ChatContent::String(String::new()))
    } else {
        ChatContent::deserialize(value).map_err(|e| serde::de::Error::custom(e.to_string()))
    }
}

impl ChatContent {
    /// Concatenate all text parts into a single string.
    pub fn as_text(&self) -> String {
        match self {
            ChatContent::String(s) => s.clone(),
            ChatContent::Array(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }

    /// Return the URLs of all image parts.
    pub fn image_urls(&self) -> Vec<String> {
        match self {
            ChatContent::String(_) => Vec::new(),
            ChatContent::Array(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::ImageUrl { image_url } => Some(image_url.url.clone()),
                    _ => None,
                })
                .collect(),
        }
    }

    /// Return the URLs of all file parts.
    pub fn file_urls(&self) -> Vec<String> {
        match self {
            ChatContent::String(_) => Vec::new(),
            ChatContent::Array(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::FileUrl { file_url } => Some(file_url.url.clone()),
                    _ => None,
                })
                .collect(),
        }
    }

    /// Approximate character length (used for token estimates).
    pub fn len(&self) -> usize {
        match self {
            ChatContent::String(s) => s.len(),
            ChatContent::Array(parts) => parts.iter().map(|p| p.len()).sum(),
        }
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for ChatContent {
    fn default() -> Self {
        ChatContent::String(String::new())
    }
}

impl From<String> for ChatContent {
    fn from(s: String) -> Self {
        ChatContent::String(s)
    }
}

impl From<&str> for ChatContent {
    fn from(s: &str) -> Self {
        ChatContent::String(s.to_string())
    }
}

/// A single part of a multimodal message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
    FileUrl { file_url: FileUrl },
}

impl ContentPart {
    fn len(&self) -> usize {
        match self {
            ContentPart::Text { text } => text.len(),
            ContentPart::ImageUrl { image_url } => image_url.url.len(),
            ContentPart::FileUrl { file_url } => file_url.url.len(),
        }
    }
}

/// OpenAI-style image URL reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// OpenAI-style file URL reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileUrl {
    pub url: String,
}

/// A single message in a chat completion conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(deserialize_with = "deserialize_content", default)]
    pub content: ChatContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Chain-of-thought reasoning text, returned when `thinking` was enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Search citations, returned when `search` was enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citations: Option<Vec<Citation>>,
    /// Tool calls generated by the assistant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// For `role: "tool"` messages, the id of the tool call being answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// A tool the model may call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub r#type: String,
    pub function: FunctionDefinition,
}

/// Function definition used in `tools`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// Choice of tool-calling behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    Mode(String),
    Named { r#type: String, function: NamedFunctionChoice },
}

/// Named function selection for `tool_choice`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamedFunctionChoice {
    pub name: String,
}

/// A single tool call generated by the assistant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub r#type: String,
    pub function: FunctionCall,
}

/// Function call payload inside a `ToolCall`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// A search citation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Citation {
    /// Citation number as shown in the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<i64>,
    /// Page title, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Source URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Snippet/summary, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// UTF-16 start offset of the citation span in the response text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ix: Option<i64>,
    /// UTF-16 end offset of the citation span in the response text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ix: Option<i64>,
}

/// Non-streaming chat completion response.
#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatCompletionChoice>,
    pub usage: Usage,
    /// DeepSeek web-chat URL for this conversation. Pass it back in the next
    /// request's `session_url` field to continue the same thread without
    /// resending the full message history.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionChoice {
    pub index: i32,
    pub message: ChatMessage,
    #[serde(rename = "finish_reason")]
    pub finish_reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Usage {
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
}

/// Streaming chat completion chunk.
#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
    /// DeepSeek web-chat URL for this conversation. Included in every chunk
    /// so streaming clients can capture it at any point.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChunkChoice {
    pub index: i32,
    pub delta: ChatMessageDelta,
    #[serde(rename = "finish_reason")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ChatMessageDelta {
    pub role: Option<String>,
    pub content: Option<String>,
    /// Chain-of-thought reasoning delta, separate from `content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Search citation delta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citations: Option<Vec<Citation>>,
    /// Tool calls generated by the assistant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// /v1/models response.
#[derive(Debug, Clone, Serialize)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<Model>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Model {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub owned_by: String,
}

impl ChatCompletionRequest {
    /// Validate the request without side effects.
    pub fn validate(&self) -> Result<(), crate::error::GatewayError> {
        if self.model.is_empty() {
            return Err(crate::error::GatewayError::BadRequest(
                "model is required".to_string(),
            ));
        }
        if self.messages.is_empty() {
            return Err(crate::error::GatewayError::BadRequest(
                "messages cannot be empty".to_string(),
            ));
        }
        for (i, msg) in self.messages.iter().enumerate() {
            if msg.role.is_empty() {
                return Err(crate::error::GatewayError::BadRequest(format!(
                    "message[{i}].role cannot be empty"
                )));
            }
        }

        if let Some(ref v) = self.temperature {
            if !(0.0..=2.0).contains(v) {
                return Err(crate::error::GatewayError::BadRequest(
                    "temperature must be between 0 and 2".to_string(),
                ));
            }
        }
        if let Some(ref v) = self.top_p {
            if !(0.0..=1.0).contains(v) {
                return Err(crate::error::GatewayError::BadRequest(
                    "top_p must be between 0 and 1".to_string(),
                ));
            }
        }
        if let Some(ref v) = self.presence_penalty {
            if !(-2.0..=2.0).contains(v) {
                return Err(crate::error::GatewayError::BadRequest(
                    "presence_penalty must be between -2 and 2".to_string(),
                ));
            }
        }
        if let Some(ref v) = self.frequency_penalty {
            if !(-2.0..=2.0).contains(v) {
                return Err(crate::error::GatewayError::BadRequest(
                    "frequency_penalty must be between -2 and 2".to_string(),
                ));
            }
        }
        if let Some(v) = self.max_tokens {
            if v < 1 {
                return Err(crate::error::GatewayError::BadRequest(
                    "max_tokens must be >= 1".to_string(),
                ));
            }
        }
        if let Some(ref v) = self.stop {
            if v.is_empty() {
                return Err(crate::error::GatewayError::BadRequest(
                    "stop must be a non-empty array".to_string(),
                ));
            }
        }

        if let Some(ref fmt) = self.response_format {
            match fmt.r#type.as_str() {
                "text" | "json_object" => {}
                other => {
                    return Err(crate::error::GatewayError::BadRequest(format!(
                        "unsupported response_format type '{other}' (supported: text, json_object)"
                    )));
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_chat_request() {
        let json = serde_json::json!({
            "model": "deepseek-chat",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true
        });
        let req: ChatCompletionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.model, "deepseek-chat");
        assert!(req.stream);
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].content.as_text(), "hello");
    }

    #[test]
    fn deserialize_multimodal_request() {
        let json = serde_json::json!({
            "model": "deepseek-vision",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "describe"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,abc"}}
                ]
            }]
        });
        let req: ChatCompletionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.messages[0].content.as_text(), "describe");
        assert_eq!(
            req.messages[0].content.image_urls(),
            vec!["data:image/png;base64,abc"]
        );
    }

    #[test]
    fn serialize_string_content_as_string() {
        let msg = ChatMessage {
            role: "user".to_string(),
            content: ChatContent::String("hello".to_string()),
            name: None,
            reasoning_content: None,
            citations: None,
            tool_calls: None,
            tool_call_id: None,
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert!(s.contains("\"content\":\"hello\""));
    }

    #[test]
    fn deserialize_null_content_for_tool_call_round_trip() {
        // OpenAI's chat API emits "content": null on assistant messages that
        // only carry tool_calls. This must deserialize (and default to an
        // empty string) instead of erroring, so tool-calling round-trips work.
        let json = serde_json::json!({
            "model": "deepseek-chat",
            "messages": [
                {"role": "user", "content": "weather in Paris"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_x",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
                }]},
                {"role": "tool", "tool_call_id": "call_x", "content": "sunny, 22C"}
            ]
        });
        let req: ChatCompletionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.messages.len(), 3);
        assert_eq!(req.messages[1].content.as_text(), "");
        let tc = req.messages[1].tool_calls.as_ref().unwrap();
        assert_eq!(tc[0].function.name, "get_weather");
        assert_eq!(req.messages[2].tool_call_id.as_deref(), Some("call_x"));
    }

    #[test]
    fn validation_rejects_empty_messages() {
        let req = ChatCompletionRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![],
            stream: false,
            session_url: None,
            thinking: None,
            search: None,
            tools: None,
            tool_choice: None,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            presence_penalty: None,
            frequency_penalty: None,
            user: None,
            response_format: None,
        };
        assert!(req.validate().is_err());
    }
}
