//! GLM/Z.AI internal API request building and SSE response parsing.

use crate::models::{ChatCompletionRequest, ChatMessage, Citation, ToolCall};

use super::models::GlmModelDef;

/// Frontend version expected by the upstream internal API. This value is
/// rotated by Z.AI periodically; if direct requests start failing with
/// signature/validation errors, check the latest value from a live browser
/// request.
// Must match the current deployed SPA version on https://chat.z.ai.
// The Z.AI backend checks this header and rejects outdated clients with
// "请刷新页面以更新应用后重试".  Bump this when the SPA is redeployed.
// In the long run this should be extracted from the initial HTML.
pub const X_FE_VERSION: &str = "prod-fe-1.1.84";

/// Capabilities toggles for the hidden MCP feature list.
#[derive(Debug, Clone)]
pub struct FeatureToggles {
    pub thinking: bool,
    pub search: bool,
}

/// Reference to a file uploaded to Z.AI.
#[derive(Debug, Clone)]
pub struct UploadedFile {
    /// Wire reference, usually `{id}_{filename}`.
    pub reference: String,
    /// Original part type (`image_url` or `file_url`).
    pub part_type: String,
}

/// Build the JSON body for the v2 chat/completions endpoint.
#[allow(clippy::too_many_arguments)]
pub fn build_completion_body(
    request: &ChatCompletionRequest,
    model: &GlmModelDef,
    chat_id: &str,
    request_id: &str,
    last_user_text: &str,
    files: &[UploadedFile],
    features: FeatureToggles,
    internal_model_id: &str,
) -> serde_json::Value {
    let is_thinking_model = model.id.contains("thinking") || model.id.contains("think");
    let is_search_model = model.id.contains("search");
    let enable_thinking = request.thinking.unwrap_or(false) || is_thinking_model || features.thinking;
    let web_search = request.search.unwrap_or(false) || is_search_model || features.search;

    let mut params = serde_json::Map::new();
    if let Some(t) = request.temperature {
        params.insert("temperature".to_string(), serde_json::Number::from_f64(t as f64).unwrap_or(serde_json::Number::from(0)).into());
    }
    if let Some(p) = request.top_p {
        params.insert("top_p".to_string(), serde_json::Number::from_f64(p as f64).unwrap_or(serde_json::Number::from(0)).into());
    }
    if let Some(m) = request.max_tokens {
        params.insert("max_tokens".to_string(), serde_json::Number::from(m).into());
    }
    if let Some(ref s) = request.stop {
        params.insert("stop".to_string(), serde_json::json!(s));
    }
    if let Some(p) = request.presence_penalty {
        params.insert("presence_penalty".to_string(), serde_json::Number::from_f64(p as f64).unwrap_or(serde_json::Number::from(0)).into());
    }
    if let Some(f) = request.frequency_penalty {
        params.insert("frequency_penalty".to_string(), serde_json::Number::from_f64(f as f64).unwrap_or(serde_json::Number::from(0)).into());
    }

    let messages = request
        .messages
        .iter()
        .map(|m| convert_message(m, files))
        .collect::<Vec<_>>();

    let body = serde_json::json!({
        "stream": true,
        "model": internal_model_id,
        "messages": messages,
        "signature_prompt": last_user_text,
        "files": files.iter().map(|f| &f.reference).collect::<Vec<_>>(),
        "params": params,
        "extra": {},
        "features": {
            "image_generation": false,
            "web_search": web_search,
            "auto_web_search": web_search,
            "preview_mode": false,
            "flags": [],
            "features": [
                { "type": "mcp", "server": "vibe-coding", "status": "hidden" },
                { "type": "mcp", "server": "ppt-maker", "status": "hidden" },
                { "type": "mcp", "server": "image-search", "status": "hidden" },
                { "type": "mcp", "server": "deep-research", "status": "hidden" },
                { "type": "tool_selector", "server": "tool_selector", "status": "hidden" },
                { "type": "mcp", "server": "advanced-search", "status": "hidden" },
            ],
            "enable_thinking": enable_thinking,
        },
        "background_tasks": {
            "title_generation": false,
            "tags_generation": false,
        },
        "mcp_servers": [],
        "variables": {
            "{{USER_NAME}}": "Guest",
            "{{USER_LOCATION}}": "Unknown",
            "{{CURRENT_DATETIME}}": current_iso_datetime(),
            "{{CURRENT_DATE}}": current_iso_date(),
            "{{CURRENT_TIME}}": current_iso_time(),
            "{{CURRENT_WEEKDAY}}": current_weekday(),
            "{{CURRENT_TIMEZONE}}": "Asia/Shanghai",
            "{{USER_LANGUAGE}}": "zh-CN",
        },
        "chat_id": chat_id,
        "id": request_id,
        "session_id": uuid::Uuid::new_v4().to_string(),
        "current_user_message_id": request_id,
        "current_user_message_parent_id": serde_json::Value::Null,
    });

    // OpenAI `tools`/`tool_choice` are NEVER forwarded upstream: the MTP
    // pipeline compiles them into the prompted tool protocol instead
    // (strip_upstream_tools is a hard gateway invariant). The Z.AI endpoint's
    // native tool fields trigger its own built-in tools, which the MTP policy
    // explicitly forbids the model from using.

    body
}

/// Convert an OpenAI-style message to the upstream format, replacing any
/// uploaded attachment URLs with the uploaded file references.
fn convert_message(msg: &ChatMessage, files: &[UploadedFile]) -> serde_json::Value {
    match &msg.content {
        crate::models::ChatContent::String(text) => serde_json::json!({
            "role": msg.role,
            "content": text,
        }),
        crate::models::ChatContent::Array(parts) => {
            let parts: Vec<_> = parts
                .iter()
                .map(|p| match p {
                    crate::models::ContentPart::Text { text } => {
                        serde_json::json!({"type": "text", "text": text})
                    }
                    crate::models::ContentPart::ImageUrl { image_url } => {
                        let url = find_upload_reference(&image_url.url, "image_url", files)
                            .unwrap_or_else(|| image_url.url.clone());
                        serde_json::json!({"type": "image_url", "image_url": {"url": url}})
                    }
                    crate::models::ContentPart::FileUrl { file_url } => {
                        let url = find_upload_reference(&file_url.url, "file_url", files)
                            .unwrap_or_else(|| file_url.url.clone());
                        serde_json::json!({"type": "file_url", "file_url": {"url": url}})
                    }
                })
                .collect();
            serde_json::json!({"role": msg.role, "content": parts})
        }
    }
}

fn find_upload_reference(original_url: &str, part_type: &str, files: &[UploadedFile]) -> Option<String> {
    files
        .iter()
        .find(|f| f.part_type == part_type && original_url.ends_with(&f.reference))
        .map(|f| f.reference.clone())
        .or_else(|| {
            // If the original URL is already a short uploaded reference, keep it.
            if !original_url.starts_with("data:") && !original_url.starts_with("http") {
                Some(original_url.to_string())
            } else {
                None
            }
        })
}

/// Extract the text of the last user message, used for the signature and the
/// `signature_prompt` field.
pub fn last_user_text(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.as_text())
        .unwrap_or_default()
}

/// A single parsed upstream SSE event.
#[derive(Debug, Clone, Default)]
pub struct UpstreamEvent {
    pub content_delta: Option<String>,
    pub edit_delta: Option<String>,
    pub reasoning_delta: Option<String>,
    pub citations: Option<Vec<Citation>>,
    #[allow(dead_code)]
    pub tool_calls: Option<Vec<ToolCall>>,
    pub done: bool,
    pub error: Option<String>,
    /// Application error code (e.g. `FRONTEND_CAPTCHA_REQUIRED`). Set
    /// alongside `error` when the upstream returns a structured error.
    pub error_code: Option<String>,
}

/// Parse one SSE line into an event, if it contains data.
pub fn parse_sse_line(line: &str) -> Option<UpstreamEvent> {
    let line = line.trim();
    if !line.starts_with("data:") {
        return None;
    }
    let payload = line["data:".len()..].trim();
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(payload).ok()?;

    // v2 wraps events in {"type":"chat:completion","data":{...}}
    let data = if json.get("type").and_then(|v| v.as_str()) == Some("chat:completion") {
        json.get("data").cloned().unwrap_or(json)
    } else {
        json
    };

    let mut event = UpstreamEvent::default();

    if let Some(err) = extract_error(&data) {
        event.error = Some(err.0);
        event.error_code = err.1;
        return Some(event);
    }

    if let Some(content) = data.get("delta_content").and_then(|v| v.as_str()) {
        event.content_delta = Some(content.to_string());
    }
    if let Some(content) = data.get("edit_content").and_then(|v| v.as_str()) {
        event.edit_delta = Some(content.to_string());
    }

    let phase = data.get("phase").and_then(|v| v.as_str());
    if phase == Some("thinking") {
        if let Some(content) = data.get("delta_content").and_then(|v| v.as_str()) {
            event.reasoning_delta = Some(content.to_string());
            event.content_delta = None;
        }
    }

    if data.get("done").and_then(|v| v.as_bool()).unwrap_or(false) || phase == Some("done") {
        event.done = true;
    }

    if let Some(citations) = extract_citations(&data) {
        event.citations = Some(citations);
    }

    Some(event)
}

fn extract_error(data: &serde_json::Value) -> Option<(String, Option<String>)> {
    for path in ["/error", "/inner/error"] {
        if let Some(obj) = data.pointer(path) {
            let detail = obj
                .get("detail")
                .and_then(|v| v.as_str())
                .or_else(|| obj.get("message").and_then(|v| v.as_str()))
                .map(|s| s.to_string());
            if let Some(detail) = detail {
                let code = obj
                    .get("code")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                return Some((detail, code));
            }
        }
    }
    None
}

fn extract_citations(data: &serde_json::Value) -> Option<Vec<Citation>> {
    for key in ["citations", "search_results", "references", "results"] {
        if let Some(arr) = data.get(key).and_then(|v| v.as_array()) {
            let citations: Vec<_> = arr
                .iter()
                .enumerate()
                .filter_map(|(i, v)| parse_citation(v, Some(i as i64 + 1)))
                .collect();
            if !citations.is_empty() {
                return Some(citations);
            }
        }
    }
    None
}

fn parse_citation(value: &serde_json::Value, fallback_index: Option<i64>) -> Option<Citation> {
    let obj = value.as_object()?;
    let url = obj
        .get("url")
        .or_else(|| obj.get("link"))
        .or_else(|| obj.get("href"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let title = obj
        .get("title")
        .or_else(|| obj.get("name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let snippet = obj
        .get("snippet")
        .or_else(|| obj.get("summary"))
        .or_else(|| obj.get("content"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let index = obj
        .get("cite_index")
        .and_then(|v| v.as_i64())
        .or_else(|| obj.get("index").and_then(|v| v.as_i64()))
        .or(fallback_index);

    if url.is_none() && title.is_none() && snippet.is_none() {
        return None;
    }
    Some(Citation {
        index,
        title,
        url,
        snippet,
        start_ix: None,
        end_ix: None,
    })
}

/// Current ISO-8601 datetime with space separator (matches upstream examples).
fn current_iso_datetime() -> String {
    let now = std::time::SystemTime::now();
    let dt = chrono::DateTime::<chrono::Utc>::from(now);
    dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

fn current_iso_date() -> String {
    let now = std::time::SystemTime::now();
    chrono::DateTime::<chrono::Utc>::from(now).format("%Y-%m-%d").to_string()
}

fn current_iso_time() -> String {
    let now = std::time::SystemTime::now();
    chrono::DateTime::<chrono::Utc>::from(now).format("%H:%M:%S").to_string()
}

fn current_weekday() -> String {
    let now = std::time::SystemTime::now();
    chrono::DateTime::<chrono::Utc>::from(now).format("%A").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ChatMessage;

    #[test]
    fn last_user_text_concatenates_parts() {
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: crate::models::ChatContent::String("hello".to_string()),
            name: None,
            reasoning_content: None,
            citations: None,
            tool_calls: None,
            tool_call_id: None,
        }];
        assert_eq!(last_user_text(&messages), "hello");
    }

    #[test]
    fn parse_v2_sse_event() {
        let line = r#"data: {"type":"chat:completion","data":{"delta_content":"Hello","phase":"answer"}}"#;
        let event = parse_sse_line(line).unwrap();
        assert_eq!(event.content_delta, Some("Hello".to_string()));
        assert!(!event.done);
    }

    #[test]
    fn parse_v2_thinking_event() {
        let line = r#"data: {"type":"chat:completion","data":{"delta_content":"think","phase":"thinking"}}"#;
        let event = parse_sse_line(line).unwrap();
        assert_eq!(event.reasoning_delta, Some("think".to_string()));
        assert!(event.content_delta.is_none());
    }

    #[test]
    fn parse_done_event() {
        let line = r#"data: {"type":"chat:completion","data":{"done":true,"phase":"done"}}"#;
        let event = parse_sse_line(line).unwrap();
        assert!(event.done);
    }

    #[test]
    fn parse_error_event() {
        let line = r#"data: {"type":"chat:completion","data":{"error":{"detail":"captcha required"}}}"#;
        let event = parse_sse_line(line).unwrap();
        assert_eq!(event.error, Some("captcha required".to_string()));
    }
}
