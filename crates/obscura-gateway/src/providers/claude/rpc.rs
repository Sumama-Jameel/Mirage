use uuid::Uuid;

use crate::models::ChatCompletionRequest;
use crate::models::Citation;
use crate::models::{ToolCall, FunctionCall};

/// Build the native Claude.ai completion payload for direct API calls.
pub fn build_native_request_payload(
    request: &ChatCompletionRequest,
    timezone: &str,
    search: bool,
    file_refs: &[String],
    image_refs: &[String],
) -> serde_json::Value {
    let prompt = request
        .messages
        .iter()
        .map(|m| {
            let role_prefix = match m.role.as_str() {
                "user" => "\n\nHuman: ",
                "assistant" => "\n\nAssistant: ",
                "system" => "\n\nSystem: ",
                "tool" => "\n\nTool: ",
                _ => "\n\nHuman: ",
            };
            format!("{}{}", role_prefix, m.content.as_text())
        })
        .collect::<Vec<_>>()
        .join("")
        .trim()
        .to_string()
        + "\n\nAssistant:";

    let sync_sources = if search {
        serde_json::json!([{"type": "web_search"}])
    } else {
        serde_json::Value::Array(vec![])
    };

    let attachments: Vec<serde_json::Value> = file_refs
        .iter()
        .map(|id| serde_json::json!({"id": id, "type": "file"}))
        .collect();
    let files: Vec<serde_json::Value> = image_refs
        .iter()
        .map(|id| serde_json::json!({"id": id, "type": "image", "width": 0, "height": 0}))
        .collect();

    let mut payload = serde_json::json!({
        "prompt": prompt,
        "model": request.model,
        "timezone": timezone,
        "locale": "en-US",
        "rendering_mode": "messages",
        "turn_message_uuids": {
            "human_message_uuid": Uuid::new_v4().to_string(),
            "assistant_message_uuid": Uuid::new_v4().to_string(),
        },
        "attachments": attachments,
        "files": files,
        "sync_sources": sync_sources,
    });

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
        payload["stop_sequences"] = serde_json::json!(s);
    }

    if let Some(p) = request.presence_penalty {
        payload["presence_penalty"] = serde_json::json!(p);
    }
    if let Some(f) = request.frequency_penalty {
        payload["frequency_penalty"] = serde_json::json!(f);
    }

    if let Some(ref tools) = request.tools {
        let tools_json: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.function.name,
                    "description": t.function.description.as_deref().unwrap_or(""),
                    "input_schema": t.function.parameters.clone().unwrap_or_else(|| serde_json::json!({"type": "object"})),
                })
            })
            .collect();
        payload["tools"] = serde_json::Value::Array(tools_json);
    }

    payload
}

/// Parse a single SSE line from the Claude streaming response.
///
/// When using direct claude.ai, the format is:
/// `data: {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "..."}}`
/// `data: {"type": "content_block_delta", "delta": {"type": "thinking_delta", "thinking": "..."}}`
/// `data: {"type": "content_block_start", "content_block": {"type": "tool_use", "id": "...", "name": "..."}}`
/// `data: {"type": "content_block_delta", "delta": {"type": "input_json_delta", "partial_json": "..."}}`
/// `data: {"type": "message_stop"}`
///
/// Returns `(delta_text, reasoning, citations, is_done, tool_calls)`.
#[allow(clippy::type_complexity)]
pub fn parse_sse_line(
    line: &str,
) -> Option<(Option<String>, Option<String>, Option<Vec<Citation>>, bool, Option<Vec<ToolCall>>)> {
    let body = line.strip_prefix("data: ")?;

    if body == "[DONE]" {
        return Some((None, None, None, true, None));
    }

    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return None;
    };

    let obj = json.as_object()?;

    // OpenAI format (worker proxy)
    if let Some(choices) = obj.get("choices").and_then(|v| v.as_array()) {
        if let Some(choice) = choices.first() {
            if let Some(delta) = choice.get("delta") {
                let text = delta.get("content").and_then(|v| v.as_str()).map(|s| s.to_string());
                let reasoning = delta.get("reasoning_content")
                    .or_else(|| delta.get("reasoning"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let citations = delta.get("citations").and_then(|v| v.as_array()).map(|arr| {
                    arr.iter()
                        .filter_map(|c| {
                            let obj = c.as_object()?;
                            Some(Citation {
                                index: obj.get("index").and_then(|v| v.as_i64()),
                                title: obj.get("title").and_then(|v| v.as_str()).map(|s| s.to_string()),
                                url: obj.get("url").and_then(|v| v.as_str()).map(|s| s.to_string()),
                                snippet: obj.get("snippet").and_then(|v| v.as_str()).map(|s| s.to_string()),
                                start_ix: obj.get("start_ix").or_else(|| obj.get("start_index")).and_then(|v| v.as_i64()),
                                end_ix: obj.get("end_ix").or_else(|| obj.get("end_index")).and_then(|v| v.as_i64()),
                            })
                        })
                        .collect::<Vec<Citation>>()
                }).filter(|v: &Vec<Citation>| !v.is_empty());

                let finish = choice.get("finish_reason")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty() && s != &"null");

                let is_done = finish.is_some();

                return Some((text, reasoning, citations, is_done, None));
            }
        }
        return None;
    }

    // Claude native format
    let event_type = obj.get("type").and_then(|v| v.as_str())?;

    match event_type {
        "content_block_delta" => {
            let delta = obj.get("delta")?;
            let delta_type = delta.get("type").and_then(|v| v.as_str())?;

            match delta_type {
                "text_delta" => {
                    let text = delta.get("text").and_then(|v| v.as_str()).map(|s| s.to_string());
                    Some((text, None, None, false, None))
                }
                "thinking_delta" => {
                    let thinking = delta.get("thinking").and_then(|v| v.as_str()).map(|s| s.to_string());
                    Some((None, thinking, None, false, None))
                }
                "signature_delta" => {
                    Some((None, None, None, false, None))
                }
                "citations_delta" => {
                    let citations = delta
                        .get("citations")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|c| {
                                    let obj = c.as_object()?;
                                    Some(Citation {
                                        index: obj.get("index").and_then(|v| v.as_i64()),
                                        title: obj.get("title").and_then(|v| v.as_str()).map(|s| s.to_string()),
                                        url: obj.get("url").and_then(|v| v.as_str()).map(|s| s.to_string()),
                                        snippet: obj.get("snippet").and_then(|v| v.as_str()).map(|s| s.to_string()),
                                        start_ix: obj.get("start_ix").or_else(|| obj.get("start_index")).and_then(|v| v.as_i64()),
                                        end_ix: obj.get("end_ix").or_else(|| obj.get("end_index")).and_then(|v| v.as_i64()),
                                    })
                                })
                                .collect::<Vec<Citation>>()
                        })
                        .filter(|v: &Vec<Citation>| !v.is_empty());
                    Some((None, None, citations, false, None))
                }
                "input_json_delta" => {
                    // Native tool call argument streaming — partial JSON chunks.
                    // We emit the partial_json as text so the caller can accumulate it.
                    // The actual ToolCall structure is emitted at content_block_start.
                    let _partial = delta.get("partial_json").and_then(|v| v.as_str());
                    // For now, pass through — tool calls are assembled in content_block_start.
                    Some((None, None, None, false, None))
                }
                _ => None,
            }
        }
        "content_block_start" => {
            let block = obj.get("content_block")?;
            let block_type = block.get("type").and_then(|v| v.as_str())?;
            match block_type {
                "thinking" => {
                    let thinking = block.get("thinking").and_then(|v| v.as_str()).map(|s| s.to_string());
                    Some((None, thinking, None, false, None))
                }
                "text" => {
                    let text = block.get("text").and_then(|v| v.as_str()).map(|s| s.to_string());
                    Some((text, None, None, false, None))
                }
                "tool_use" => {
                    // Native tool call start: extract id, name, and initial input.
                    let id = block.get("id").and_then(|v| v.as_str())
                        .unwrap_or("").to_string();
                    let name = block.get("name").and_then(|v| v.as_str())
                        .unwrap_or("").to_string();
                    let input = block.get("input")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "{}".to_string());
                    let call = ToolCall {
                        id: if id.is_empty() { format!("call_{}", Uuid::new_v4().simple()) } else { id },
                        r#type: "function".to_string(),
                        function: FunctionCall {
                            name,
                            arguments: input,
                        },
                    };
                    Some((None, None, None, false, Some(vec![call])))
                }
                "subscribed_citation" => {
                    let citations = block
                        .get("citation")
                        .and_then(|v| v.as_object())
                        .map(|obj| {
                            vec![Citation {
                                index: obj.get("index").and_then(|v| v.as_i64()),
                                title: obj.get("title").and_then(|v| v.as_str()).map(|s| s.to_string()),
                                url: obj.get("url").and_then(|v| v.as_str()).map(|s| s.to_string()),
                                snippet: obj.get("snippet").and_then(|v| v.as_str()).map(|s| s.to_string()),
                                start_ix: obj.get("start_ix").or_else(|| obj.get("start_index")).and_then(|v| v.as_i64()),
                                end_ix: obj.get("end_ix").or_else(|| obj.get("end_index")).and_then(|v| v.as_i64()),
                            }]
                        })
                        .filter(|v: &Vec<Citation>| !v.is_empty());
                    Some((None, None, citations, false, None))
                }
                _ => Some((None, None, None, false, None)),
            }
        }
        "message_stop" => {
            Some((None, None, None, true, None))
        }
        "error" => {
            Some((None, None, None, true, None))
        }
        "message_start" | "content_block_stop" | "message_delta" => {
            Some((None, None, None, false, None))
        }
        "ping" => {
            Some((None, None, None, false, None))
        }
        _ => None,
    }
}

/// Collect full text, citations, and native tool calls from a non-streaming SSE body.
pub fn collect_text_from_sse(body: &[u8]) -> (String, Option<String>, Vec<Citation>, Vec<ToolCall>) {
    let text = String::from_utf8_lossy(body);
    let mut content = String::new();
    let mut reasoning = None;
    let mut all_citations = Vec::new();
    let mut all_tool_calls = Vec::new();
    for line in text.lines() {
        if let Some((delta, think, citations, _, tool_calls)) = parse_sse_line(line) {
            if let Some(t) = delta {
                content.push_str(&t);
            }
            if let Some(t) = think {
                reasoning = Some(t);
            }
            if let Some(cits) = citations {
                for c in cits {
                    if !all_citations.iter().any(|existing: &Citation| existing.index == c.index && existing.url == c.url) {
                        all_citations.push(c);
                    }
                }
            }
            if let Some(calls) = tool_calls {
                for call in calls {
                    if !all_tool_calls.iter().any(|existing: &ToolCall| existing.id == call.id) {
                        all_tool_calls.push(call);
                    }
                }
            }
        }
    }
    (content, reasoning, all_citations, all_tool_calls)
}

/// Extract real token counts from a Claude SSE response body.
///
/// Supports both native Anthropic Messages API format and the worker-proxy
/// OpenAI-compatible format:
///
/// Native format:
///   `{"type":"message_start","message":{"usage":{"input_tokens":100,"output_tokens":5}}}`
///   `{"type":"message_delta","delta":{...},"usage":{"output_tokens":50}}`
///
/// Proxy/OpenAI format (final chunk):
///   `{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":50}}`
///
/// Returns `(input_tokens, output_tokens)` if both values are found in any
/// combination across the SSE stream, or `None` if neither provider returned
/// usage data.
pub fn extract_usage_from_sse(body: &[u8]) -> Option<(i32, i32)> {
    let text = String::from_utf8_lossy(body);
    let mut input_tokens: Option<i32> = None;
    let mut output_tokens: Option<i32> = None;

    for line in text.lines() {
        let Some(body) = line.strip_prefix("data: ") else {
            continue;
        };

        if body == "[DONE]" {
            continue;
        }

        let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
            continue;
        };
        let Some(obj) = json.as_object() else {
            continue;
        };

        // Native format: message_start carries input_tokens in message.usage
        if obj.get("type").and_then(|v| v.as_str()) == Some("message_start") {
            if let Some(usage) = obj.get("message").and_then(|m| m.get("usage")) {
                if input_tokens.is_none() {
                    input_tokens = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32);
                }
                if output_tokens.is_none() {
                    output_tokens = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32);
                }
            }
        }

        // Native format: message_delta carries the final output_tokens
        if obj.get("type").and_then(|v| v.as_str()) == Some("message_delta") {
            if let Some(usage) = obj.get("usage") {
                output_tokens = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_i64())
                    .map(|v| v as i32);
            }
        }

        // Proxy/OpenAI format: top-level usage field in the final chunk
        if let Some(usage) = obj.get("usage") {
            if input_tokens.is_none() {
                input_tokens = usage
                    .get("prompt_tokens")
                    .and_then(|v| v.as_i64())
                    .map(|v| v as i32);
            }
            if output_tokens.is_none() {
                output_tokens = usage
                    .get("completion_tokens")
                    .and_then(|v| v.as_i64())
                    .map(|v| v as i32);
            }
        }
    }

    Some((input_tokens?, output_tokens?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ChatMessage;

    #[test]
    fn parse_openai_format() {
        let line = r#"data: {"choices":[{"delta":{"content":"Hello, "},"index":0}]}"#;
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (text, _, _, done, _) = result.unwrap();
        assert_eq!(text, Some("Hello, ".to_string()));
        assert!(!done);
    }

    #[test]
    fn parse_openai_format_reasoning() {
        let line = r#"data: {"choices":[{"delta":{"reasoning_content":"thinking..."},"index":0}]}"#;
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (_, reasoning, _, _, _) = result.unwrap();
        assert_eq!(reasoning, Some("thinking...".to_string()));
    }

    #[test]
    fn parse_openai_finish() {
        let line = r#"data: {"choices":[{"delta":{},"finish_reason":"stop","index":0}]}"#;
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (_, _, _, done, _) = result.unwrap();
        assert!(done);
    }

    #[test]
    fn parse_claude_text_delta() {
        let line = r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}"#;
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (text, _, _, done, _) = result.unwrap();
        assert_eq!(text, Some("Hello".to_string()));
        assert!(!done);
    }

    #[test]
    fn parse_claude_thinking_delta() {
        let line = r#"data: {"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"thinking..."}}"#;
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (_, thinking, _, _, _) = result.unwrap();
        assert_eq!(thinking, Some("thinking...".to_string()));
    }

    #[test]
    fn parse_claude_message_stop() {
        let line = r#"data: {"type":"message_stop"}"#;
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (_, _, _, done, _) = result.unwrap();
        assert!(done);
    }

    #[test]
    fn parse_done_marker() {
        let line = "data: [DONE]";
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (_, _, _, done, _) = result.unwrap();
        assert!(done);
    }

    #[test]
    fn extract_usage_native_format() {
        let body = br#"data: {"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-3-5-sonnet-20241022","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":100,"output_tokens":5}}}
data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":50}}
data: {"type":"message_stop"}
"#;
        let result = extract_usage_from_sse(body);
        assert!(result.is_some());
        let (input, output) = result.unwrap();
        assert_eq!(input, 100);
        assert_eq!(output, 50);
    }

    #[test]
    fn extract_usage_proxy_format() {
        let body = br#"data: {"choices":[{"delta":{"content":"Hello"},"index":0}]}
data: {"choices":[{"delta":{},"finish_reason":"stop","index":0}],"usage":{"prompt_tokens":80,"completion_tokens":30}}
"#;
        let result = extract_usage_from_sse(body);
        assert!(result.is_some());
        let (input, output) = result.unwrap();
        assert_eq!(input, 80);
        assert_eq!(output, 30);
    }

    #[test]
    fn extract_usage_no_usage() {
        let body = br#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}
data: {"type":"message_stop"}
"#;
        let result = extract_usage_from_sse(body);
        assert!(result.is_none());
    }

    #[test]
    fn build_native_search_payload_populates_sync_sources() {
        use crate::models::ChatCompletionRequest;
        let request = ChatCompletionRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: crate::models::ChatContent::String("Search query".to_string()),
                name: None,
                reasoning_content: None,
                citations: None,
                tool_calls: None,
                tool_call_id: None,
            }],
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
        let payload = build_native_request_payload(&request, "America/New_York", true, &[], &[]);
        let sources = payload["sync_sources"].as_array().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0]["type"], "web_search");
    }

    #[test]
    fn parse_claude_citations_delta() {
        let line = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citations":[{"index":0,"title":"Wikipedia","url":"https://en.wikipedia.org/wiki/France","snippet":"France is a country","start_ix":0,"end_ix":15}]}}"#;
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (text, reasoning, citations, done, _) = result.unwrap();
        assert!(text.is_none());
        assert!(reasoning.is_none());
        assert!(!done);
        let cits = citations.unwrap();
        assert_eq!(cits.len(), 1);
        assert_eq!(cits[0].title.as_deref(), Some("Wikipedia"));
        assert_eq!(cits[0].url.as_deref(), Some("https://en.wikipedia.org/wiki/France"));
        assert_eq!(cits[0].snippet.as_deref(), Some("France is a country"));
        assert_eq!(cits[0].index, Some(0));
        assert_eq!(cits[0].start_ix, Some(0));
        assert_eq!(cits[0].end_ix, Some(15));
    }

    #[test]
    fn parse_claude_subscribed_citation() {
        let line = r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"subscribed_citation","citation":{"index":0,"title":"Britannica","url":"https://britannica.com/paris","snippet":"Paris is the capital"}}}"#;
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (text, reasoning, citations, done, _) = result.unwrap();
        assert!(text.is_none());
        assert!(reasoning.is_none());
        assert!(!done);
        let cits = citations.unwrap();
        assert_eq!(cits.len(), 1);
        assert_eq!(cits[0].title.as_deref(), Some("Britannica"));
        assert_eq!(cits[0].url.as_deref(), Some("https://britannica.com/paris"));
        assert_eq!(cits[0].index, Some(0));
    }

    #[test]
    fn parse_claude_citations_delta_empty_returns_none() {
        let line = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citations":[]}}"#;
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (_, _, citations, _, _) = result.unwrap();
        assert!(citations.is_none());
    }

    #[test]
    fn parse_openai_format_with_citations() {
        let line = r#"data: {"choices":[{"delta":{"content":"answer","citations":[{"index":0,"title":"Src","url":"https://src.com","snippet":"source"}]},"index":0}]}"#;
        let result = parse_sse_line(line);
        assert!(result.is_some());
        let (text, _, citations, _, _) = result.unwrap();
        assert_eq!(text, Some("answer".to_string()));
        let cits = citations.unwrap();
        assert_eq!(cits.len(), 1);
        assert_eq!(cits[0].url.as_deref(), Some("https://src.com"));
    }

    #[test]
    fn collect_text_from_sse_accumulates_citations() {
        let body = br#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Based on sources"}}
data: {"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citations":[{"index":0,"title":"Src1","url":"https://src1.com","snippet":"first source","start_ix":0,"end_ix":10}]}}
data: {"type":"content_block_start","index":1,"content_block":{"type":"subscribed_citation","citation":{"index":1,"title":"Src2","url":"https://src2.com","snippet":"second source"}}}
data: {"type":"content_block_start","index":2,"content_block":{"type":"text","text":" here is the answer"}}
data: {"type":"message_stop"}
"#;
        let (content, _, citations, _) = collect_text_from_sse(body);
        assert_eq!(content, "Based on sources here is the answer");
        assert_eq!(citations.len(), 2);
        assert_eq!(citations[0].url.as_deref(), Some("https://src1.com"));
        assert_eq!(citations[1].url.as_deref(), Some("https://src2.com"));
    }

    #[test]
    fn build_native_no_search_has_empty_sync_sources() {
        use crate::models::ChatCompletionRequest;
        let request = ChatCompletionRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: crate::models::ChatContent::String("Hello".to_string()),
                name: None,
                reasoning_content: None,
                citations: None,
                tool_calls: None,
                tool_call_id: None,
            }],
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
        let payload = build_native_request_payload(&request, "America/New_York", false, &[], &[]);
        let sources = payload["sync_sources"].as_array().unwrap();
        assert!(sources.is_empty());
    }

    #[test]
    fn build_native_payload_forwards_presence_penalty() {
        use crate::models::ChatCompletionRequest;
        let mut request = ChatCompletionRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: crate::models::ChatContent::String("Hello".to_string()),
                name: None,
                reasoning_content: None,
                citations: None,
                tool_calls: None,
                tool_call_id: None,
            }],
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
        request.presence_penalty = Some(0.5);
        let payload = build_native_request_payload(&request, "America/New_York", false, &[], &[]);
        let val = payload["presence_penalty"].as_f64().unwrap();
        assert!((val - 0.5_f64).abs() < 1e-6, "expected ~0.5, got {val}");
    }

    #[test]
    fn build_native_payload_forwards_frequency_penalty() {
        use crate::models::ChatCompletionRequest;
        let mut request = ChatCompletionRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: crate::models::ChatContent::String("Hello".to_string()),
                name: None,
                reasoning_content: None,
                citations: None,
                tool_calls: None,
                tool_call_id: None,
            }],
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
        request.frequency_penalty = Some(-0.3);
        let payload = build_native_request_payload(&request, "America/New_York", false, &[], &[]);
        let val = payload["frequency_penalty"].as_f64().unwrap();
        assert!((val - (-0.3_f64)).abs() < 1e-6, "expected ~-0.3, got {val}");
    }

    #[test]
    fn build_native_payload_omits_penalties_when_not_set() {
        use crate::models::ChatCompletionRequest;
        let request = ChatCompletionRequest {
            model: "claude-sonnet-4-6".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: crate::models::ChatContent::String("Hello".to_string()),
                name: None,
                reasoning_content: None,
                citations: None,
                tool_calls: None,
                tool_call_id: None,
            }],
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
        let payload = build_native_request_payload(&request, "America/New_York", false, &[], &[]);
        assert!(payload.get("presence_penalty").is_none());
        assert!(payload.get("frequency_penalty").is_none());
    }
}
