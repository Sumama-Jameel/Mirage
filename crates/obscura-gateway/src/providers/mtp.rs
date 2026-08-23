//! Mirage Tool Protocol v1 (MTP/1).
//!
//! The universal tool-calling dialect for this gateway. Browser/internal
//! provider APIs do not expose a native channel that accepts arbitrary
//! OpenAI `tools`; they ignore the `tools` parameter. Instead, Mirage
//! compiles client tools into a prompted tool-output protocol: the model is
//! instructed to emit a `[MIRAGE_TOOL_CALL_V1]` block containing JSON, which
//! Mirage parses, validates, repairs, and converts back into OpenAI
//! `tool_calls`.
//!
//! This module is transport-agnostic: it operates on text deltas and raw
//! blocks, so any provider stream (SSE, ConnectRPC, WebSocket) can feed it.

use serde::{Deserialize, Serialize};

use crate::models::{ChatCompletionRequest, ChatMessage, FunctionCall, Tool, ToolCall, ToolChoice};

/// Opening marker of a Mirage tool-call block.
pub const TOOL_CALL_START: &str = "[MIRAGE_TOOL_CALL_V1]";
/// Closing marker of a Mirage tool-call block.
pub const TOOL_CALL_END: &str = "[/MIRAGE_TOOL_CALL_V1]";
/// Opening marker of a Mirage tool-result block.
pub const TOOL_RESULT_START: &str = "[MIRAGE_TOOL_RESULT_V1]";
/// Closing marker of a Mirage tool-result block.
pub const TOOL_RESULT_END: &str = "[/MIRAGE_TOOL_RESULT_V1]";

/// A single tool call emitted by the model inside an MTP block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MirageToolCall {
    /// Optional client-supplied id; a stable id is generated if absent.
    #[serde(default)]
    pub id: Option<String>,
    /// Tool name.
    pub name: String,
    /// Tool arguments as a JSON object.
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// A tool result fed back to the model inside an MTP result block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MirageToolResult {
    /// Id of the tool call this result answers.
    pub id: String,
    /// Tool output.
    #[serde(default)]
    pub output: serde_json::Value,
    /// Error, if the tool call failed.
    #[serde(default)]
    pub error: Option<serde_json::Value>,
}

/// Error produced while parsing or validating an MTP block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MtpError {
    /// The block payload was not valid JSON.
    InvalidJson(String),
    /// The JSON parsed but was not a valid tool-call object.
    InvalidShape(String),
    /// The tool name is not in the provided definitions.
    UnknownTool(String),
    /// The arguments failed validation against the tool schema.
    InvalidArguments(String),
}

impl std::fmt::Display for MtpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MtpError::InvalidJson(e) => write!(f, "invalid JSON in MTP block: {e}"),
            MtpError::InvalidShape(e) => write!(f, "invalid MTP tool-call shape: {e}"),
            MtpError::UnknownTool(name) => write!(f, "unknown tool: {name}"),
            MtpError::InvalidArguments(e) => write!(f, "invalid tool arguments: {e}"),
        }
    }
}

/// Compile a list of OpenAI `Tool` definitions into the compact prompt form
/// used for weak models (not a raw JSON Schema dump).
///
/// Example output:
/// ```text
/// 1. write_file
/// Description: Write a file.
/// Arguments:
/// - name: string, required. The file name.
/// - content: string, required. The file content.
/// ```
pub fn compile_tools_for_prompt(tools: &[Tool]) -> String {
    let mut out = Vec::new();
    for (index, tool) in tools.iter().enumerate() {
        let mut lines = Vec::new();
        lines.push(format!("{}. {}", index + 1, tool.function.name));
        if let Some(desc) = &tool.function.description {
            if !desc.is_empty() {
                lines.push(format!("Description: {desc}"));
            }
        }
        lines.push("Arguments:".to_string());
        if let Some(params) = &tool.function.parameters {
            if let Some(obj) = params.as_object() {
                let required: std::collections::HashSet<&str> = obj
                    .get("required")
                    .and_then(|r| r.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<std::collections::HashSet<_>>()
                    })
                    .unwrap_or_default();
                if let Some(props) = obj.get("properties").and_then(|p| p.as_object()) {
                    for (key, schema) in props {
                        let ty = schema
                            .get("type")
                            .and_then(|t| t.as_str())
                            .unwrap_or("string");
                        let req = if required.contains(key.as_str()) {
                            "required"
                        } else {
                            "optional"
                        };
                        let desc = schema
                            .get("description")
                            .and_then(|d| d.as_str())
                            .unwrap_or("");
                        if desc.is_empty() {
                            lines.push(format!("- {key}: {ty}, {req}"));
                        } else {
                            lines.push(format!("- {key}: {ty}, {req}. {desc}"));
                        }
                    }
                }
            }
        }
        out.push(lines.join("\n"));
    }
    out.join("\n\n")
}

/// Build the MTP system prompt that instructs the model to emit tool blocks.
///
/// `tools` is the compiled tool list (from [`compile_tools_for_prompt`]).
/// `tool_choice` maps to the MTP behavior. `final_reminder` appends a
/// trailing nudge for weak models.
pub fn build_mtp_system_prompt(
    tools: &[Tool],
    tool_choice: Option<&ToolChoice>,
    final_reminder: bool,
) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are running inside Mirage, an OpenAI-compatible gateway.\n\n\
         CRITICAL TOOL POLICY:\n\
         - Native tool calling is disabled in this pipeline.\n\
         - Do not attempt to call tools natively.\n\
         - Do not use built-in tools from the host app.\n\
         - Do not use provider tools, browser tools, search tools, code tools, canvas tools, or any hidden system tools.\n\
         - Native tool invocation will break the pipeline.\n\n\
         If you need to use a tool, you must output exactly one Mirage tool block in this format:\n\n",
    );
    prompt.push_str(TOOL_CALL_START);
    prompt.push_str("\n{\n  \"id\": \"call_001\",\n  \"name\": \"tool_name\",\n  \"arguments\": {\n    \"argument_name\": \"argument_value\"\n  }\n}\n");
    prompt.push_str(TOOL_CALL_END);
    prompt.push_str(
        "\n\nRules:\n\
         - Output only the Mirage tool block.\n\
         - Do not wrap it in markdown.\n\
         - Do not add explanations before or after it.\n\
         - Do not output multiple tool blocks unless explicitly allowed.\n\
         - The JSON must be valid.\n\
         - Use only the tools listed below.\n\
         - If no tool is needed, respond normally.\n\n",
    );

    if !tools.is_empty() {
        prompt.push_str("Available tools:\n\n");
        prompt.push_str(&compile_tools_for_prompt(tools));
        prompt.push('\n');
    }

    match tool_choice {
        Some(ToolChoice::Named { function, .. }) => {
            prompt.push_str(&format!(
                "\nYou MUST call the function `{}`. Output only a Mirage tool block for it.\n",
                function.name
            ));
        }
        Some(ToolChoice::Mode(mode)) => match mode.as_str() {
            "none" => {
                prompt.push_str("\nDo not use any tools. Answer the user's question directly.\n");
            }
            "required" => {
                prompt.push_str(
                    "\nYou MUST use exactly one tool. Do not answer directly. Output only a Mirage tool block.\n",
                );
            }
            _ => {
                prompt.push_str("\nUse a tool only if necessary.\n");
            }
        },
        None => {
            prompt.push_str("\nUse a tool only if necessary.\n");
        }
    }

    if final_reminder {
        prompt.push_str(
            "\nReminder: If you need to use a tool, output only a [MIRAGE_TOOL_CALL_V1] block. Do not call tools natively.\n",
        );
    }

    prompt
}

/// Streaming parser that extracts MTP tool blocks from model output deltas.
///
/// Normal text is streamed through immediately. When a `[MIRAGE_TOOL_CALL_V1]`
/// marker begins, text emission pauses and the block is buffered until the
/// closing marker, then emitted as a complete raw block. Markers split across
/// arbitrary chunk boundaries are handled by an internal carry buffer.
#[derive(Debug, Default)]
pub struct MtpStreamParser {
    in_tool_call: bool,
    tool_call_buffer: String,
    suffix: String,
    /// Blocks completed during a `process` call beyond the first, deferred
    /// so the single-block `process` API never drops them.
    pending_blocks: Vec<String>,
}

// `process`/`in_tool_call`/`finish_pending` are the single-block legacy
// surface kept for the Xml/Gemini dialect rollback path and direct parser
// use; providers normally drive `MtpStreamState::process_delta` instead.
#[allow(dead_code)]
impl MtpStreamParser {
    /// Create a new parser with no in-flight tool call.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one delta chunk.
    ///
    /// Returns `(cleaned_text, raw_block)` where `cleaned_text` is the delta
    /// with any MTP markers removed (safe to forward as `content`) and
    /// `raw_block` is `Some` exactly when a complete `[MIRAGE_TOOL_CALL_V1]`
    /// block finished within this chunk. If multiple blocks complete within
    /// one chunk, only the first is returned; use [`Self::process_all`] to
    /// collect every completed block.
    pub fn process(&mut self, delta: &str) -> (String, Option<String>) {
        let (clean, mut blocks) = self.process_all(delta);
        if blocks.is_empty() {
            (clean, None)
        } else {
            let rest = blocks.split_off(1);
            // Re-buffer any additional completed blocks so they surface on
            // subsequent calls instead of being dropped.
            self.pending_blocks.splice(..0, rest);
            (clean, Some(blocks.remove(0)))
        }
    }

    /// Feed one delta chunk, collecting every completed tool block.
    ///
    /// Returns `(cleaned_text, blocks)`. Also drains blocks deferred from
    /// earlier [`Self::process`] calls.
    pub fn process_all(&mut self, delta: &str) -> (String, Vec<String>) {
        const MAX_MARKER: usize = 24;

        let mut clean = String::new();
        let mut raw_blocks = std::mem::take(&mut self.pending_blocks);

        let mut combined = std::mem::take(&mut self.suffix);
        combined.push_str(delta);

        let combined_len = combined.len();
        let mut scan = 0usize;

        while scan < combined_len {
            let remaining = &combined[scan..];

            if self.in_tool_call {
                if let Some(end_pos) = remaining.find(TOOL_CALL_END) {
                    self.tool_call_buffer.push_str(&remaining[..end_pos]);
                    raw_blocks.push(std::mem::take(&mut self.tool_call_buffer));
                    self.in_tool_call = false;
                    scan += end_pos + TOOL_CALL_END.len();
                } else {
                    let rlen = remaining.len();
                    let check_start = if rlen > MAX_MARKER { rlen - MAX_MARKER } else { 0 };
                    let mut prefix_end = rlen;
                    for cl in (1..=rlen - check_start).rev() {
                        if TOOL_CALL_END.starts_with(&remaining[rlen - cl..]) {
                            prefix_end = rlen - cl;
                            break;
                        }
                    }
                    self.tool_call_buffer.push_str(&remaining[..prefix_end]);
                    self.suffix = remaining[prefix_end..].to_string();
                    break;
                }
            } else if let Some(start_pos) = remaining.find(TOOL_CALL_START) {
                clean.push_str(&remaining[..start_pos]);
                scan += start_pos + TOOL_CALL_START.len();
                self.in_tool_call = true;
            } else {
                let rlen = remaining.len();
                let check_start = if rlen > MAX_MARKER { rlen - MAX_MARKER } else { 0 };
                let mut prefix_end = rlen;
                for cl in (1..=rlen - check_start).rev() {
                    let trail = &remaining[rlen - cl..];
                    if TOOL_CALL_START.starts_with(trail) || TOOL_CALL_END.starts_with(trail) {
                        prefix_end = rlen - cl;
                        break;
                    }
                }
                clean.push_str(&remaining[..prefix_end]);
                self.suffix = remaining[prefix_end..].to_string();
                break;
            }
        }

        (clean, raw_blocks)
    }

    /// True when a tool block is open at the end of the last processed delta.
    pub fn in_tool_call(&self) -> bool {
        self.in_tool_call
    }

    /// Flush a tool block that is still open when the stream ends.
    /// Returns the raw block if the buffered payload is non-empty.
    pub fn finish_pending(&mut self) -> Option<String> {
        if !self.in_tool_call || self.tool_call_buffer.is_empty() {
            return None;
        }
        let raw = std::mem::take(&mut self.tool_call_buffer);
        self.in_tool_call = false;
        self.suffix.clear();
        Some(raw)
    }

    /// Flush at stream end, returning every outstanding raw block: deferred
    /// blocks from earlier [`Self::process`] calls plus any still-open block.
    pub fn finish_all(&mut self) -> Vec<String> {
        let mut out = std::mem::take(&mut self.pending_blocks);
        if let Some(raw) = self.finish_pending() {
            out.push(raw);
        }
        // Any residual suffix is text the caller already emitted around; it
        // cannot form a complete block on its own, so drop it.
        self.suffix.clear();
        out
    }
}

/// Parse a raw MTP tool-call block (with or without markers) into a
/// [`MirageToolCall`].
pub fn parse_tool_block(raw: &str) -> Result<MirageToolCall, MtpError> {
    let payload = strip_tool_markers(raw);
    let value: serde_json::Value =
        serde_json::from_str(payload).map_err(|e| MtpError::InvalidJson(e.to_string()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| MtpError::InvalidShape("block is not a JSON object".to_string()))?;
    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MtpError::InvalidShape("missing string field 'name'".to_string()))?
        .to_string();
    let arguments = obj
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
    let id = obj.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
    Ok(MirageToolCall {
        id,
        name,
        arguments,
    })
}

/// Strip MTP markers from a raw block payload.
fn strip_tool_markers(raw: &str) -> &str {
    let trimmed = raw.trim();
    let start = if let Some(pos) = trimmed.find(TOOL_CALL_START) {
        pos + TOOL_CALL_START.len()
    } else {
        0
    };
    let end = if let Some(pos) = trimmed.find(TOOL_CALL_END) {
        pos
    } else {
        trimmed.len()
    };
    trimmed[start..end].trim()
}

/// Validate a parsed tool call against the provided tool definitions.
///
/// Checks that the tool name exists and that the arguments are a JSON object
/// (basic shape validation; deep schema validation is intentionally light to
/// avoid rejecting valid calls from weak models).
pub fn validate_tool_call(
    call: &MirageToolCall,
    tools: &[Tool],
) -> Result<(), MtpError> {
    let def = tools
        .iter()
        .find(|t| t.function.name == call.name)
        .ok_or_else(|| MtpError::UnknownTool(call.name.clone()))?;
    if !call.arguments.is_object() {
        return Err(MtpError::InvalidArguments(
            "arguments must be a JSON object".to_string(),
        ));
    }
    // If the tool declares required fields, ensure they are present.
    if let Some(params) = &def.function.parameters {
        if let Some(obj) = params.as_object() {
            if let Some(required) = obj.get("required").and_then(|r| r.as_array()) {
                let args_obj = call.arguments.as_object().unwrap();
                for req in required {
                    if let Some(name) = req.as_str() {
                        if !args_obj.contains_key(name) {
                            return Err(MtpError::InvalidArguments(format!(
                                "missing required argument '{name}'"
                            )));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Build a repair prompt asking the model to re-emit a valid MTP block.
// `build_repair_prompt` is consumed via `MtpPipeline::next_repair_prompt`.
#[allow(dead_code)]
pub fn build_repair_prompt(error: &MtpError, original_block: &str) -> String {
    format!(
        "Your previous response contained an invalid Mirage tool block.\n\n\
         Error:\n- {error}\n\n\
         Original block:\n{original_block}\n\n\
         Respond again with only a valid Mirage tool block.\n\
         Do not add explanation."
    )
}

/// Convert a parsed MTP tool call into an OpenAI `ToolCall`.
///
/// OpenAI expects `function.arguments` to be a JSON string, not an object.
pub fn to_openai_tool_call(call: &MirageToolCall) -> ToolCall {
    let arguments_str = serde_json::to_string(&call.arguments)
        .unwrap_or_else(|_| "{}".to_string());
    let id = call
        .id
        .clone()
        .unwrap_or_else(|| generate_tool_call_id(&call.name, &arguments_str));
    ToolCall {
        id,
        r#type: "function".to_string(),
        function: FunctionCall {
            name: call.name.clone(),
            arguments: arguments_str,
        },
    }
}

/// Generate a deterministic tool-call id from the call content.
fn generate_tool_call_id(name: &str, arguments: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in format!("{name}\u{0}{arguments}").bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("call_{:016x}", h)
}

/// Format one or more tool results into an MTP result block for upstream.
pub fn format_tool_results(items: &[(Option<&ToolCall>, Option<&str>, &str)]) -> String {
    if items.is_empty() {
        return String::new();
    }

    let header = if items.len() == 1 {
        "You used this tool:\n\n"
    } else {
        "You used these tools:\n\n"
    };

    let mut parts = Vec::with_capacity(items.len());
    for (call, fallback_id, output) in items {
        let id = call
            .map(|c| c.id.clone())
            .or_else(|| fallback_id.map(|s| s.to_string()))
            .unwrap_or_else(|| "call_unknown".to_string());
        let output_value: serde_json::Value =
            serde_json::from_str(output).unwrap_or_else(|_| serde_json::Value::String(output.to_string()));
        let result = MirageToolResult {
            id,
            output: output_value,
            error: None,
        };
        let block = serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string());
        parts.push(format!(
            "{TOOL_RESULT_START}\n{block}\n{TOOL_RESULT_END}"
        ));
    }

    format!("{header}{}\n\nContinue the conversation based on this tool result.", parts.join("\n\n"))
}

/// Prepare a request for the MTP pipeline.
///
/// This is the shared entry point providers call when `request.tools` is
/// present. It:
///
/// 1. Normalizes the message history (converts `role: "tool"` and
///    `assistant.tool_calls` into MTP blocks).
/// 2. Prepends the MTP system prompt (compiled from the tools) to the
///    messages.
/// 3. Returns the prepared messages and a flag indicating whether tools
///    were present (so the caller knows to strip `tools`/`tool_choice`
///    from the upstream request and to run the MTP stream parser).
///
/// `final_reminder` appends a trailing nudge for weak models.
pub fn prepare_request(
    request: &ChatCompletionRequest,
    final_reminder: bool,
) -> (Vec<ChatMessage>, bool) {
    let Some(tools) = &request.tools else {
        return (request.messages.clone(), false);
    };
    if tools.is_empty() {
        return (request.messages.clone(), false);
    }

    let mut messages = normalize_history(&request.messages);
    let system_prompt = build_mtp_system_prompt(tools, request.tool_choice.as_ref(), final_reminder);

    // Prepend the MTP system prompt as a system message.
    messages.insert(
        0,
        ChatMessage {
            role: "system".to_string(),
            content: system_prompt.into(),
            name: None,
            reasoning_content: None,
            citations: None,
            tool_calls: None,
            tool_call_id: None,
        },
    );

    (messages, true)
}

/// Streaming state for the MTP pipeline.
///
/// Wraps an [`MtpStreamParser`] and collects parsed tool calls. Providers
/// feed content deltas through [`MtpStreamState::process_delta`] and call
/// [`MtpStreamState::finish`] at stream end.
#[derive(Debug, Default)]
pub struct MtpStreamState {
    parser: MtpStreamParser,
    /// Tool calls collected from the stream.
    pub collected_tool_calls: Vec<ToolCall>,
    /// Whether the stream contained any tool calls.
    pub saw_tool_calls: bool,
    /// Raw blocks that failed parsing or validation, with the error.
    errors: Vec<(String, MtpError)>,
}

// Error/diagnostics accessors are consumed through `MtpPipeline`.
#[allow(dead_code)]
impl MtpStreamState {
    /// Create a new MTP stream state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one content delta through the MTP parser.
    ///
    /// Returns the cleaned text (safe to forward as `content`). Every
    /// complete MTP tool block is parsed and validated against `tools`;
    /// valid calls are collected, invalid ones are recorded (see
    /// [`Self::take_errors`]) and their raw text is never leaked.
    pub fn process_delta(&mut self, delta: &str, tools: &[Tool]) -> String {
        let (clean, blocks) = self.parser.process_all(delta);
        self.absorb(blocks, tools);
        clean
    }

    /// Flush any pending tool block(s) at stream end.
    pub fn finish(&mut self, tools: &[Tool]) {
        let blocks = self.parser.finish_all();
        self.absorb(blocks, tools);
    }

    /// Drain every recorded parse/validation failure so far.
    ///
    /// Each entry is `(raw_block_payload_without_markers, error)`. Callers
    /// use this to drive repair loops ([`build_repair_prompt`]) or surface
    /// diagnostics; an empty result means no invalid blocks were seen.
    pub fn take_errors(&mut self) -> Vec<(String, MtpError)> {
        std::mem::take(&mut self.errors)
    }

    /// True when at least one block failed parsing or validation.
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// True when a tool block is still open (mid-stream).
    pub fn in_tool_call(&self) -> bool {
        self.parser.in_tool_call()
    }

    fn absorb(&mut self, blocks: Vec<String>, tools: &[Tool]) {
        for raw in blocks {
            match parse_tool_block(&raw) {
                Ok(call) => match validate_tool_call(&call, tools) {
                    Ok(()) => {
                        self.collected_tool_calls.push(to_openai_tool_call(&call));
                        self.saw_tool_calls = true;
                    }
                    Err(e) => self.errors.push((raw, e)),
                },
                Err(e) => self.errors.push((raw, e)),
            }
        }
    }
}

/// Normalize a message history for upstream providers that do not understand
/// OpenAI `role: "tool"` or `assistant.tool_calls`.
///
/// - `role: "tool"` messages become an MTP tool-result user message.
/// - `assistant` messages with `tool_calls` become an MTP tool-call text.
///
/// Returns a new message list; messages that need no transformation are
/// passed through unchanged.
pub fn normalize_history(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(messages.len());
    for msg in messages {
        match msg.role.as_str() {
            "tool" => {
                let id = msg.tool_call_id.clone().unwrap_or_else(|| "call_unknown".to_string());
                let output_value: serde_json::Value = serde_json::from_str(&msg.content.as_text())
                    .unwrap_or_else(|_| serde_json::Value::String(msg.content.as_text()));
                let result = MirageToolResult {
                    id,
                    output: output_value,
                    error: None,
                };
                let block = serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string());
                let text = format!(
                    "{TOOL_RESULT_START}\n{block}\n{TOOL_RESULT_END}\n\nContinue the conversation based on this tool result."
                );
                out.push(ChatMessage {
                    role: "user".to_string(),
                    content: text.into(),
                    name: None,
                    reasoning_content: None,
                    citations: None,
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
            "assistant" if msg.tool_calls.is_some() => {
                let calls = msg.tool_calls.as_ref().unwrap();
                let mut blocks = Vec::new();
                for call in calls {
                    let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                        .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
                    let mtp = MirageToolCall {
                        id: Some(call.id.clone()),
                        name: call.function.name.clone(),
                        arguments: args,
                    };
                    let block = serde_json::to_string(&mtp).unwrap_or_else(|_| "{}".to_string());
                    blocks.push(format!("{TOOL_CALL_START}\n{block}\n{TOOL_CALL_END}"));
                }
                let text = blocks.join("\n\n");
                out.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: text.into(),
                    name: None,
                    reasoning_content: None,
                    citations: None,
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
            _ => out.push(msg.clone()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::FunctionDefinition;

    fn sample_tools() -> Vec<Tool> {
        vec![Tool {
            r#type: "function".to_string(),
            function: FunctionDefinition {
                name: "write_file".to_string(),
                description: Some("Write a file.".to_string()),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["name", "content"]
                })),
                strict: None,
            },
        }]
    }

    #[test]
    fn compile_tools_compact_form() {
        let out = compile_tools_for_prompt(&sample_tools());
        assert!(out.contains("1. write_file"));
        assert!(out.contains("Description: Write a file."));
        assert!(out.contains("- name: string, required"));
        assert!(out.contains("- content: string, required"));
    }

    #[test]
    fn build_prompt_contains_markers() {
        let prompt = build_mtp_system_prompt(&sample_tools(), None, false);
        assert!(prompt.contains(TOOL_CALL_START));
        assert!(prompt.contains(TOOL_CALL_END));
        assert!(prompt.contains("write_file"));
    }

    #[test]
    fn build_prompt_tool_choice_required() {
        let prompt = build_mtp_system_prompt(
            &sample_tools(),
            Some(&ToolChoice::Mode("required".to_string())),
            false,
        );
        assert!(prompt.contains("MUST use exactly one tool"));
    }

    #[test]
    fn build_prompt_tool_choice_none() {
        let prompt = build_mtp_system_prompt(
            &sample_tools(),
            Some(&ToolChoice::Mode("none".to_string())),
            false,
        );
        assert!(prompt.contains("Do not use any tools"));
    }

    #[test]
    fn build_prompt_final_reminder() {
        let prompt = build_mtp_system_prompt(&sample_tools(), None, true);
        assert!(prompt.contains("Reminder:"));
    }

    #[test]
    fn stream_parser_extracts_block() {
        let mut parser = MtpStreamParser::new();
        let (clean, block) = parser.process("hello [MIRAGE_TOOL_CALL_V1]{\"name\":\"write_file\",\"arguments\":{\"name\":\"a.txt\",\"content\":\"hi\"}}[/MIRAGE_TOOL_CALL_V1]");
        assert_eq!(clean, "hello ");
        assert!(block.is_some());
        let call = parse_tool_block(block.unwrap().as_str()).unwrap();
        assert_eq!(call.name, "write_file");
        assert_eq!(call.arguments["name"], "a.txt");
    }

    #[test]
    fn stream_parser_handles_split_markers() {
        let mut parser = MtpStreamParser::new();
        let (clean1, block1) = parser.process("hi [MIRAGE_TOOL_");
        assert_eq!(clean1, "hi ");
        assert!(block1.is_none());
        let (clean2, block2) = parser.process("CALL_V1]{\"name\":\"write_file\",\"arguments\":{}}[/MIRAGE_TOOL_CALL_V1]");
        assert_eq!(clean2, "");
        assert!(block2.is_some());
    }

    #[test]
    fn stream_parser_flush_pending() {
        let mut parser = MtpStreamParser::new();
        let _ = parser.process("[MIRAGE_TOOL_CALL_V1]{\"name\":\"write_file\",\"arguments\":{}}");
        assert!(parser.in_tool_call());
        let raw = parser.finish_pending();
        assert!(raw.is_some());
        let call = parse_tool_block(raw.unwrap().as_str()).unwrap();
        assert_eq!(call.name, "write_file");
    }

    #[test]
    fn stream_parser_multiple_blocks_in_one_chunk() {
        let mut parser = MtpStreamParser::new();
        let payload = "[MIRAGE_TOOL_CALL_V1]{\"name\":\"write_file\",\"arguments\":{}}[/MIRAGE_TOOL_CALL_V1]mid[MIRAGE_TOOL_CALL_V1]{\"name\":\"write_file\",\"arguments\":{}}[/MIRAGE_TOOL_CALL_V1]";
        let (clean, blocks) = parser.process_all(payload);
        assert_eq!(clean, "mid");
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn stream_state_records_errors_and_drains() {
        let tools = sample_tools();
        let mut state = MtpStreamState::new();
        // Unknown tool + invalid JSON + one valid call, all in a single chunk.
        let delta = "[MIRAGE_TOOL_CALL_V1]{\"name\":\"nope\",\"arguments\":{}}[/MIRAGE_TOOL_CALL_V1]ok [MIRAGE_TOOL_CALL_V1]{\"name\":\"write_file\",\"arguments\":{\"name\":\"a.txt\",\"content\":\"hi\"}}[/MIRAGE_TOOL_CALL_V1][MIRAGE_TOOL_CALL_V1]{broken[/MIRAGE_TOOL_CALL_V1]";
        let clean = state.process_delta(delta, &tools);
        assert_eq!(clean, "ok ");
        assert_eq!(state.collected_tool_calls.len(), 1);
        assert!(state.has_errors());
        let errors = state.take_errors();
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().any(|(_, e)| matches!(e, MtpError::UnknownTool(_))));
        assert!(errors.iter().any(|(_, e)| matches!(e, MtpError::InvalidJson(_))));
        assert!(!state.has_errors());

        // A second identical chunk still collects the valid call.
        let _ = state.process_delta(delta, &tools);
        assert_eq!(state.collected_tool_calls.len(), 2);
    }

    #[test]
    fn stream_state_finish_flushes_open_block() {
        let tools = sample_tools();
        let mut state = MtpStreamState::new();
        let _ = state.process_delta("[MIRAGE_TOOL_CALL_V1]{\"name\":\"write_file\",\"arguments\":{\"name\":\"a\",\"content\":\"b\"}}", &tools);
        assert!(state.in_tool_call());
        state.finish(&tools);
        assert_eq!(state.collected_tool_calls.len(), 1);
        assert!(state.take_errors().is_empty());
    }

    #[test]
    fn parse_tool_block_without_markers() {
        let call = parse_tool_block("{\"name\":\"write_file\",\"arguments\":{\"name\":\"a.txt\"}}").unwrap();
        assert_eq!(call.name, "write_file");
        assert_eq!(call.arguments["name"], "a.txt");
    }

    #[test]
    fn parse_tool_block_invalid_json() {
        let err = parse_tool_block("not json").unwrap_err();
        assert!(matches!(err, MtpError::InvalidJson(_)));
    }

    #[test]
    fn parse_tool_block_missing_name() {
        let err = parse_tool_block("{\"arguments\":{}}").unwrap_err();
        assert!(matches!(err, MtpError::InvalidShape(_)));
    }

    #[test]
    fn validate_tool_call_ok() {
        let call = MirageToolCall {
            id: None,
            name: "write_file".to_string(),
            arguments: serde_json::json!({"name": "a.txt", "content": "hi"}),
        };
        assert!(validate_tool_call(&call, &sample_tools()).is_ok());
    }

    #[test]
    fn validate_tool_call_unknown_tool() {
        let call = MirageToolCall {
            id: None,
            name: "nope".to_string(),
            arguments: serde_json::json!({}),
        };
        let err = validate_tool_call(&call, &sample_tools()).unwrap_err();
        assert!(matches!(err, MtpError::UnknownTool(_)));
    }

    #[test]
    fn validate_tool_call_missing_required() {
        let call = MirageToolCall {
            id: None,
            name: "write_file".to_string(),
            arguments: serde_json::json!({"name": "a.txt"}),
        };
        let err = validate_tool_call(&call, &sample_tools()).unwrap_err();
        assert!(matches!(err, MtpError::InvalidArguments(_)));
    }

    #[test]
    fn to_openai_tool_call_serializes_arguments_string() {
        let call = MirageToolCall {
            id: Some("call_1".to_string()),
            name: "write_file".to_string(),
            arguments: serde_json::json!({"name": "a.txt"}),
        };
        let tc = to_openai_tool_call(&call);
        assert_eq!(tc.id, "call_1");
        assert_eq!(tc.function.name, "write_file");
        // arguments must be a JSON string
        let parsed: serde_json::Value = serde_json::from_str(&tc.function.arguments).unwrap();
        assert_eq!(parsed["name"], "a.txt");
    }

    #[test]
    fn to_openai_tool_call_generates_id() {
        let call = MirageToolCall {
            id: None,
            name: "write_file".to_string(),
            arguments: serde_json::json!({}),
        };
        let tc = to_openai_tool_call(&call);
        assert!(tc.id.starts_with("call_"));
    }

    #[test]
    fn build_repair_prompt_mentions_error() {
        let err = MtpError::UnknownTool("nope".to_string());
        let prompt = build_repair_prompt(&err, "[MIRAGE_TOOL_CALL_V1]...[/MIRAGE_TOOL_CALL_V1]");
        assert!(prompt.contains("unknown tool: nope"));
        assert!(prompt.contains("valid Mirage tool block"));
    }

    #[test]
    fn format_tool_results_builds_block() {
        let out = format_tool_results(&[(None, Some("call_1"), "success")]);
        assert!(out.contains(TOOL_RESULT_START));
        assert!(out.contains(TOOL_RESULT_END));
        assert!(out.contains("call_1"));
    }

    #[test]
    fn normalize_history_converts_tool_role() {
        let msg = ChatMessage {
            role: "tool".to_string(),
            content: "success".into(),
            name: None,
            reasoning_content: None,
            citations: None,
            tool_calls: None,
            tool_call_id: Some("call_1".to_string()),
        };
        let out = normalize_history(&[msg]);
        assert_eq!(out[0].role, "user");
        assert!(out[0].content.as_text().contains(TOOL_RESULT_START));
    }

    #[test]
    fn normalize_history_converts_assistant_tool_calls() {
        let msg = ChatMessage {
            role: "assistant".to_string(),
            content: "".into(),
            name: None,
            reasoning_content: None,
            citations: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                r#type: "function".to_string(),
                function: FunctionCall {
                    name: "write_file".to_string(),
                    arguments: "{\"name\":\"a.txt\"}".to_string(),
                },
            }]),
            tool_call_id: None,
        };
        let out = normalize_history(&[msg]);
        assert_eq!(out[0].role, "assistant");
        assert!(out[0].content.as_text().contains(TOOL_CALL_START));
        assert!(out[0].tool_calls.is_none());
    }
}