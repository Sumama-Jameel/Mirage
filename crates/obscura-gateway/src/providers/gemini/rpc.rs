use serde_json::Value;
use crate::error::GatewayError;
use crate::models::{Citation, Tool, ToolCall, FunctionCall};
use crate::providers::tool_call::{
    is_tool_call_in_progress, parse_gemini_tool_calls, strip_gemini_card_prefix,
    strip_tool_call_markers,
};

const STREAM_GENERATE_PATH: &str = "/_/BardChatUi/data/assistant.lamda.BardFrontendService/StreamGenerate";

pub fn stream_generate_url() -> &'static str {
    STREAM_GENERATE_PATH
}

/// Holds conversation state for multi-turn conversations.
#[derive(Debug, Clone, Default)]
pub struct ConversationState {
    pub conversation_id: String,
    pub response_id: String,
    pub choice_id: String,
}

/// Response data extracted from a single Gemini RPC frame.
#[derive(Debug, Clone, Default)]
pub struct ResponseData {
    pub text: String,
    pub thinking: Option<String>,
    pub conversation: Option<ConversationState>,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub citations: Option<Vec<Citation>>,
}

/// Build the inner request payload for Gemini's StreamGenerate endpoint.
///
/// Produces an 80-element array matching the real browser's payload structure.
/// Known indices:
///   [0]  prompt + images
///   [1]  language
///   [2]  conversation state (10-element array)
///   [6]  [0] or [1] (search flag: 0=off, 1=on)
///   [7]  1 (streaming flag)
///   [8]  tool call results (empty list)
///   [9]  tools
///   [10] 1
///   [11] 0
///   [17] [[think_mode]]
///   [18] 0
///   [27] 1
///   [30] [4]
///   [41] [2]
///   [53] 0
///   [59] request UUID
///   [61] []
///   [68] 1
///   [79] model_mode (MODE_CATEGORY)
pub fn build_request_payload(
    prompt: &str,
    conversation: Option<&ConversationState>,
    tools: Option<&[Tool]>,
    image_list: Option<&Value>,
    think_mode: u32,
    model_mode: u32,
    request_uuid: &str,
    search: bool,
) -> serde_json::Result<Value> {
    let (conv_id, resp_id, choice_id) = match conversation {
        Some(c) => (
            Some(c.conversation_id.as_str()),
            Some(c.response_id.as_str()),
            Some(c.choice_id.as_str()),
        ),
        None => (None, None, None),
    };

    let tools_val = if let Some(tool_list) = tools {
        Value::Array(
            tool_list
                .iter()
                .map(|t| {
                    let params = t
                        .function
                        .parameters
                        .as_ref()
                        .map(|p| p.to_string())
                        .unwrap_or_default();
                    let desc = t
                        .function
                        .description
                        .as_deref()
                        .unwrap_or("");
                    Value::Array(vec![
                        Value::String(t.function.name.clone()),
                        Value::String(desc.to_string()),
                        Value::String(params),
                    ])
                })
                .collect(),
        )
    } else {
        Value::Array(vec![])
    };

    let images = image_list
        .cloned()
        .unwrap_or(Value::Array(vec![]));

    let mut request: Vec<Value> = (0..80).map(|_| Value::Null).collect();

    // [0] prompt block
    request[0] = Value::Array(vec![
        Value::String(prompt.to_string()),
        Value::Number(0.into()),
        Value::Null,
        images,
        Value::Null,
        Value::Null,
        Value::Number(0.into()),
    ]);

    // [1] language
    request[1] = Value::Array(vec![Value::String("en".to_string())]);

    // [2] conversation state (10-element array matching browser)
    request[2] = Value::Array(vec![
        conv_id.map(|s| Value::String(s.to_string())).unwrap_or(Value::Null),
        resp_id.map(|s| Value::String(s.to_string())).unwrap_or(Value::Null),
        choice_id.map(|s| Value::String(s.to_string())).unwrap_or(Value::Null),
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
    ]);

    // [6] search flag: 0=off, 1=on (index 6 of the request array).
    // Honor the caller's `search` toggle instead of hardcoding search on.
    request[6] = Value::Array(vec![Value::Number(
        if search { 1 } else { 0 }.into()
    )]);

    // [7] streaming flag
    request[7] = Value::Number(1.into());

    // [8] tool call results (empty list)
    request[8] = Value::Array(vec![]);

    // [9] tools
    request[9] = tools_val;

    // [10]
    request[10] = Value::Number(1.into());

    // [11]
    request[11] = Value::Number(0.into());

    // [17] thinking depth: [[think_mode]]
    request[17] = Value::Array(vec![Value::Array(vec![Value::Number(think_mode.into())])]);

    // [18]
    request[18] = Value::Number(0.into());

    // [27]
    request[27] = Value::Number(1.into());

    // [30]
    request[30] = Value::Array(vec![Value::Number(4.into())]);

    // [41]
    request[41] = Value::Array(vec![Value::Number(2.into())]);

    // [53]
    request[53] = Value::Number(0.into());

    // [59] per-request UUID
    request[59] = Value::String(request_uuid.to_string());

    // [61] empty array
    request[61] = Value::Array(vec![]);

    // [68]
    request[68] = Value::Number(1.into());

    // [79] model_mode (MODE_CATEGORY)
    request[79] = Value::Number(model_mode.into());

    serde_json::to_value(request)
}

/// Wrap the inner payload into the outer `f.req` envelope.
pub fn build_f_req(inner: &Value) -> serde_json::Result<String> {
    let inner_json = serde_json::to_string(inner)?;
    let outer = vec![Value::Null, Value::String(inner_json)];
    serde_json::to_string(&outer)
}

/// Regex for stripping citation markers like `[citation:1]` or `[citation:1:2]`
fn citation_marker_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\[citation:\d+(?::\d+)?\]").unwrap())
}

/// Strip inline citation markers from text.
pub fn strip_citation_markers(text: &str) -> String {
    citation_marker_regex().replace_all(text, "").to_string()
}

/// Parse a single line from a Gemini StreamGenerate response.
///
/// Each line: `["wrb.fr","<method>","<payload>",null,null,null]`
/// May be double-wrapped as `[["wrb.fr",...]]`.
///
/// Content positions:
/// - New format: `payload[4][0][1][0]`
/// - Old format: `payload[0][2]`
pub fn parse_response_line(line: &str) -> Option<ResponseData> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(")]}'") || line.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    let parsed: Value = serde_json::from_str(line).ok()?;
    let arr = parsed.as_array()?;

    let arr = if arr.len() == 1 && arr[0].is_array() {
        arr[0].as_array().unwrap()
    } else {
        arr
    };

    if arr.first()?.as_str()? != "wrb.fr" {
        return None;
    }

    let payload_str = arr.get(2)?.as_str()?;
    let payload: Value = serde_json::from_str(payload_str).ok()?;
    let payload_arr = payload.as_array()?;

    let raw_text = extract_content_new(payload_arr)
        .or_else(|| extract_content_old(payload_arr))
        .unwrap_or_default();

    let thinking = extract_thinking(payload_arr);
    let conversation = extract_conversation(payload_arr);
    let tool_calls = extract_tool_calls(payload_arr, &raw_text);
    let citations = extract_citations(payload_arr);

    // Gemini wraps some responses in a `card_content` URL prefix (tool calls,
    // weather cards). The real rendered text lives at candidate[22][0] in that
    // case; use it as the display text when no tool call was extracted. When a
    // tool call was extracted, keep the text clean (card render must not leak).
    let text = if tool_calls.is_none() {
        if is_tool_call_in_progress(&raw_text) {
            // The call is still streaming (unclosed paren/fence): suppress the
            // partial text so it never reaches the client as content.
            String::new()
        } else {
            let candidate_text = if raw_text.contains("googleusercontent.com/card_content/") {
                extract_card_fallback(payload_arr)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| strip_citation_markers(&raw_text))
            } else {
                strip_citation_markers(&raw_text)
            };
            clean_response_text(&strip_gemini_card_prefix(&candidate_text))
        }
    } else {
        String::new()
    };

    Some(ResponseData {
        text,
        thinking,
        conversation,
        tool_calls,
        citations,
    })
}

/// Extract the real rendered text at `payload[4][0][22][0]` when a response
/// carries the `card_content` marker.
fn extract_card_fallback(payload: &[Value]) -> Option<String> {
    let candidates = payload.get(4)?.as_array()?;
    let candidate = candidates.first()?.as_array()?;
    let slot = candidate.get(22)?.as_array()?;
    slot.first()?.as_str().map(String::from)
}

/// Extract conversation state from a raw line.
pub fn extract_conversation_from_line(line: &str) -> Option<ConversationState> {
    let data = parse_response_line(line)?;
    data.conversation
}

/// New format: `payload[4][0][1][0]`
fn extract_content_new(payload: &[Value]) -> Option<String> {
    let candidates = payload.get(4)?.as_array()?;
    let first = candidates.first()?.as_array()?;
    let texts = first.get(1)?.as_array()?;
    texts.first()?.as_str().map(String::from)
}

/// Old format: `payload[0][2]`
fn extract_content_old(payload: &[Value]) -> Option<String> {
    let inner = payload.first()?.as_array()?;
    inner.get(2)?.as_str().map(String::from)
}

/// Extract thinking/reasoning using gpt4free's `find_str(data, 3)` pattern.
fn extract_thinking(payload: &[Value]) -> Option<String> {
    let candidates = payload.get(4)?.as_array()?;
    let candidate = candidates.first()?.as_array()?;
    let mut parts: Vec<&str> = Vec::new();
    for item in candidate.iter().skip(3) {
        collect_strings(item, &mut parts);
    }
    if parts.is_empty() {
        return None;
    }
    let joined = parts.join("\n\n");
    if joined.trim().is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Recursively collect string values from JSON.
fn collect_strings<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(s) => {
            if !s.starts_with("$AQ") && s != "image/png" && s != "imagen_default" {
                out.push(s);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                collect_strings(item, out);
            }
        }
        _ => {}
    }
}

/// Extract conversation state: payload[1] = [conv_id, resp_id, ...]
/// payload[4][0][0] = choice_id
fn extract_conversation(payload: &[Value]) -> Option<ConversationState> {
    let meta = payload.get(1)?.as_array()?;
    let conversation_id = meta.first()?.as_str()?;
    let response_id = meta.get(1)?.as_str()?;
    let candidates = payload.get(4)?.as_array()?;
    let choice_id = candidates.first()?.as_array()?.first()?.as_str()?;
    Some(ConversationState {
        conversation_id: conversation_id.to_string(),
        response_id: response_id.to_string(),
        choice_id: choice_id.to_string(),
    })
}

/// Extract tool calls from response: native candidate[28], then Gemini text
/// formats (fenced JSON / function_call blocks / inline `name(args)` / XML).
fn extract_tool_calls(payload: &[Value], text: &str) -> Option<Vec<ToolCall>> {
    if let Some(native) = extract_native_tool_calls(payload) {
        if !native.is_empty() {
            return Some(native);
        }
    }
    if let Some(gemini) = parse_gemini_tool_calls(text) {
        if !gemini.is_empty() {
            return Some(gemini);
        }
    }
    None
}

/// Extract native Gemini tool calls from candidate[28] or search candidate slots.
fn extract_native_tool_calls(payload: &[Value]) -> Option<Vec<ToolCall>> {
    let candidates = payload.get(4)?.as_array()?;
    let candidate = candidates.first()?.as_array()?;

    let mut tool_data_opt = candidate.get(28).and_then(|v| v.as_array());
    if tool_data_opt.is_none() || tool_data_opt.map_or(true, |a| a.is_empty()) {
        for elem in candidate.iter() {
            if let Some(arr) = elem.as_array() {
                if !arr.is_empty() {
                    if let Some(first_call) = arr.first().and_then(|v| v.as_array()) {
                        if first_call.len() >= 2 && first_call.get(0).and_then(|v| v.as_str()).is_some() {
                            tool_data_opt = Some(arr);
                            break;
                        }
                    }
                }
            }
        }
    }

    let tool_data = tool_data_opt?;
    let mut calls = Vec::new();

    for entry in tool_data {
        let arr = entry.as_array()?;
        let name = arr.get(0)?.as_str()?;
        let args = match arr.get(2) {
            Some(Value::String(s)) => s.clone(),
            Some(v @ Value::Object(_)) => v.to_string(),
            _ => "{}".to_string(),
        };
        let id = arr
            .get(3)
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let call_id = if id.is_empty() {
            format!("call_{}", uuid::Uuid::new_v4().simple())
        } else {
            id.to_string()
        };

        calls.push(ToolCall {
            id: call_id,
            r#type: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: args,
            },
        });
    }

    if calls.is_empty() {
        None
    } else {
        Some(calls)
    }
}

/// Extract citations from the response payload.
///
/// Locations checked (in order):
/// 1. `payload[4][0][12]` — Gemini's source/citation block
/// 2. Individual citation objects within the source block
///
/// Each citation object can contain: url/link, title/name, snippet/content.
fn extract_citations(payload: &[Value]) -> Option<Vec<Citation>> {
    let candidates = payload.get(4)?.as_array()?;
    let candidate = candidates.first()?.as_array()?;

    // Try payload[4][0][12] where source data lives
    let source_block = candidate.get(12)?.as_array()?;

    let mut citations = Vec::new();

    // Check common citation sub-locations within source_block
    for idx in [0usize, 1, 2, 6, 7] {
        if let Some(entry) = source_block.get(idx) {
            if let Some(arr) = entry.as_array() {
                for item in arr {
                    if let Some(cite) = parse_citation_value(item) {
                        citations.push(cite);
                    }
                }
            } else if let Some(cite) = parse_citation_value(entry) {
                citations.push(cite);
            }
        }
    }

    if citations.is_empty() {
        None
    } else {
        Some(citations)
    }
}

/// Parse a single citation value into a `Citation`.
fn parse_citation_value(value: &Value) -> Option<Citation> {
    if let Some(obj) = value.as_object() {
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
            .get("index")
            .or_else(|| obj.get("cite_index"))
            .and_then(|v| v.as_i64());

        if url.is_none() && title.is_none() && snippet.is_none() {
            return None;
        }

        return Some(Citation {
            index,
            title,
            url,
            snippet,
            start_ix: None,
            end_ix: None,
        });
    } else if let Some(arr) = value.as_array() {
        if arr.is_empty() {
            return None;
        }
        let first_str = arr.get(0).and_then(|v| v.as_str());
        let second_str = arr.get(1).and_then(|v| v.as_str());
        let third_str = arr.get(2).and_then(|v| v.as_str());

        let (url, title, snippet) = if let Some(s) = first_str {
            if s.starts_with("http://") || s.starts_with("https://") {
                (Some(s.to_string()), second_str.map(|v| v.to_string()), third_str.map(|v| v.to_string()))
            } else if let Some(u) = second_str {
                if u.starts_with("http://") || u.starts_with("https://") {
                    (Some(u.to_string()), Some(s.to_string()), third_str.map(|v| v.to_string()))
                } else {
                    return None;
                }
            } else {
                return None;
            }
        } else {
            return None;
        };

        return Some(Citation {
            index: None,
            title,
            url,
            snippet,
            start_ix: None,
            end_ix: None,
        });
    }

    None
}

/// Parse the full response body, taking the LAST frame's data.
pub fn parse_full_response(body: &str) -> Result<ResponseData, GatewayError> {
    let mut last_text = String::new();
    let mut last_thinking: Option<String> = None;
    let mut last_conversation: Option<ConversationState> = None;
    let mut last_tool_calls: Option<Vec<ToolCall>> = None;
    let mut last_citations: Option<Vec<Citation>> = None;
    let mut frames_found = 0;

    for line in body.lines() {
        if let Some(data) = parse_response_line(line) {
            frames_found += 1;
            if !data.text.is_empty() {
                last_text = data.text;
            }
            if data.thinking.is_some() {
                last_thinking = data.thinking;
            }
            if data.conversation.is_some() {
                last_conversation = data.conversation;
            }
            if data.tool_calls.is_some() {
                last_tool_calls = data.tool_calls;
            }
            if data.citations.is_some() {
                last_citations = data.citations;
            }
        }
    }

    if frames_found == 0 {
        let preview: String = body.chars().take(200).collect();
        return Err(GatewayError::Provider(format!(
            "Gemini response contained no parseable frames. Body preview: {preview}"
        )));
    }

    Ok(ResponseData {
        text: last_text,
        thinking: last_thinking,
        conversation: last_conversation,
        tool_calls: last_tool_calls,
        citations: last_citations,
    })
}

/// Parse a streaming chunk, returning delta from previous_text.
///
/// Returns `(delta, thinking, tool_calls, citations, full_text)` — the 5th
/// element is the complete response text at this point. Callers MUST use it
/// to reset their `previous_text` tracker so non-prefix text revisions
/// (Gemini editing its output mid-stream) don't corrupt the accumulator.
pub fn parse_streaming_chunk(
    line: &str,
    previous_text: &str,
) -> Result<
    Option<(
        String,
        Option<String>,
        Option<Vec<ToolCall>>,
        Option<Vec<Citation>>,
        String,
    )>,
    GatewayError,
> {
    let data = match parse_response_line(line) {
        Some(d) => d,
        None => return Ok(None),
    };

    // Fast path: no text, no tool calls, no citations, no thinking — skip.
    if data.text.is_empty()
        && data.tool_calls.is_none()
        && data.citations.is_none()
        && data.thinking.is_none()
    {
        return Ok(None);
    }

    let (delta, tool_calls, citations) = if !data.text.is_empty() {
        if data.text == previous_text {
            // Text unchanged, but tool calls or citations may have changed.
            // If tool calls or citations arrived, emit them with empty delta.
            if data.tool_calls.is_some() || data.citations.is_some() {
                (String::new(), data.tool_calls, data.citations)
            } else {
                return Ok(None);
            }
        } else {
            let delta = if data.text.len() > previous_text.len()
                && data.text.starts_with(previous_text)
            {
                data.text
                    .get(previous_text.len()..)
                    .unwrap_or(&data.text)
                    .to_string()
            } else {
                data.text.clone()
            };
            (delta, data.tool_calls, data.citations)
        }
    } else {
        (String::new(), data.tool_calls, data.citations)
    };

    if delta.is_empty()
        && tool_calls.is_none()
        && citations.is_none()
        && data.thinking.is_none()
    {
        return Ok(None);
    }
    Ok(Some((
        delta,
        data.thinking,
        tool_calls,
        citations,
        data.text.clone(),
    )))
}

/// Strip tool call markers and citation markers from text for clean output.
pub fn clean_response_text(text: &str) -> String {
    let no_tools = strip_tool_call_markers(text);
    strip_citation_markers(&no_tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrb_line(payload: &str) -> String {
        format!(r#"[["wrb.fr","hNvQHb",{payload:?},null,null,null]]"#)
    }

    #[test]
    fn parse_old_format_content() {
        let line = wrb_line(r#"[[null,null,"Hello! How can I help?",null,null,[]]]"#);
        let data = parse_response_line(&line).unwrap();
        assert_eq!(data.text, "Hello! How can I help?");
    }

    #[test]
    fn parse_full_response_takes_last_text() {
        let body = format!(
            ")]}}'\n\n12\n{}\n14\n{}\n",
            wrb_line(r#"[[null,null,"First text",null,null,[]]]"#),
            wrb_line(r#"[[null,null,"Final text",null,null,[]]]"#)
        );
        let result = parse_full_response(&body).unwrap();
        assert_eq!(result.text, "Final text");
    }

    #[test]
    fn parse_empty_body_returns_error() {
        let result = parse_full_response(")]}'\n\n");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("no parseable frames"), "got: {err}");
    }

    fn test_uuid() -> &'static str {
        "00000000-0000-0000-0000-000000000000"
    }

    #[test]
    fn build_request_payload_basic() {
        let payload = build_request_payload("hello", None, None, None, 4, 1, test_uuid(), false).unwrap();
        let arr = payload.as_array().unwrap();
        assert_eq!(arr.len(), 80);
        assert_eq!(arr[0][0], "hello");
        assert_eq!(arr[1][0], "en");
        assert!(arr[2][0].is_null());
        assert_eq!(arr[9].as_array().unwrap().len(), 0);
        assert_eq!(arr[7], 1);
        assert_eq!(arr[79], 1);
        assert_eq!(arr[59], "00000000-0000-0000-0000-000000000000");
        assert_eq!(arr[17][0][0], 4);
    }

    #[test]
    fn build_request_payload_search_flag_honors_toggle() {
        // Search disabled: index 6 must be [0], not the old hardcoded [1].
        let off = build_request_payload("q", None, None, None, 4, 1, test_uuid(), false).unwrap();
        let arr_off = off.as_array().unwrap();
        assert_eq!(arr_off[6].as_array().unwrap()[0], 0);

        // Search enabled: index 6 must be [1].
        let on = build_request_payload("q", None, None, None, 4, 1, test_uuid(), true).unwrap();
        let arr_on = on.as_array().unwrap();
        assert_eq!(arr_on[6].as_array().unwrap()[0], 1);
    }

    #[test]
    fn build_request_payload_with_conversation() {
        let conv = ConversationState {
            conversation_id: "conv-1".to_string(),
            response_id: "resp-1".to_string(),
            choice_id: "choice-1".to_string(),
        };
        let payload = build_request_payload("follow up", Some(&conv), None, None, 4, 1, test_uuid(), false).unwrap();
        let arr = payload.as_array().unwrap();
        assert_eq!(arr[2][0], "conv-1");
        assert_eq!(arr[2][1], "resp-1");
        assert_eq!(arr[2][2], "choice-1");
    }

    #[test]
    fn build_request_payload_with_tools() {
        let tools = vec![Tool {
            r#type: "function".to_string(),
            function: crate::models::FunctionDefinition {
                name: "search".to_string(),
                description: Some("Search the web".to_string()),
                parameters: Some(serde_json::json!({"type":"object"})),
                strict: None,
            },
        }];
        let payload = build_request_payload("search", None, Some(&tools), None, 4, 1, test_uuid(), false).unwrap();
        let arr = payload.as_array().unwrap();
        assert_eq!(arr[9].as_array().unwrap().len(), 1);
        assert_eq!(arr[9][0][0], "search");
        assert_eq!(arr[9][0][1], "Search the web");
    }

    #[test]
    fn streaming_chunk_delta() {
        let line1 = wrb_line(r#"[[null,null,"Hel",null,null,[]]]"#);
        let line2 = wrb_line(r#"[[null,null,"Hello",null,null,[]]]"#);

        let result1 = parse_streaming_chunk(&line1, "").unwrap();
        assert_eq!(result1.as_ref().map(|(d, _, _, _, _)| d.as_str()), Some("Hel"));

        let result2 = parse_streaming_chunk(&line2, "Hel").unwrap();
        assert_eq!(result2.as_ref().map(|(d, _, _, _, _)| d.as_str()), Some("lo"));
    }

    #[test]
    fn parse_actual_gemini_format() {
        let inner = r#"[null,["c_conv","r_resp"],null,null,[["rc_choice",["Hey."],null,null,null,null,null,null,[1],"en",null,null,[null,null,null,null,null,null,[]],null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,[],null,null,null,null,null,null,null,null,[]]]]"#;
        let line = wrb_line(inner);
        let data = parse_response_line(&line).unwrap();
        assert_eq!(data.text, "Hey.");
        assert_eq!(data.conversation.as_ref().unwrap().conversation_id, "c_conv");
        assert_eq!(data.conversation.as_ref().unwrap().response_id, "r_resp");
        assert_eq!(data.conversation.as_ref().unwrap().choice_id, "rc_choice");
    }

    #[test]
    fn handles_single_wrapped_format() {
        let inner = r#"[[null,null,"Single wrapped",null,null,[]]]"#;
        let line = format!(r#"["wrb.fr","hNvQHb",{inner:?},null,null,null]"#);
        let data = parse_response_line(&line).unwrap();
        assert_eq!(data.text, "Single wrapped");
    }

    #[test]
    fn streaming_skips_duplicate_frame_no_tool_calls() {
        let line = wrb_line(r#"[[null,null,"Hello",null,null,[]]]"#);
        let result = parse_streaming_chunk(&line, "Hello").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn streaming_unchanged_text_with_tool_calls_emits_them() {
        // Text is "Hello" same as previous, but candidate[28] has a tool call.
        let inner = r#"[null,["c","r"],null,null,[["ch",["Hello"],null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,[["search",null,"{}","call_1"]]]]]"#;
        let line = wrb_line(inner);
        let result = parse_streaming_chunk(&line, "Hello").unwrap();
        assert!(result.is_some());
        let (delta, _, tool_calls, _, _) = result.unwrap();
        assert_eq!(delta, ""); // no text change
        assert!(tool_calls.is_some());
        assert_eq!(tool_calls.unwrap()[0].function.name, "search");
    }

    #[test]
    fn streaming_non_prefix_delta_emits_full_text() {
        let line = wrb_line(r#"[[null,null,"Completely different",null,null,[]]]"#);
        let result = parse_streaming_chunk(&line, "Hello").unwrap();
        assert_eq!(result.as_ref().map(|(d, _, _, _, _)| d.as_str()), Some("Completely different"));
    }

    #[test]
    fn skips_length_prefix_lines() {
        assert!(parse_response_line("1512").is_none());
        assert!(parse_response_line(")]}'").is_none());
        assert!(parse_response_line("").is_none());
    }

    #[test]
    fn extract_thinking_from_response() {
        let inner = r#"[null,["conv","resp"],null,null,[["rc_choice",["Hello"],null,"First reasoning step","Second reasoning step"]]]"#;
        let line = wrb_line(inner);
        let data = parse_response_line(&line).unwrap();
        assert_eq!(data.text, "Hello");
        let thinking = data.thinking.unwrap();
        assert!(thinking.contains("First reasoning step"));
        assert!(thinking.contains("Second reasoning step"));
    }

    #[test]
    fn extract_conversation_from_line_works() {
        let inner = r#"[null,["c_id","r_id"],null,null,[["ch_id",["text"]]]]"#;
        let line = wrb_line(inner);
        let conv = extract_conversation_from_line(&line).unwrap();
        assert_eq!(conv.conversation_id, "c_id");
        assert_eq!(conv.response_id, "r_id");
        assert_eq!(conv.choice_id, "ch_id");
    }

    #[test]
    fn build_request_with_tools_non_empty() {
        let tools = vec![Tool {
            r#type: "function".to_string(),
            function: crate::models::FunctionDefinition {
                name: "get_weather".to_string(),
                description: Some("Get weather for a city".to_string()),
                parameters: Some(serde_json::json!({"type":"object","properties":{"city":{"type":"string"}}})),
                strict: None,
            },
        }];
        let payload = build_request_payload("weather", None, Some(&tools), None, 4, 1, test_uuid(), false).unwrap();
        let arr = payload.as_array().unwrap();
        assert_eq!(arr[9].as_array().unwrap().len(), 1);
        assert_eq!(arr[9][0][0], "get_weather");
    }

    #[test]
    fn build_request_payload_with_images() {
        let images = serde_json::json!([ [["http://example.com/img.png", 1], "img.png"] ]);
        let payload = build_request_payload("describe", None, None, Some(&images), 4, 1, test_uuid(), false).unwrap();
        let arr = payload.as_array().unwrap();
        assert_eq!(arr[0][3].as_array().unwrap().len(), 1);
        assert_eq!(arr[0][3][0][0][0], "http://example.com/img.png");
    }

    #[test]
    fn build_request_payload_with_images_and_tools() {
        let images = serde_json::json!([ [["http://example.com/img.png", 1], "img.png"] ]);
        let tools = vec![Tool {
            r#type: "function".to_string(),
            function: crate::models::FunctionDefinition {
                name: "analyze".to_string(),
                description: Some("Analyze image".to_string()),
                parameters: None,
                strict: None,
            },
        }];
        let payload =
            build_request_payload("analyze this", None, Some(&tools), Some(&images), 4, 1, test_uuid(), false).unwrap();
        let arr = payload.as_array().unwrap();
        assert_eq!(arr[0][3].as_array().unwrap().len(), 1);
        assert_eq!(arr[9].as_array().unwrap().len(), 1);
    }

    #[test]
    fn clean_response_text_removes_tool_markers() {
        let text = r#"Let me search for that.
<tool_call>{"name":"search","arguments":{"q":"test"}}</tool_call>"#;
        let cleaned = clean_response_text(text);
        assert_eq!(cleaned, "Let me search for that.");
    }

    #[test]
    fn citation_markers_stripped_from_text() {
        let text = "Some text with a [citation:1] citation and [citation:2:3] multiple.";
        let cleaned = strip_citation_markers(text);
        assert_eq!(cleaned, "Some text with a  citation and  multiple.");
    }

    #[test]
    fn streaming_chunk_with_citations() {
        // Frame with text + citation marker
        let inner = r#"[null,["c","r"],null,null,[["ch",["Result [citation:1] here"]]]]"#;
        let line = wrb_line(inner);
        let result = parse_streaming_chunk(&line, "").unwrap();
        assert!(result.is_some());
        let (delta, _, _, _, _) = result.unwrap();
        // Citation markers are stripped from the text
        assert_eq!(delta, "Result  here");
    }
}
