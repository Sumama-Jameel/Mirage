use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use rand::Rng;
use sha3::{Digest, Sha3_512};
use uuid::Uuid;

use crate::models::ChatCompletionRequest;
use crate::models::ChatMessage;
use crate::models::Citation;
use crate::models::{Tool, ToolChoice};

use super::state::StoredConversation;

/// Build the proof-of-work token using SHA3-512 hashing.
///
/// The PoW challenge requires finding a nonce whose SHA3-512 hash of
/// (seed + base64(fingerprint)) is lexicographically <= the difficulty
/// string. This follows the exact algorithm from gpt4free's
/// `proofofwork.py` / `OpenaiChat.py`.
pub fn generate_proof_token(
    required: bool,
    seed: &str,
    difficulty: &str,
) -> Option<String> {
    if !required {
        return None;
    }

    let mut rng = rand::thread_rng();
    let screen_res: u32 = rng.gen_range(3008..=6016) * 2;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();

    let mut proof_token: Vec<serde_json::Value> = vec![
        serde_json::Value::Number(serde_json::Number::from(screen_res)),
        serde_json::Value::String(format!("{now:?}")),
        serde_json::Value::Null,
        serde_json::Value::Number(serde_json::Number::from(0)),
        serde_json::Value::String("Mozilla/5.0".to_string()),
        serde_json::Value::String(
            "https://tcr9i.chat.openai.com/v2/35536E1E-65B4-4D96-9D97-6ADB7EFF8147/api.js"
                .to_string(),
        ),
        serde_json::Value::String("dpl=1440a687921de39ff5ee56b92807faaadce73f13".to_string()),
        serde_json::Value::String("en".to_string()),
        serde_json::Value::String("en-US".to_string()),
        serde_json::Value::Null,
        serde_json::Value::String("_reactListeningcfilawjnerp".to_string()),
        serde_json::Value::String("alert".to_string()),
    ];

    let diff_len = difficulty.len();
    for nonce in 0..100_000 {
        proof_token[3] = serde_json::Value::Number(serde_json::Number::from(nonce));
        let json_str = serde_json::to_string(&proof_token).unwrap_or_default();
        let b64 = base64::engine::general_purpose::STANDARD.encode(json_str.as_bytes());
        let combined = format!("{seed}{b64}");
        let hash = Sha3_512::digest(combined.as_bytes());
        let hex = format!("{hash:x}");
        if &hex[..diff_len.min(hex.len())] <= difficulty {
            return Some(format!("gAAAAAB{b64}"));
        }
    }

    let fallback = base64::engine::general_purpose::STANDARD.encode(format!("\"{seed}\""));
    Some(format!("gAAAAABwQ8Lk5FbGpA2NcR9dShT6gYjU7VxZ4D{fallback}"))
}

/// Helper for the first proof_token config used in nonce-less requests.
pub fn get_proof_token_config(user_agent: &str) -> Vec<serde_json::Value> {
    let mut rng = rand::thread_rng();
    vec![
        serde_json::Value::Number(serde_json::Number::from(rng.gen_range(3008..=6016) * 2)),
        serde_json::Value::String("unknown".to_string()),
        serde_json::Value::Null,
        serde_json::Value::Number(serde_json::Number::from(0)),
        serde_json::Value::String(user_agent.to_string()),
        serde_json::Value::String(
            "https://tcr9i.chat.openai.com/v2/35536E1E-65B4-4D96-9D97-6ADB7EFF8147/api.js"
                .to_string(),
        ),
        serde_json::Value::String("dpl=1440a687921de39ff5ee56b92807faaadce73f13".to_string()),
        serde_json::Value::String("en".to_string()),
        serde_json::Value::String("en-US".to_string()),
        serde_json::Value::Null,
        serde_json::Value::String("_reactListeningcfilawjnerp".to_string()),
        serde_json::Value::String("alert".to_string()),
    ]
}

/// Build the requirements token from the proof token config.
/// This is sent as `{"p": <serialized config>}` to `/backend-api/sentinel/chat-requirements`.
pub fn get_requirements_token(config: &[serde_json::Value]) -> String {
    serde_json::to_string(config).unwrap_or_default()
}

/// Parse a single SSE line from ChatGPT's streaming response.
///
/// ChatGPT returns SSE with path-based deltas:
/// `{"p": "/message/content/parts/0", "v": "...", "weight": 1}` → content delta
/// `{"p": "/message/content/parts/0", "v": "...", "weight": 0}` → thinking/reasoning delta
/// `{"p": "/message/metadata/search_result_groups", "v": [...]}` → citations
/// `{"p": "/message/tool_calls", "v": [...]}` → native tool calls
/// `{"conversation_id": "...", "message_id": "..."}` → first chunk with session info
/// `data: [DONE]` → stream termination
///
/// Returns `(delta_text, citations, conversation_id, message_id, thinking_delta, tool_calls)`.
#[allow(clippy::type_complexity)]
pub fn parse_sse_line(
    line: &str,
    accumulated_text: &str,
) -> Option<(
    Option<String>,
    Option<Vec<Citation>>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<Vec<serde_json::Value>>,
)> {
    let body = line.strip_prefix("data: ")?;

    // Handle [DONE]
    if body == "[DONE]" {
        return None;
    }

    // Handle chatgpt.com internal ping messages
    if body == "ping" {
        return None;
    }

    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return None;
    };
    let Some(obj) = json.as_object() else {
        return None;
    };

    // Extract conversation_id from first chunk
    let conversation_id = obj
        .get("conversation_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    // Extract message_id from first chunk
    let message_id = obj
        .get("message_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    // Extract path-based deltas
    let p = match obj.get("p").and_then(|v| v.as_str()) {
        Some(path) => path,
        None => {
            // A `patch`-typed event carries its own paths inside `v` and has
            // no top-level `p`. Handle it before treating the event as
            // metadata-only.
            if obj.get("o").and_then(|o| o.as_str()) == Some("patch") {
                return handle_patch_event(obj, conversation_id, message_id);
            }
            // No path field — this is a metadata-only event (conversation_id, etc.)
            if conversation_id.is_some() || message_id.is_some() {
                return Some((None, None, conversation_id, message_id, None, None));
            }
            return None;
        }
    };

    // Native tool calls path
    if p == "/message/tool_calls" {
        let tool_calls = obj.get("v").and_then(|v| v.as_array()).cloned();
        if tool_calls.as_ref().map_or(true, |c| c.is_empty()) {
            return Some((None, None, conversation_id, message_id, None, None));
        }
        return Some((None, None, conversation_id, message_id, None, tool_calls));
    }

    // Check for part index indicators — chatgpt sometimes routes thinking
    // to a different part index (e.g. parts/1 for thinking, parts/0 for
    // final text). We also check the weight field: weight=0 indicates
    // thinking/reasoning content, weight=1 (or absent) indicates visible
    // content. This matches the behavior of o-series models.
    if p.starts_with("/message/content/parts/") {
        let v = obj.get("v").and_then(|v| v.as_str())?;
        let part_index: Option<i64> = p
            .rsplit('/')
            .next()
            .and_then(|s| s.parse().ok());

        let is_thinking = obj
            .get("weight")
            .and_then(|w| w.as_i64())
            == Some(0)
            || part_index == Some(1);

        if is_thinking {
            return Some((None, None, conversation_id, message_id, Some(v.to_string()), None));
        } else {
            return Some((Some(v.to_string()), None, conversation_id, message_id, None, None));
        }
    }

    if p == "/message/metadata/search_result_groups" {
        let citations = extract_citations_from_sse(obj, accumulated_text);
        return Some((None, citations, conversation_id, message_id, None, None));
    }

    // Handle patch operations: {"p": "", "o": "patch", "v": [{patch}, ...]}
    // Each patch has its own p and v fields. We look for content patches
    // within the patch array. Also reached from the `p`-guard above when the
    // event has no top-level path field.
    if obj.get("o").and_then(|o| o.as_str()) == Some("patch") {
        return handle_patch_event(obj, conversation_id, message_id);
    }

    // Unknown path, still propagate session info
    if conversation_id.is_some() || message_id.is_some() {
        return Some((None, None, conversation_id, message_id, None, None));
    }

    None
}

/// Handle a ChatGPT patch operation: `{"o":"patch","v":[{patch},...]}`.
///
/// Each patch carries its own `p`/`v` pair. Content deltas and native tool
/// calls live in patches whose path matches `/message/content/parts/*` and
/// `/message/tool_calls`. This is reached both for events with a top-level
/// `p` and, importantly, for events that have no top-level path (the format
/// ChatGPT's web endpoint uses for consolidated content).
fn handle_patch_event(
    obj: &serde_json::Map<String, serde_json::Value>,
    conversation_id: Option<String>,
    message_id: Option<String>,
) -> Option<(
    Option<String>,
    Option<Vec<Citation>>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<Vec<serde_json::Value>>,
)> {
    let mut result: Option<(
        Option<String>,
        Option<Vec<Citation>>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<Vec<serde_json::Value>>,
    )> = None;
    if let Some(patches) = obj.get("v").and_then(|v| v.as_array()) {
        for patch in patches {
            let pp = patch.get("p").and_then(|v| v.as_str()).unwrap_or("");
            if pp.starts_with("/message/content/parts/") {
                if let Some(t) = patch.get("v").and_then(|v| v.as_str()) {
                    result = Some((
                        Some(t.to_string()),
                        None,
                        conversation_id.clone(),
                        message_id.clone(),
                        None,
                        None,
                    ));
                }
            }
            if pp == "/message/tool_calls" {
                if let Some(calls) = patch.get("v").and_then(|v| v.as_array()) {
                    if !calls.is_empty() {
                        result = Some((
                            None,
                            None,
                            conversation_id.clone(),
                            message_id.clone(),
                            None,
                            Some(calls.clone()),
                        ));
                    }
                }
            }
        }
    }
    if result.is_some() {
        return result;
    }
    // Patch processed, still propagate session info
    if conversation_id.is_some() || message_id.is_some() {
        return Some((None, None, conversation_id, message_id, None, None));
    }
    None
}

/// Extract citations from a `/message/metadata/search_result_groups` SSE event.
///
/// Computes UTF-16 `start_ix`/`end_ix` offsets by locating the citation
/// snippet (or title) inside `accumulated_text`. These offsets match the
/// `url_citation` annotation format used by OpenAI-compatible clients.
fn extract_citations_from_sse(
    obj: &serde_json::Map<String, serde_json::Value>,
    accumulated_text: &str,
) -> Option<Vec<Citation>> {
    let sources = obj.get("v").and_then(|v| v.as_array())?;
    let citations: Vec<Citation> = sources
        .iter()
        .filter_map(|source| {
            let url = source
                .get("url")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let title = source
                .get("title")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let snippet = source
                .get("snippet")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            if url.is_none() && title.is_none() {
                return None;
            }

            // Compute UTF-16 offsets: search for snippet first, then title.
            let search = snippet.as_deref().or_else(|| title.as_deref());
            let (start_ix, end_ix) = search
                .and_then(|needle| {
                    let byte_pos = accumulated_text.find(needle)?;
                    let start = accumulated_text[..byte_pos]
                        .chars()
                        .map(|c| c.len_utf16() as i64)
                        .sum::<i64>();
                    let end = start + needle
                        .chars()
                        .map(|c| c.len_utf16() as i64)
                        .sum::<i64>();
                    Some((Some(start), Some(end)))
                })
                .unwrap_or((None, None));

            Some(Citation {
                index: None,
                title,
                url,
                snippet,
                start_ix,
                end_ix,
            })
        })
        .collect();

    if citations.is_empty() {
        None
    } else {
        Some(citations)
    }
}

/// Build the payload for the `/backend-api/codex/responses` endpoint.
///
/// This endpoint supports native tool calling (unlike the conversation
/// endpoint which only handles server-side tools). Uses OpenAI's
/// Responses API format internally.
#[allow(dead_code)]
pub fn build_codex_responses_payload(
    messages: &[ChatMessage],
    model_id: &str,
    tools: Option<&[Tool]>,
    tool_choice: Option<&ToolChoice>,
    stream: bool,
) -> serde_json::Value {
    let input: Vec<serde_json::Value> = messages
        .iter()
        .map(|msg| {
            let role = msg.role.clone();
            serde_json::json!({
                "role": role,
                "content": [
                    {
                        "type": "input_text",
                        "text": msg.content.as_text()
                    }
                ]
            })
        })
        .collect();

    let tools_val = tools.map(|t| serde_json::to_value(t).unwrap_or_default());

    let mut body = serde_json::json!({
        "model": model_id,
        "input": input,
        "stream": stream,
    });

    if let Some(ref t) = tools_val {
        body["tools"] = t.clone();
    }
    if let Some(ref tc) = tool_choice {
        body["tool_choice"] = serde_json::to_value(tc).unwrap_or_default();
    }

    body
}

/// Parse a single SSE line from the `/backend-api/codex/responses` endpoint.
///
/// Responses API events:
/// - `response.output_text.delta` → text content delta
/// - `response.thinking.delta` → reasoning/thinking content
/// - `response.output_item.added` → a tool call started
/// - `response.function_call_arguments.delta` → streaming tool call args
/// - `response.function_call_arguments.done` → final tool call args
/// - `response.output_item.done` → tool call completed
/// - `response.completed` → response finished
///
/// Returns `(text_delta, thinking_delta, tool_calls_json)`.
#[allow(dead_code)]
pub fn parse_codex_sse_line(
    line: &str,
) -> Option<(
    Option<String>,
    Option<String>,
    Option<serde_json::Value>,
)> {
    let body = line.strip_prefix("data: ")?;
    if body == "[DONE]" {
        return None;
    }

    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return None;
    };

    let event_type = json.get("type")?.as_str()?;

    match event_type {
        "response.output_text.delta" => {
            let delta = json
                .pointer("/data/delta")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some((delta, None, None))
        }
        "response.thinking.delta" => {
            let delta = json
                .pointer("/data/delta")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some((None, delta, None))
        }
        "response.output_item.added" => {
            Some((None, None, Some(json)))
        }
        "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
            let delta = json
                .pointer("/data/delta")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some((None, None, delta.map(serde_json::Value::String)))
        }
        "response.output_item.done" => {
            let obj = json.pointer("/data/object");
            if let Some(o) = obj {
                if o.get("type").and_then(|v| v.as_str()) == Some("function_call") {
                    return Some((None, None, Some(o.clone())));
                }
            }
            Some((None, None, None))
        }
        "response.completed" => {
            // End of stream — signal with None
            None
        }
        _ => {
            // Unknown event type — ignore
            Some((None, None, None))
        }
    }
}

/// Build the ChatGPT conversation request body.
///
/// Accepts messages (including tool results), optional image list,
/// optional conversation state, and the model ID.
pub fn build_request_payload(
    messages: &[ChatMessage],
    conv: Option<&StoredConversation>,
    image_list: Option<&serde_json::Value>,
    model_id: &str,
    parent_message_id: &str,
    search: Option<bool>,
    request: &ChatCompletionRequest,
) -> serde_json::Value {
    let chat_messages: Vec<serde_json::Value> = messages
        .iter()
        .map(|msg| {
            let role = msg.role.clone();
            let parts = if role == "tool" {
                // Tool results carry the content as part of the tool response
                vec![serde_json::Value::String(msg.content.as_text())]
            } else {
                vec![serde_json::Value::String(msg.content.as_text())]
            };

            let mut m = serde_json::json!({
                "id": Uuid::new_v4().to_string(),
                "author": { "role": role },
                "content": {
                    "content_type": "text",
                    "parts": parts,
                },
                "metadata": {
                    "serialization_metadata": {
                        "custom_symbol_offsets": []
                    }
                },
                "create_time": SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64(),
            });

            // Inject images into the last user message as image_asset_pointer
            // parts and populate the attachments metadata so the model can
            // resolve file-service:// references to image content.
            if role == "user" {
                if let Some(images) = image_list {
                    if let Some(arr) = images.as_array() {
                        if !arr.is_empty() {
                            let text_part = msg.content.as_text();
                            let mut multimodal_parts: Vec<serde_json::Value> = Vec::with_capacity(1 + arr.len());
                            multimodal_parts.push(serde_json::Value::String(text_part));
                            let mut attachments = Vec::with_capacity(arr.len());
                            for item in arr {
                                if let Some(ap) = item.get("asset_pointer") {
                                    multimodal_parts.push(ap.clone());
                                }
                                if let Some(att) = item.get("attachment") {
                                    attachments.push(att.clone());
                                }
                            }
                            m["content"]["content_type"] = serde_json::json!("multimodal_text");
                            m["content"]["parts"] = serde_json::json!(multimodal_parts);
                            if !attachments.is_empty() {
                                m["metadata"]["attachments"] = serde_json::json!(attachments);
                            }
                        }
                    }
                }
            }

            m
        })
        .collect();

    let mut body = serde_json::json!({
        "action": "next",
        "messages": chat_messages,
        "parent_message_id": parent_message_id,
        "model": model_id,
        "conversation_mode": {
            "kind": "primary_assistant"
        },
        "timezone_offset_min": -480,
        "timezone": "America/Los_Angeles",
        "supports_buffering": true,
        "supported_encodings": ["v1"],
        "history_and_training_disabled": false,
        "paragen_cot_summary_display_override": "allow",
    });

    if let Some(conv) = conv {
        body["conversation_id"] = serde_json::json!(conv.conversation_id);
    }

    if search == Some(true) {
        body["force_search"] = serde_json::json!(true);
    }

    if let Some(t) = request.temperature {
        body["temperature"] = serde_json::json!(t);
    }
    if let Some(m) = request.max_tokens {
        body["max_tokens"] = serde_json::json!(m);
    }
    if let Some(p) = request.top_p {
        body["top_p"] = serde_json::json!(p);
    }
    if let Some(ref s) = request.stop {
        body["stop"] = serde_json::json!(s);
    }
    if let Some(p) = request.presence_penalty {
        body["presence_penalty"] = serde_json::json!(p);
    }
    if let Some(f) = request.frequency_penalty {
        body["frequency_penalty"] = serde_json::json!(f);
    }

    if let Some(ref tools) = request.tools {
        body["tools"] = serde_json::to_value(tools).unwrap_or(serde_json::Value::Null);
    }
    // tool_choice is NOT sent to the conversation endpoint — the endpoint
    // ignores the standard OpenAI tool_choice parameter. The choice is
    // controlled through prompt injection in direct.rs instead.

    // Forward native JSON mode if provided (ChatGPT web API supports it for gpt-4o, gpt-4o-mini).
    if let Some(ref fmt) = request.response_format {
        if fmt.r#type == "json_object" {
            body["response_format"] = serde_json::json!({
                "type": "json_object"
            });
        }
    }

    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_proof_token_when_not_required() {
        assert_eq!(generate_proof_token(false, "", ""), None);
    }

    #[test]
    fn generate_proof_token_returns_string() {
        let result = generate_proof_token(true, "test_seed", "0fffff");
        assert!(result.is_some());
        let token = result.unwrap();
        assert!(token.starts_with("gAAAAAB"));
        assert!(token.len() > 50);
    }

    #[test]
    fn parse_sse_line_returns_none_for_done() {
        assert_eq!(parse_sse_line("data: [DONE]", ""), None);
    }

    #[test]
    fn parse_sse_line_returns_none_for_non_data() {
        assert_eq!(parse_sse_line("hello", ""), None);
    }

    #[test]
    fn parse_sse_line_extracts_conversation_id() {
        let line = r#"data: {"conversation_id": "abc-123", "message_id": "msg-456"}"#;
        let result = parse_sse_line(line, "");
        assert!(result.is_some());
        let (content, citations, conv_id, msg_id, thinking, _tool_calls) = result.unwrap();
        assert!(content.is_none());
        assert!(citations.is_none());
        assert_eq!(conv_id.as_deref(), Some("abc-123"));
        assert_eq!(msg_id.as_deref(), Some("msg-456"));
        assert!(thinking.is_none());
    }

    #[test]
    fn parse_sse_line_extracts_content_delta() {
        let line = r#"data: {"p": "/message/content/parts/0", "v": "Hello, "}"#;
        let result = parse_sse_line(line, "");
        assert!(result.is_some());
        let (content, citations, conv_id, msg_id, thinking, _tool_calls) = result.unwrap();
        assert_eq!(content, Some("Hello, ".to_string()));
        assert!(citations.is_none());
        assert!(conv_id.is_none());
        assert!(msg_id.is_none());
        assert!(thinking.is_none());
    }

    #[test]
    fn parse_sse_line_extracts_thinking_from_weight_zero() {
        let line = r#"data: {"p": "/message/content/parts/0", "v": "Let me reason...", "weight": 0}"#;
        let result = parse_sse_line(line, "");
        assert!(result.is_some());
        let (content, citations, _conv_id, _msg_id, thinking, _tool_calls) = result.unwrap();
        assert!(content.is_none());
        assert!(thinking.is_some());
        assert_eq!(thinking, Some("Let me reason...".to_string()));
        assert!(citations.is_none());
    }

    #[test]
    fn parse_sse_line_extracts_content_with_weight_one() {
        let line = r#"data: {"p": "/message/content/parts/0", "v": "Final answer", "weight": 1}"#;
        let result = parse_sse_line(line, "");
        assert!(result.is_some());
        let (content, _, _, _, thinking, _tool_calls) = result.unwrap();
        assert_eq!(content, Some("Final answer".to_string()));
        assert!(thinking.is_none());
    }

    #[test]
    fn parse_sse_line_extracts_thinking_from_part_index_one() {
        let line = r#"data: {"p": "/message/content/parts/1", "v": "reasoning step"}"#;
        let result = parse_sse_line(line, "");
        assert!(result.is_some());
        let (content, _, _, _, thinking, _tool_calls) = result.unwrap();
        assert!(content.is_none());
        assert_eq!(thinking, Some("reasoning step".to_string()));
    }

    #[test]
    fn parse_sse_line_extracts_citations() {
        let line = r#"data: {"p": "/message/metadata/search_result_groups", "v": [{"url": "https://example.com", "title": "Example"}]}"#;
        let result = parse_sse_line(line, "");
        assert!(result.is_some());
        let (content, citations, _, _, _, _tool_calls) = result.unwrap();
        assert!(content.is_none());
        let cites = citations.unwrap();
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].url, Some("https://example.com".to_string()));
        assert_eq!(cites[0].title, Some("Example".to_string()));
        assert_eq!(cites[0].snippet, None);
        // Citation text not found in empty accumulated_text → offsets stay None
        assert_eq!(cites[0].start_ix, None);
        assert_eq!(cites[0].end_ix, None);
    }

    #[test]
    fn parse_sse_line_citations_compute_utf16_offsets() {
        let body = "According to Example, the sky is blue.";
        let line = r#"data: {"p": "/message/metadata/search_result_groups", "v": [{"url": "https://example.com", "title": "Example", "snippet": "Example"}]}"#;
        let result = parse_sse_line(line, body);
        assert!(result.is_some());
        let (content, citations, _, _, _, _) = result.unwrap();
        assert!(content.is_none());
        let cites = citations.unwrap();
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].url, Some("https://example.com".to_string()));
        assert_eq!(cites[0].start_ix, Some(13));
        assert_eq!(cites[0].end_ix, Some(20));
    }

    fn default_request(model: &str) -> crate::models::ChatCompletionRequest {
        crate::models::ChatCompletionRequest {
            model: model.to_string(),
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
    fn build_request_payload_includes_messages() {
        let msgs = vec![
            ChatMessage {
                role: "user".to_string(),
                content: crate::models::ChatContent::String("Hello".to_string()),
                name: None,
                reasoning_content: None,
                citations: None,
                tool_calls: None,
                tool_call_id: None,
            },
        ];
        let req = default_request("text-davinci-002-render-sha");
        let payload = build_request_payload(&msgs, None, None, "text-davinci-002-render-sha", "parent-1", None, &req);
        let messages = payload["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["author"]["role"], "user");
        assert_eq!(messages[0]["content"]["parts"][0], "Hello");
        assert_eq!(payload["model"], "text-davinci-002-render-sha");
        assert_eq!(payload["parent_message_id"], "parent-1");
        assert_eq!(payload["action"], "next");
    }

    #[test]
    fn build_request_payload_includes_force_search() {
        let msgs = vec![];
        let req = default_request("model-1");
        let payload = build_request_payload(&msgs, None, None, "model-1", "parent-1", Some(true), &req);
        assert_eq!(payload["force_search"], true);
    }

    #[test]
    fn build_request_payload_omits_force_search_when_not_requested() {
        let msgs = vec![];
        let req = default_request("model-1");
        let payload = build_request_payload(&msgs, None, None, "model-1", "parent-1", None, &req);
        assert!(payload.get("force_search").is_none());
    }

    #[test]
    fn build_request_payload_includes_conversation_id() {
        let msgs = vec![];
        let conv = StoredConversation {
            conversation_id: "conv-1".to_string(),
            message_id: "msg-1".to_string(),
            model_id: "test".to_string(),
        };
        let req = default_request("model-1");
        let payload = build_request_payload(&msgs, Some(&conv), None, "model-1", "parent-1", None, &req);
        assert_eq!(payload["conversation_id"], "conv-1");
    }

    #[test]
    fn parse_sse_line_extracts_native_tool_calls() {
        let line = r#"data: {"p": "/message/tool_calls", "v": [{"id": "call_abc123", "type": "function", "function": {"name": "get_weather", "arguments": "{\"location\":\"Paris\"}"}}]}"#;
        let result = parse_sse_line(line, "");
        assert!(result.is_some());
        let (content, citations, _conv_id, _msg_id, _thinking, tool_calls) = result.unwrap();
        assert!(content.is_none());
        assert!(citations.is_none());
        let calls = tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_abc123");
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        assert_eq!(calls[0]["function"]["arguments"], "{\"location\":\"Paris\"}");
    }

    #[test]
    fn parse_sse_line_returns_none_for_empty_tool_calls() {
        let line = r#"data: {"p": "/message/tool_calls", "v": []}"#;
        let result = parse_sse_line(line, "");
        assert!(result.is_some());
        let (content, _citations, _conv_id, _msg_id, _thinking, tool_calls) = result.unwrap();
        assert!(content.is_none());
        assert!(tool_calls.is_none());
    }

    #[test]
    fn build_request_payload_includes_tools() {
        use crate::models::{FunctionDefinition, Tool};
        let msgs = vec![];
        let mut req = default_request("model-1");
        req.tools = Some(vec![Tool {
            r#type: "function".to_string(),
            function: FunctionDefinition {
                name: "get_weather".to_string(),
                description: Some("Get weather".to_string()),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "location": {"type": "string"}
                    },
                    "required": ["location"]
                })),
                strict: None,
            },
        }]);
        let payload = build_request_payload(&msgs, None, None, "model-1", "parent-1", None, &req);
        let tools = payload.get("tools");
        assert!(tools.is_some(), "payload should include tools");
        assert!(tools.unwrap().is_array(), "tools should be an array");
        assert_eq!(tools.unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn build_request_payload_omits_tool_choice() {
        use crate::models::ToolChoice;
        let msgs = vec![];
        let mut req = default_request("model-1");
        req.tool_choice = Some(ToolChoice::Mode("auto".to_string()));
        let payload = build_request_payload(&msgs, None, None, "model-1", "parent-1", None, &req);
        // The conversation endpoint does not support tool_choice;
        // it is handled via prompt injection in direct.rs instead.
        assert!(payload.get("tool_choice").is_none_or(|v| v.is_null()));
    }

    #[test]
    fn build_request_payload_multimodal_with_file_urls() {
        use crate::models::ChatContent;
        let msgs = vec![
            ChatMessage {
                role: "user".to_string(),
                content: ChatContent::String("What's in this image?".to_string()),
                name: None,
                reasoning_content: None,
                citations: None,
                tool_calls: None,
                tool_call_id: None,
            },
        ];
        // New format: each entry is an object with asset_pointer and attachment
        let images = serde_json::json!([{
            "asset_pointer": {
                "content_type": "image_asset_pointer",
                "asset_pointer": "file-service://file_abc123",
                "size_bytes": 100,
                "width": 640,
                "height": 480,
            },
            "attachment": {
                "id": "file-service://file_abc123",
                "name": "image.png",
                "mimeType": "image/png",
                "size": 100,
                "width": 640,
                "height": 480,
            }
        }]);
        let req = default_request("text-davinci-002-render-sha");
        let payload = build_request_payload(&msgs, None, Some(&images), "text-davinci-002-render-sha", "parent-1", None, &req);
        let message = &payload["messages"][0];
        assert_eq!(message["content"]["content_type"], "multimodal_text");
        let parts = message["content"]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "What's in this image?");
        // Second part should be an image_asset_pointer object, not a plain string
        assert!(parts[1].is_object(), "image part should be an object");
        assert_eq!(parts[1]["content_type"], "image_asset_pointer");
        assert_eq!(parts[1]["asset_pointer"], "file-service://file_abc123");
        assert_eq!(parts[1]["width"], 640);
        assert_eq!(parts[1]["height"], 480);
        // Verify attachments metadata is present
        let metadata = message.get("metadata").unwrap();
        let attachments = metadata.get("attachments").unwrap().as_array().unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0]["id"], "file-service://file_abc123");
        assert_eq!(attachments[0]["name"], "image.png");
        assert_eq!(attachments[0]["mimeType"], "image/png");
    }

    // -----------------------------------------------------------------------
    // codex/responses API tests
    // -----------------------------------------------------------------------

    #[test]
    fn build_codex_payload_basic() {
        use crate::models::ChatContent;
        let msgs = vec![ChatMessage {
            role: "user".to_string(),
            content: ChatContent::String("Hello".to_string()),
            name: None,
            reasoning_content: None,
            citations: None,
            tool_calls: None,
            tool_call_id: None,
        }];
        let payload = build_codex_responses_payload(&msgs, "model-1", None, None, false);
        assert_eq!(payload["model"], "model-1");
        assert_eq!(payload["stream"], false);
        let input = payload["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "Hello");
    }

    #[test]
    fn build_codex_payload_streaming() {
        let msgs = vec![];
        let payload = build_codex_responses_payload(&msgs, "model-1", None, None, true);
        assert_eq!(payload["stream"], true);
    }

    #[test]
    fn build_codex_payload_with_tools() {
        let msgs = vec![];
        let tools = vec![Tool {
            r#type: "function".to_string(),
            function: crate::models::FunctionDefinition {
                name: "get_weather".to_string(),
                description: Some("Get weather".to_string()),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {"location": {"type": "string"}},
                    "required": ["location"]
                })),
                strict: None,
            },
        }];
        let payload = build_codex_responses_payload(&msgs, "model-1", Some(&tools), None, false);
        let tools_arr = payload["tools"].as_array().unwrap();
        assert_eq!(tools_arr.len(), 1);
        assert_eq!(tools_arr[0]["function"]["name"], "get_weather");
    }

    #[test]
    fn build_codex_payload_with_tool_choice() {
        let msgs = vec![];
        let choice = ToolChoice::Mode("auto".to_string());
        let payload = build_codex_responses_payload(&msgs, "model-1", None, Some(&choice), false);
        assert_eq!(payload["tool_choice"], "auto");
    }

    #[test]
    fn parse_codex_sse_text_delta() {
        let line = r#"data: {"type": "response.output_text.delta", "data": {"delta": "Hello, ", "index": 0}}"#;
        let result = parse_codex_sse_line(line);
        assert!(result.is_some());
        let (text, thinking, tool_calls) = result.unwrap();
        assert_eq!(text, Some("Hello, ".to_string()));
        assert!(thinking.is_none());
        assert!(tool_calls.is_none());
    }

    #[test]
    fn parse_codex_sse_thinking_delta() {
        let line = r#"data: {"type": "response.thinking.delta", "data": {"delta": "Let me think..."}}"#;
        let result = parse_codex_sse_line(line);
        assert!(result.is_some());
        let (text, thinking, tool_calls) = result.unwrap();
        assert!(text.is_none());
        assert_eq!(thinking, Some("Let me think...".to_string()));
        assert!(tool_calls.is_none());
    }

    #[test]
    fn parse_codex_sse_output_item_added() {
        let line = r#"data: {"type": "response.output_item.added", "data": {"object": {"id": "fc_123", "type": "function_call", "status": "in_progress", "call_id": "call_abc", "name": "get_weather"}}}"#;
        let result = parse_codex_sse_line(line);
        assert!(result.is_some());
        let (_text, _thinking, tool_calls) = result.unwrap();
        assert!(tool_calls.is_some(), "should have captured tool call data");
    }

    #[test]
    fn parse_codex_sse_output_item_done_with_function_call() {
        let line = r#"data: {"type": "response.output_item.done", "data": {"object": {"id": "fc_123", "type": "function_call", "status": "completed", "call_id": "call_abc", "name": "get_weather", "arguments": "{\"location\":\"Paris\"}"}}}"#;
        let result = parse_codex_sse_line(line);
        assert!(result.is_some());
        let (_text, _thinking, tool_calls) = result.unwrap();
        assert!(tool_calls.is_some(), "should have captured function call");
        let obj = tool_calls.unwrap();
        assert_eq!(obj["name"], "get_weather");
        assert_eq!(obj["arguments"], "{\"location\":\"Paris\"}");
    }

    #[test]
    fn parse_codex_sse_done() {
        let result = parse_codex_sse_line("data: [DONE]");
        assert!(result.is_none());
    }

    #[test]
    fn parse_codex_sse_invalid_event_returns_some_null_fields() {
        let line = r#"data: {"type": "response.unknown_event", "data": {}}"#;
        let result = parse_codex_sse_line(line);
        assert!(result.is_some());
        let (text, thinking, tool_calls) = result.unwrap();
        assert!(text.is_none());
        assert!(thinking.is_none());
        assert!(tool_calls.is_none());
    }
}

#[cfg(test)]
mod debug_capture_tests {
    use super::*;

    /// Regression test: ChatGPT's web endpoint sometimes delivers the final
    /// content as a consolidated `{"o":"patch","v":[{...}]}` event that has
    /// NO top-level `p` path. Before the fix, `parse_sse_line` returned `None`
    /// from the `p`-guard and the content was lost, so the model produced
    /// empty responses intermittently. This body is a real captured response.
    const CAPTURED_PATCH_WITHOUT_TOP_LEVEL_P: &str = r#"data: {"o": "patch", "v": [{"p": "/message/content/parts/0", "o": "append", "v": "4"}, {"p": "/message/status", "o": "replace", "v": "finished_successfully"}, {"p": "/message/end_turn", "o": "replace", "v": true}, {"p": "/message/metadata", "o": "append", "v": {"is_complete": true}}]}"#;

    #[test]
    fn parse_patch_without_top_level_p_extends_content() {
        let mut content = String::new();
        // parse_sse_line must now reach the patch branch despite no top-level p.
        let delta = parse_sse_line(CAPTURED_PATCH_WITHOUT_TOP_LEVEL_P, "")
            .and_then(|(d, _, _, _, _, _)| d);
        content.push_str(&delta.unwrap_or_default());
        assert_eq!(content, "4");
    }
}
