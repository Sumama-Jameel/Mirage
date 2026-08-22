//! MTP/1 conformance suite.
//!
//! Behavioral gate for the universal tool protocol: every scenario a client
//! can drive through the OpenAI-compatible surface, executed against
//! [`MtpPipeline`] exactly as provider adapters run it (feed deltas, finish,
//! drain). No network involved.

use super::mtp::TOOL_CALL_START;
use super::mtp_pipeline::MtpPipeline;
use crate::models::ChatCompletionRequest;

fn request(tools: serde_json::Value, choice: Option<&str>) -> ChatCompletionRequest {
    let mut body = serde_json::json!({
        "model": "glm-5.2",
        "messages": [{"role": "user", "content": "do something"}],
    });
    if !tools.is_null() {
        body["tools"] = tools;
    }
    if let Some(c) = choice {
        body["tool_choice"] = serde_json::json!(c);
    }
    serde_json::from_value(body).unwrap()
}

fn echo_tools() -> serde_json::Value {
    serde_json::json!([{
        "type": "function",
        "function": {
            "name": "echo",
            "description": "Echo text back.",
            "parameters": {
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            }
        }
    }])
}

fn block(name: &str, args: &str) -> String {
    format!("{TOOL_CALL_START}{{\"name\":\"{name}\",\"arguments\":{args}}}[/MIRAGE_TOOL_CALL_V1]")
}

/// Feed `text` one character at a time, like a worst-case token stream.
fn drip_feed(pipe: &mut MtpPipeline, text: &str) -> String {
    let mut clean = String::new();
    for ch in text.chars() {
        clean.push_str(&pipe.feed(&ch.to_string()));
    }
    clean
}

#[test]
fn c01_plain_chat_no_tool_block() {
    let req = request(echo_tools(), None);
    let mut pipe = MtpPipeline::prepare("glm", "glm-5.2", &req);
    let out = drip_feed(&mut pipe, "Just a normal answer, no tools needed.");
    pipe.finish();
    assert_eq!(out, "Just a normal answer, no tools needed.");
    assert!(pipe.tool_calls().is_empty());
    assert_eq!(pipe.finish_reason(), "stop");
    assert!(!pipe.has_errors());
}

#[test]
fn c02_echo_tool_block_emitted() {
    let req = request(echo_tools(), None);
    let mut pipe = MtpPipeline::prepare("glm", "glm-5.2", &req);
    let _ = pipe.feed(&format!(
        "{}",
        block("echo", "{\"text\":\"hello world\"}")
    ));
    pipe.finish();
    let calls = pipe.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].r#type, "function");
    assert_eq!(calls[0].function.name, "echo");
    let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
    assert_eq!(args["text"], "hello world");
    assert!(calls[0].id.starts_with("call_"));
    assert_eq!(pipe.finish_reason(), "tool_calls");
}

#[test]
fn c03_no_tools_in_request_means_no_parsing() {
    let req = request(serde_json::Value::Null, None);
    let mut pipe = MtpPipeline::prepare("glm", "glm-5.2", &req);
    // Even if a degenerate model emits markers, they pass through untouched:
    // without tools the gateway is not in tool mode.
    let raw = "some [MIRAGE_TOOL_CALL_V1]{}[/MIRAGE_TOOL_CALL_V1] text";
    let out = pipe.feed(raw);
    pipe.finish();
    assert_eq!(out, raw);
    assert!(pipe.tool_calls().is_empty());
    assert_eq!(pipe.finish_reason(), "stop");
}

#[test]
fn c04_forced_tool_choice_required_prompt() {
    let req = request(echo_tools(), Some("required"));
    let pipe = MtpPipeline::prepare("glm", "glm-5.2", &req);
    let msgs = pipe.upstream_messages(&req);
    let sys = msgs[0].content.as_text();
    assert!(sys.contains("MUST use exactly one tool"));
    assert!(sys.contains("echo"));

    // And the forced turn actually completes when the model complies.
    let mut pipe = MtpPipeline::prepare("glm", "glm-5.2", &req);
    let _ = pipe.feed(&block("echo", "{\"text\":\"x\"}"));
    pipe.finish();
    assert_eq!(pipe.finish_reason(), "tool_calls");
}

#[test]
fn c05_invalid_block_repair_path() {
    let req = request(echo_tools(), Some("required"));
    let mut pipe = MtpPipeline::prepare("glm", "glm-5.2", &req);
    // Truncated JSON inside the block (stream cut short by upstream).
    let _ = pipe.feed("[MIRAGE_TOOL_CALL_V1]{\"name\":\"ec");
    pipe.finish(); // flushes the open block; payload is invalid JSON

    assert!(pipe.tool_calls().is_empty());
    let errs = pipe.take_errors();
    assert_eq!(errs.len(), 1);

    // Repair prompt is generated once within the profile's budget.
    let (repair_msg, raw) = pipe.next_repair_prompt().unwrap();
    assert!(repair_msg.content.as_text().contains("invalid Mirage tool block"));
    assert!(raw.contains("\"name\":\"ec"));
    assert!(pipe.next_repair_prompt().is_none());
}

#[test]
fn c06_streaming_markers_split_across_chunks() {
    let req = request(echo_tools(), None);
    let full = format!(
        "prefix {} suffix",
        block("echo", "{\"text\":\"split\"}")
    );
    let mut pipe = MtpPipeline::prepare("glm", "glm-5.2", &req);
    let out = drip_feed(&mut pipe, &full);
    pipe.finish();

    // Markers never leak into user-visible text.
    assert!(!out.contains("MIRAGE_TOOL_CALL"), "leaked: {out}");
    assert!(out.contains("prefix") && out.contains("suffix"));
    let calls = pipe.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].function.name, "echo");
}

#[test]
fn c07_builtin_tool_name_suppression() {
    // Model gets confused into calling the provider's built-in "search".
    let req = request(echo_tools(), None);
    let mut pipe = MtpPipeline::prepare("glm", "glm-5.2", &req);
    let out = drip_feed(
        &mut pipe,
        &format!("{}", block("search", "{\"query\":\"x\"}")),
    );
    pipe.finish();
    assert_eq!(out, "");
    assert!(
        pipe.tool_calls().is_empty(),
        "non-Mirage tool name must be rejected"
    );
    let errs = pipe.take_errors();
    assert!(matches!(errs.first(), Some((_, super::mtp::MtpError::UnknownTool(_)))));
    assert_eq!(pipe.finish_reason(), "stop");
}
