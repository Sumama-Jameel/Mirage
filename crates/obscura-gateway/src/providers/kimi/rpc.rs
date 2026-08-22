use crate::models::ChatCompletionRequest;
use crate::models::ChatMessage;
use crate::models::Citation;
use crate::models::ToolCall;

use super::models::KimiModelDef;
use super::state::StoredConversation;

/// Build the Kimi chat completion request payload.
///
/// Payload format matches the kimi.moonshot.cn internal API:
/// - `kimiplus_id`: the assistant/kimiplus identifier
/// - `messages`: array of user messages
/// - `use_search` / `use_research` / `use_math`: feature flags
/// - `refs`: file reference IDs
/// - `refs_file`: image reference IDs
/// - `extend`: metadata flags
pub fn build_request_payload(
    messages: &[ChatMessage],
    conv: Option<&StoredConversation>,
    model_def: &KimiModelDef,
    search: bool,
    request: &ChatCompletionRequest,
    refs: &[String],          // file refs (non-image)
    refs_file: &[String],     // image refs
) -> serde_json::Value {
    let has_any = !refs.is_empty() || !refs_file.is_empty();
    let mut chat_messages: Vec<serde_json::Value> = messages
        .iter()
        .map(|msg| {
            let mut content = msg.content.as_text();
            // Append inline markers for files and images so the model
            // understands what content is attached.
            if has_any && msg.role == "user" {
                for ref_id in refs {
                    if !content.contains(ref_id) {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(&format!("[file: {}]", ref_id));
                    }
                }
                for ref_id in refs_file {
                    if !content.contains(ref_id) {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(&format!("[image: {}]", ref_id));
                    }
                }
            }
            serde_json::json!({
                "role": msg.role,
                "content": content,
            })
        })
        .collect();

    if chat_messages.is_empty() {
        chat_messages.push(serde_json::json!({
            "role": "user",
            "content": "Hello",
        }));
    }

    let mut payload = serde_json::json!({
        "kimiplus_id": model_def.kimiplus_id,
        "messages": chat_messages,
        "refs": refs,
        "refs_file": refs_file,
        "use_search": search || model_def.use_search,
        "use_research": model_def.use_research,
        "use_math": model_def.use_math,
        "extend": {
            "sidebar": true,
        },
    });

    if let Some(conv) = conv {
        if let Some(ref segment_id) = conv.segment_id {
            payload["segment_id"] = serde_json::json!(segment_id);
            payload["action"] = serde_json::json!("continue");
        }
    }

    if let Some(t) = request.temperature {
        payload["temperature"] = serde_json::json!(t);
    }
    if let Some(m) = request.max_tokens {
        payload["max_tokens"] = serde_json::json!(m);
    }
    if let Some(p) = request.top_p {
        payload["top_p"] = serde_json::json!(p);
    }
    if let Some(ref s) = request.stop {
        payload["stop"] = serde_json::json!(s);
    }
    if let Some(p) = request.presence_penalty {
        payload["presence_penalty"] = serde_json::json!(p);
    }
    if let Some(f) = request.frequency_penalty {
        payload["frequency_penalty"] = serde_json::json!(f);
    }

    if let Some(ref tools) = request.tools {
        payload["tools"] = serde_json::json!(tools);
    }
    if let Some(ref tool_choice) = request.tool_choice {
        payload["tool_choice"] = serde_json::json!(tool_choice);
    }
    payload["use_reasoning"] = serde_json::json!(request.thinking.unwrap_or(model_def.is_thinking));

    payload
}

/// Parse a single SSE line from Kimi's streaming response.
///
/// Kimi uses a custom event-based SSE format (NOT OpenAI-standard):
/// - `data: {"event": "cmpl", "text": "...", "reasoning": "..."}` → content + thinking delta
/// - `data: {"event": "req", "id": "..."}` → segment/request ID
/// - `data: {"event": "length"}` → max tokens reached
/// - `data: {"event": "search_plus", ...}` → search result metadata
/// - `data: {"event": "all_done"}` → stream complete
/// - `data: {"event": "error"}` → content policy violation
///
/// Returns `(delta_text, citations, segment_id, is_done, is_length, is_error, thinking_delta, tool_calls)`.
pub fn parse_sse_line(
    line: &str,
) -> Option<(Option<String>, Option<Vec<Citation>>, Option<String>, bool, bool, bool, Option<String>, Option<Vec<ToolCall>>)> {
    let body = line.strip_prefix("data: ")?;

    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return None;
    };
    let obj = json.as_object()?;

    let event = obj.get("event").and_then(|v| v.as_str())?;

    match event {
        "cmpl" => {
            let text = obj.get("text").and_then(|v| v.as_str()).map(|s| s.to_string());
            let reasoning = obj.get("reasoning")
                .or_else(|| obj.get("thinking"))
                .or_else(|| obj.get("thought"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let tool_calls_value = obj.get("tool_calls");
            if tool_calls_value.is_some() {
                let parsed: Result<Vec<ToolCall>, _> = serde_json::from_value(tool_calls_value.unwrap().clone());
                tracing::trace!(?tool_calls_value, ?parsed, "Kimi cmpl tool_calls field");
            }
            let finish_reason = obj.get("finish_reason")
                .or_else(|| obj.get("stop_reason"))
                .or_else(|| obj.get("stop"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if finish_reason.is_some() {
                tracing::trace!(?finish_reason, "Kimi cmpl finish_reason");
            }
            let tool_calls = tool_calls_value
                .and_then(|v| serde_json::from_value::<Vec<ToolCall>>(v.clone()).ok())
                .filter(|c| !c.is_empty());
            Some((text, None, None, false, false, false, reasoning, tool_calls))
        }
        "req" => {
            let id = obj.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
            Some((None, None, id, false, false, false, None, None))
        }
        "length" => {
            Some((None, None, None, false, true, false, None, None))
        }
        "search_plus" => {
            let citations = extract_search_citations(obj);
            Some((None, citations, None, false, false, false, None, None))
        }
        "all_done" => {
            tracing::trace!(event_json = %body, "Kimi all_done event");
            Some((None, None, None, true, false, false, None, None))
        }
        "error" => {
            Some((None, None, None, true, false, true, None, None))
        }
        _ => None,
    }
}

/// Extract citations from a `search_plus` event.
fn extract_search_citations(obj: &serde_json::Map<String, serde_json::Value>) -> Option<Vec<Citation>> {
    let msg = obj.get("msg")?.as_object()?;
    let citation_type = msg.get("type")?.as_str()?;
    if citation_type != "get_res" {
        return None;
    }
    let title = msg.get("title").and_then(|v| v.as_str()).map(|s| s.to_string());
    let url = msg.get("url").and_then(|v| v.as_str()).map(|s| s.to_string());

    if url.is_some() || title.is_some() {
        Some(vec![Citation {
            index: None,
            title,
            url,
            snippet: None,
            start_ix: None,
            end_ix: None,
        }])
    } else {
        None
    }
}

/// A single event from the new Kimi message API (K3+).
///
/// Unlike the legacy `completion/stream` endpoint which uses custom SSE with
/// an `event` field, the new `/session/{id}/message` endpoint sends standard
/// SSE with JSON `data:` lines that contain a `type` discriminator:
///
/// - `{"type": "delta", "content": "text"}` — streaming text delta
/// - `{"type": "thinking", "content": "..."}` — reasoning/thinking text
/// - `{"type": "message_finished"}` — the current message is complete
/// - `{"type": "done"}` — all messages in the session are done
/// - `{"type": "error", "error": "..."}` — error condition
/// - `{"type": "heartbeat"}` — keepalive
/// - `{"type": "tool_call", ...}` — tool call invocation
/// - `{"type": "complete_message", ...}` — full message object
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NewSseEvent {
    pub r#type: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub msg_id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub timestamp: Option<i64>,
    /// Some K3 responses put the completed assistant message below a
    /// `message` object instead of putting its fields on the event itself.
    #[serde(default)]
    pub message: Option<serde_json::Value>,
    #[serde(default)]
    pub reasoning_content: Option<serde_json::Value>,
    #[serde(default)]
    pub thinking: Option<serde_json::Value>,
}

/// Parse a single SSE line from the new Kimi message API (K3+).
///
/// Returns `(delta, thinking, turn_id, is_done, is_error, error_msg)`.
/// `turn_id` is the continuation token for follow-up messages.
pub fn parse_new_sse_line(
    line: &str,
) -> Option<(Option<String>, Option<String>, Option<String>, bool, bool, Option<String>)> {
    let trimmed = line.trim();

    // Handle [DONE] sentinel (may come without data: prefix)
    if trimmed == "[DONE]" {
        return Some((None, None, None, true, false, None));
    }

    let body = trimmed.strip_prefix("data: ")?;
    let body = body.trim();

    let Ok(event) = serde_json::from_str::<NewSseEvent>(body) else {
        tracing::debug!(raw = %body.chars().take(200).collect::<String>(), "K3 SSE: failed to parse event");
        return None;
    };

    match event.r#type.as_str() {
        "delta" => {
            tracing::trace!("K3 SSE delta: content_len={}", event.content.as_ref().map(|c| c.len()).unwrap_or(0));
            Some((event.content, None, None, false, false, None))
        }
        "thinking" => {
            tracing::trace!("K3 SSE thinking: content_len={}", event.content.as_ref().map(|c| c.len()).unwrap_or(0));
            Some((None, event.content, None, false, false, None))
        }
        "message_finished" => {
            tracing::debug!("K3 SSE message_finished: turn_id={:?}", event.turn_id);
            Some((None, None, event.turn_id, true, false, None))
        }
        "done" => {
            Some((None, None, None, true, false, None))
        }
        "error" => {
            let msg = event.error.unwrap_or_else(|| "unknown error".to_string());
            tracing::error!("K3 SSE error: {msg}");
            Some((None, None, None, true, true, Some(msg)))
        }
        "tool_call" => {
            // The K3 API may deliver tool calls as a `tool_call` event with
            // a structured payload rather than embedded in text as XML markers.
            tracing::info!(
                "K3 SSE tool_call event (raw): {}",
                body.chars().take(500).collect::<String>()
            );
            // Return as delta content — the post-stream XML parser will handle it.
            // If the API returns native tool_calls here, we'll adapt after seeing real data.
            Some((event.content, None, event.turn_id, false, false, None))
        }
        "complete_message" => {
            // The complete_message event carries the fully assembled response.
            // It may contain content and/or tool calls that weren't streamed
            // through individual delta events.
            tracing::info!(
                "K3 SSE complete_message (raw): {}",
                body.chars().take(500).collect::<String>()
            );
            let (content, thinking, turn_id) = event
                .message
                .as_ref()
                .map(|message| {
                    (
                        event.content.clone().or_else(|| text_field(message, "content")),
                        text_field(message, "reasoning_content")
                            .or_else(|| text_field(message, "thinking"))
                            .or_else(|| event.reasoning_content.as_ref().and_then(|v| text_value(v)))
                            .or_else(|| event.thinking.as_ref().and_then(|v| text_value(v))),
                        event.turn_id.clone().or_else(|| text_field(message, "turn_id")),
                    )
                })
                .unwrap_or((
                    event.content,
                    event
                        .reasoning_content
                        .as_ref()
                        .and_then(|v| text_value(v))
                        .or_else(|| event.thinking.as_ref().and_then(|v| text_value(v))),
                    event.turn_id,
                ));
            Some((content, thinking, turn_id, true, false, None))
        }
        "heartbeat" => {
            tracing::trace!("K3 SSE heartbeat");
            None
        }
        _ => {
            tracing::debug!(
                "K3 SSE unknown event type '{}': {}",
                event.r#type,
                body.chars().take(300).collect::<String>()
            );
            None
        }
    }
}

fn text_field(value: &serde_json::Value, key: &str) -> Option<String> {
    let field = value.get(key)?;
    text_value(field)
}

fn text_value(field: &serde_json::Value) -> Option<String> {
    if let Some(text) = field.as_str() {
        return Some(text.to_string());
    }
    if let Some(parts) = field.as_array() {
        let text = parts
            .iter()
            .filter_map(|part| {
                part.as_str().map(str::to_string).or_else(|| {
                    part.get("text").and_then(|v| v.as_str()).map(str::to_string)
                })
            })
            .collect::<String>();
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

/// Extract native tool calls from a K3 message-api event.
///
/// The message endpoint has used more than one envelope for the same
/// structured result (`tool_calls` on the event, `tool_call` for a single
/// event, and nested fields on `complete_message`). Keep this extraction
/// envelope-independent so a wire wrapper change does not force XML parsing.
pub fn extract_new_sse_tool_calls(line: &str) -> Option<Vec<ToolCall>> {
    let body = line.trim().strip_prefix("data: ")?.trim();
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let mut found = Vec::new();
    collect_tool_calls(&value, &mut found);
    let mut unique = Vec::with_capacity(found.len());
    for call in found {
        if !unique.iter().any(|existing: &ToolCall| existing.id == call.id) {
            unique.push(call);
        }
    }
    if unique.is_empty() { None } else { Some(unique) }
}

fn collect_tool_calls(value: &serde_json::Value, found: &mut Vec<ToolCall>) {
    match value {
        serde_json::Value::Object(map) => {
            for key in ["tool_calls", "toolCalls"] {
                if let Some(calls) = map.get(key) {
                    collect_tool_call_value(calls, found);
                }
            }
            if let Some(call) = map.get("tool_call").or_else(|| map.get("toolCall")) {
                collect_tool_call_value(call, found);
            }
            for child in map.values() {
                collect_tool_calls(child, found);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_tool_calls(item, found);
            }
        }
        _ => {}
    }
}

fn collect_tool_call_value(value: &serde_json::Value, found: &mut Vec<ToolCall>) {
    if let Ok(calls) = serde_json::from_value::<Vec<ToolCall>>(value.clone()) {
        found.extend(calls);
    } else if let Ok(call) = serde_json::from_value::<ToolCall>(value.clone()) {
        found.push(call);
    }
}

/// Collect full text and thinking from a non-streaming SSE body for the NEW API format.
/// Returns `(content_text, thinking_text, turn_id)`.
pub fn collect_text_from_new_sse(body: &[u8]) -> (String, Option<String>, Option<String>) {
    let text = String::from_utf8_lossy(body);
    let mut content = String::new();
    let mut thinking = String::new();
    let mut turn_id = None;
    for line in text.lines() {
        if let Some((delta, think, turn, is_done, is_error, _)) = parse_new_sse_line(line) {
            if let Some(t) = delta {
                append_delta_or_snapshot(&mut content, &t);
            }
            if let Some(t) = think {
                append_delta_or_snapshot(&mut thinking, &t);
            }
            if let Some(id) = turn {
                turn_id = Some(id);
            }
            if is_done || is_error {
                break;
            }
        }
    }
    let thinking = if thinking.is_empty() { None } else { Some(thinking) };
    (content, thinking, turn_id)
}

/// Merge a streamed delta or a later full-message snapshot without duplicating
/// content. K3 has emitted both forms in the same response: `delta` events
/// carry increments while `complete_message` can carry the complete text.
fn append_delta_or_snapshot(current: &mut String, next: &str) {
    if next.is_empty() {
        return;
    }
    if current.is_empty() {
        current.push_str(next);
    } else if next.starts_with(current.as_str()) {
        *current = next.to_string();
    } else if !current.starts_with(next) {
        current.push_str(next);
    }
}

/// Collect full text and thinking from a non-streaming SSE body.
/// Returns `(content_text, thinking_text)`.
pub fn collect_text_from_sse(body: &[u8]) -> (String, Option<String>) {
    let text = String::from_utf8_lossy(body);
    let mut content = String::new();
    let mut thinking = String::new();
    for line in text.lines() {
        if let Some((delta, _, _, _, _, _, think, _)) = parse_sse_line(line) {
            if let Some(t) = delta {
                content.push_str(&t);
            }
            if let Some(t) = think {
                thinking.push_str(&t);
            }
        }
    }
    let thinking = if thinking.is_empty() { None } else { Some(thinking) };
    (content, thinking)
}

/// Collect native tool calls from a non-streaming SSE body.
/// Returns all tool_calls discovered across all `cmpl` events.
pub fn collect_tool_calls_from_sse(body: &[u8]) -> Option<Vec<ToolCall>> {
    let text = String::from_utf8_lossy(body);
    let mut all_calls: Vec<ToolCall> = Vec::new();
    for line in text.lines() {
        if let Some((_, _, _, _, _, _, _, tool_calls)) = parse_sse_line(line) {
            if let Some(calls) = tool_calls {
                all_calls.extend(calls);
            }
        }
    }
    if all_calls.is_empty() { None } else { Some(all_calls) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_new_delta_event() {
        let line = r#"data: {"type": "delta", "content": "Hello, "}"#;
        let result = parse_new_sse_line(line);
        assert!(result.is_some());
        let (delta, thinking, turn_id, is_done, is_error, error_msg) = result.unwrap();
        assert_eq!(delta, Some("Hello, ".to_string()));
        assert!(thinking.is_none());
        assert!(turn_id.is_none());
        assert!(!is_done);
        assert!(!is_error);
        assert!(error_msg.is_none());
    }

    #[test]
    fn parse_new_thinking_event() {
        let line = r#"data: {"type": "thinking", "content": "I am reasoning..."}"#;
        let result = parse_new_sse_line(line);
        assert!(result.is_some());
        let (delta, thinking, _, _, _, _) = result.unwrap();
        assert!(delta.is_none());
        assert_eq!(thinking, Some("I am reasoning...".to_string()));
    }

    #[test]
    fn parse_new_message_finished() {
        let line = r#"data: {"type": "message_finished", "turn_id": "turn_abc123"}"#;
        let result = parse_new_sse_line(line);
        assert!(result.is_some());
        let (delta, _, turn_id, is_done, is_error, _) = result.unwrap();
        assert!(delta.is_none());
        assert_eq!(turn_id, Some("turn_abc123".to_string()));
        assert!(is_done);
        assert!(!is_error);
    }

    #[test]
    fn parse_new_done_event() {
        let line = r#"data: {"type": "done"}"#;
        let result = parse_new_sse_line(line);
        assert!(result.is_some());
        let (_, _, _, is_done, is_error, _) = result.unwrap();
        assert!(is_done);
        assert!(!is_error);
    }

    #[test]
    fn parse_new_done_sentinel() {
        let line = "[DONE]";
        let result = parse_new_sse_line(line);
        assert!(result.is_some());
        let (_, _, _, is_done, is_error, _) = result.unwrap();
        assert!(is_done);
        assert!(!is_error);
    }

    #[test]
    fn parse_new_error_event() {
        let line = r#"data: {"type": "error", "error": "content policy violation"}"#;
        let result = parse_new_sse_line(line);
        assert!(result.is_some());
        let (_, _, _, is_done, is_error, error_msg) = result.unwrap();
        assert!(is_done);
        assert!(is_error);
        assert_eq!(error_msg, Some("content policy violation".to_string()));
    }

    #[test]
    fn parse_new_heartbeat_skipped() {
        let line = r#"data: {"type": "heartbeat"}"#;
        let result = parse_new_sse_line(line);
        assert!(result.is_none());
    }

    #[test]
    fn extract_new_native_tool_calls_from_event() {
        let line = r#"data: {"type":"tool_call","tool_call":{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}}"#;
        let calls = extract_new_sse_tool_calls(line).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "get_weather");
    }

    #[test]
    fn extract_new_native_tool_calls_from_complete_message() {
        let line = r#"data: {"type":"complete_message","message":{"tool_calls":[{"id":"call_2","type":"function","function":{"name":"lookup","arguments":"{}"}}]}}"#;
        let calls = extract_new_sse_tool_calls(line).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_2");
    }

    #[test]
    fn extract_new_native_tool_calls_deduplicates_nested_envelopes() {
        let line = r#"data: {"type":"complete_message","tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{}"}}],"message":{"tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{}"}}]}}"#;
        let calls = extract_new_sse_tool_calls(line).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
    }

    #[test]
    fn parse_new_complete_message_extracts_nested_content_and_reasoning() {
        let line = r#"data: {"type":"complete_message","message":{"content":"answer","reasoning_content":"thought","turn_id":"turn_nested"}}"#;
        let (delta, thinking, turn_id, done, error, _) = parse_new_sse_line(line).unwrap();
        assert_eq!(delta, Some("answer".to_string()));
        assert_eq!(thinking, Some("thought".to_string()));
        assert_eq!(turn_id, Some("turn_nested".to_string()));
        assert!(done);
        assert!(!error);
    }

    #[test]
    fn parse_new_complete_message_extracts_top_level_reasoning() {
        let line = r#"data: {"type":"complete_message","content":"answer","reasoning_content":"thought","turn_id":"turn_top"}"#;
        let (delta, thinking, turn_id, done, error, _) = parse_new_sse_line(line).unwrap();
        assert_eq!(delta, Some("answer".to_string()));
        assert_eq!(thinking, Some("thought".to_string()));
        assert_eq!(turn_id, Some("turn_top".to_string()));
        assert!(done);
        assert!(!error);
    }

    #[test]
    fn collect_text_from_new_sse_basic() {
        let sse = concat!(
            "data: {\"type\": \"delta\", \"content\": \"Hello\"}\n",
            "data: {\"type\": \"delta\", \"content\": \" World\"}\n",
            "data: {\"type\": \"message_finished\"}\n",
            "data: {\"type\": \"done\"}\n",
        );
        let (text, thinking, turn_id) = collect_text_from_new_sse(sse.as_bytes());
        assert_eq!(text, "Hello World");
        assert!(thinking.is_none());
        assert!(turn_id.is_none());
    }

    #[test]
    fn collect_text_from_new_sse_with_thinking_and_turn() {
        let sse = concat!(
            "data: {\"type\": \"thinking\", \"content\": \"thinking step\"}\n",
            "data: {\"type\": \"delta\", \"content\": \"answer\"}\n",
            "data: {\"type\": \"message_finished\", \"turn_id\": \"turn_xyz\"}\n",
        );
        let (text, thinking, turn_id) = collect_text_from_new_sse(sse.as_bytes());
        assert_eq!(text, "answer");
        assert_eq!(thinking, Some("thinking step".to_string()));
        assert_eq!(turn_id, Some("turn_xyz".to_string()));
    }

    #[test]
    fn collect_text_from_new_sse_does_not_duplicate_complete_snapshot() {
        let sse = concat!(
            "data: {\"type\":\"delta\",\"content\":\"Hello\"}\n",
            "data: {\"type\":\"delta\",\"content\":\" world\"}\n",
            "data: {\"type\":\"complete_message\",\"message\":{\"content\":\"Hello world\",\"reasoning_content\":\"thought\"}}\n",
        );
        let (text, thinking, _) = collect_text_from_new_sse(sse.as_bytes());
        assert_eq!(text, "Hello world");
        assert_eq!(thinking.as_deref(), Some("thought"));
    }

    #[test]
    fn parse_cmpl_event() {
        let line = r#"data: {"event": "cmpl", "text": "Hello, "}"#;
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (delta, _, _, done, length, error, thinking, tool_calls) = result.unwrap();
        assert_eq!(delta, Some("Hello, ".to_string()));
        assert!(!done);
        assert!(!length);
        assert!(!error);
        assert!(thinking.is_none());
        assert!(tool_calls.is_none());
    }

    #[test]
    fn parse_cmpl_with_thinking() {
        let line = r#"data: {"event": "cmpl", "text": "Visible answer", "reasoning": "thinking step"}"#;
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (delta, _, _, done, _, _, thinking, tool_calls) = result.unwrap();
        assert_eq!(delta, Some("Visible answer".to_string()));
        assert_eq!(thinking, Some("thinking step".to_string()));
        assert!(!done);
        assert!(tool_calls.is_none());
    }

    #[test]
    fn parse_cmpl_with_thinking_field() {
        let line = r#"data: {"event": "cmpl", "text": "answer", "thinking": "inner monologue"}"#;
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (delta, _, _, _, _, _, thinking, tool_calls) = result.unwrap();
        assert_eq!(delta, Some("answer".to_string()));
        assert_eq!(thinking, Some("inner monologue".to_string()));
        assert!(tool_calls.is_none());
    }

    #[test]
    fn parse_cmpl_with_native_tool_calls() {
        let line = r#"data: {"event": "cmpl", "text": "", "tool_calls": [{"id":"call_abc","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]}"#;
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (delta, _, _, _, _, _, _, tool_calls) = result.unwrap();
        assert_eq!(delta, Some("".to_string()));
        let calls = tool_calls.expect("should have native tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].function.name, "get_weather");
    }

    #[test]
    fn parse_all_done_event() {
        let line = r#"data: {"event": "all_done"}"#;
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (delta, _, _, done, length, error, thinking, tool_calls) = result.unwrap();
        assert!(delta.is_none());
        assert!(done);
        assert!(!length);
        assert!(!error);
        assert!(thinking.is_none());
        assert!(tool_calls.is_none());
    }

    #[test]
    fn parse_length_event() {
        let line = r#"data: {"event": "length"}"#;
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (_, _, _, done, length, _, _, _) = result.unwrap();
        assert!(!done);
        assert!(length);
    }

    #[test]
    fn parse_req_event() {
        let line = r#"data: {"event": "req", "id": "seg_123"}"#;
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (_, _, segment_id, _, _, _, _, _) = result.unwrap();
        assert_eq!(segment_id, Some("seg_123".to_string()));
    }

    fn default_request() -> crate::models::ChatCompletionRequest {
        crate::models::ChatCompletionRequest {
            model: String::new(),
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
        }
    }

    #[test]
    fn build_payload_has_correct_structure() {
        let msg = ChatMessage {
            role: "user".to_string(),
            content: crate::models::ChatContent::String("Hi".to_string()),
            name: None,
            reasoning_content: None,
            citations: None,
            tool_calls: None,
            tool_call_id: None,
        };
        let model_def = KimiModelDef {
            id: "kimi-k2".to_string(),
            kimiplus_id: "kimi".to_string(),
            use_search: false,
            use_research: false,
            use_math: false,
            is_thinking: false,
            supports_vision: true,
            supports_tools: true,
        };
        let req = default_request();
        let payload = build_request_payload(&[msg], None, &model_def, false, &req, &[], &[]);
        assert_eq!(payload["kimiplus_id"], "kimi");
        assert!(payload.get("model").is_none(), "model field should NOT be in legacy payload");
        assert!(!payload["use_search"].as_bool().unwrap());
        assert!(payload["extend"]["sidebar"].as_bool().unwrap());
    }

    #[test]
    fn collect_text_joins_cmpl_events() {
        let sse = concat!(
            "data: {\"event\": \"cmpl\", \"text\": \"Hello\"}\n",
            "data: {\"event\": \"cmpl\", \"text\": \" World\"}\n",
            "data: {\"event\": \"all_done\"}\n",
        );
        let (text, thinking) = collect_text_from_sse(sse.as_bytes());
        assert_eq!(text, "Hello World");
        assert!(thinking.is_none());
    }

    #[test]
    fn collect_text_with_thinking() {
        let sse = concat!(
            "data: {\"event\": \"cmpl\", \"text\": \"answer\", \"reasoning\": \"think step\"}\n",
            "data: {\"event\": \"all_done\"}\n",
        );
        let (text, thinking) = collect_text_from_sse(sse.as_bytes());
        assert_eq!(text, "answer");
        assert_eq!(thinking, Some("think step".to_string()));
    }
}
