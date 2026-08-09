//! Response parsing for `chat.z.ai`.
//!
//! The site returns the assistant's reply as an SSE stream. The exact wire
//! format varies by frontend version, so the parser is intentionally tolerant
//! and tries multiple known shapes.

use crate::error::GatewayError;
use crate::models::{Citation, ToolCall};

/// Structured data extracted from a `chat.z.ai` response.
#[derive(Debug, Clone, Default)]
pub struct ResponseData {
    pub text: String,
    pub thinking: Option<String>,
    pub citations: Option<Vec<Citation>>,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub chat_id: Option<String>,
}

/// Parse a captured response body into structured data.
///
/// Tries, in order:
/// 1. OpenAI-style SSE (`choices[0].delta.content`).
/// 2. chat.z.ai native SSE (`content`, `delta_content`, `edit_content`).
/// 3. A plain JSON object.
/// 4. Plain text fallback.
pub fn parse_response_body(body: &[u8], model: &str) -> Result<ResponseData, GatewayError> {
    let text = String::from_utf8_lossy(body);
    let mut data = ResponseData::default();

    // SSE path.
    if text.contains("data:") {
        for line in text.lines() {
            let line = line.trim();
            if !line.starts_with("data:") {
                continue;
            }
            let payload = line["data:".len()..].trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            let Ok(json) = serde_json::from_str::<serde_json::Value>(payload) else {
                continue;
            };

            // OpenAI-style chunk.
            if let Some(delta) = json
                .get("choices")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|v| v.get("delta"))
                .or_else(|| json.get("delta"))
            {
                if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                    data.text.push_str(content);
                }
                if let Some(reasoning) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                    data.thinking
                        .get_or_insert_with(String::new)
                        .push_str(reasoning);
                }
                if let Some(calls) = delta.get("tool_calls") {
                    if let Some(parsed) = parse_tool_calls(calls) {
                        data.tool_calls = Some(parsed);
                    }
                }
            }

            // Native chat.z.ai fields.
            if let Some(content) = json.get("content").and_then(|v| v.as_str()) {
                data.text.push_str(content);
            }
            if let Some(content) = json.get("delta_content").and_then(|v| v.as_str()) {
                data.text.push_str(content);
            }
            if let Some(content) = json.get("edit_content").and_then(|v| v.as_str()) {
                data.text.push_str(content);
            }
            if let Some(reasoning) = json.get("reasoning_content").and_then(|v| v.as_str()) {
                data.thinking
                    .get_or_insert_with(String::new)
                    .push_str(reasoning);
            }

            // Chat/session id.
            if let Some(chat_id) = json
                .get("chat_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
            {
                data.chat_id = Some(chat_id);
            }
        }
        return Ok(data);
    }

    // Plain JSON object fallback.
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
        if let Some(content) = json.get("content").and_then(|v| v.as_str()) {
            data.text = content.to_string();
            return Ok(data);
        }
        if let Some(text_field) = json.get("text").and_then(|v| v.as_str()) {
            data.text = text_field.to_string();
            return Ok(data);
        }
    }

    // Plain text fallback: this should rarely happen, but it keeps the provider
    // from failing when the upstream returns an unexpected body.
    if !text.trim().is_empty() {
        data.text = text.trim().to_string();
        return Ok(data);
    }

    Err(GatewayError::Provider(format!(
        "GLM model {model} returned an empty or unparseable response"
    )))
}

fn parse_tool_calls(value: &serde_json::Value) -> Option<Vec<ToolCall>> {
    let arr = value.as_array()?;
    let mut calls = Vec::new();
    for item in arr {
        let obj = item.as_object()?;
        let id = obj.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let name = obj
            .get("function")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .or_else(|| obj.get("name").and_then(|v| v.as_str()))?
            .to_string();
        let arguments = obj
            .get("function")
            .and_then(|v| v.get("arguments"))
            .and_then(|v| v.as_str())
            .or_else(|| obj.get("arguments").and_then(|v| v.as_str()))
            .unwrap_or("{}")
            .to_string();
        calls.push(ToolCall {
            id,
            r#type: "function".to_string(),
            function: crate::models::FunctionCall { name, arguments },
        });
    }
    if calls.is_empty() {
        None
    } else {
        Some(calls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_openai_style_sse() {
        let body = br#"data: {"choices":[{"delta":{"role":"assistant","content":"Hello"}}]}

data: {"choices":[{"delta":{"content":" world"}}]}

data: [DONE]
"#;
        let data = parse_response_body(body, "glm-5.2").unwrap();
        assert_eq!(data.text, "Hello world");
        assert!(data.thinking.is_none());
    }

    #[test]
    fn parse_native_sse() {
        let body = br#"data: {"content":"Hi"}

data: {"content":" there"}
"#;
        let data = parse_response_body(body, "glm-5.2").unwrap();
        assert_eq!(data.text, "Hi there");
    }

    #[test]
    fn parse_reasoning_sse() {
        let body = br#"data: {"delta":{"reasoning_content":"thinking","content":" answer"}}
"#;
        let data = parse_response_body(body, "glm-5.2").unwrap();
        assert_eq!(data.text, " answer");
        assert_eq!(data.thinking, Some("thinking".to_string()));
    }

    #[test]
    fn parse_plain_json() {
        let body = br#"{"content":"plain json"}"#;
        let data = parse_response_body(body, "glm-5.2").unwrap();
        assert_eq!(data.text, "plain json");
    }
}
