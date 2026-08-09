//! Shared tool call formatting and parsing utilities.
//!
//! Both DeepSeek and Gemini inject tool definitions as system-level
//! instructions into the prompt (when the native API does not support
//! function calling directly) and parse `<tool_call>`…`</tool_call>`
//! XML markers from the response text.

use crate::models::{FunctionCall, Tool, ToolCall, ToolChoice};

const TOOL_CALL_FORMAT_INSTRUCTION: &str =
    "When you need to call a function, output one or more JSON objects of \
     the form {\"name\":\"function_name\",\"arguments\":{...}} wrapped in \
     <tool_call> tags, and nothing else.";

/// Format tool definitions and inject them into the user prompt.
///
/// Prepends a system-style instruction block describing each available
/// function, respecting the `tool_choice` policy.
pub fn inject_tool_prompt(prompt: &str, tools: &[Tool], tool_choice: Option<&ToolChoice>) -> String {
    let mut instructions = String::new();
    instructions.push_str(
        "You have access to the following functions. Use them if they help answer the user.\n\n",
    );
    for tool in tools {
        if let Ok(json) = serde_json::to_string(&tool.function) {
            instructions.push_str(&format!("Function: {}\n{json}\n\n", tool.function.name));
        }
    }

    match tool_choice {
        Some(ToolChoice::Named { function, .. }) => {
            instructions.push_str(&format!(
                "You MUST call the function `{}`. {}\n\n",
                function.name, TOOL_CALL_FORMAT_INSTRUCTION
            ));
        }
        Some(ToolChoice::Mode(mode)) => match mode.as_str() {
            "none" => {
                instructions.push_str("Do not call any functions. Answer the user's question directly.\n\n")
            }
            "required" => instructions.push_str(&format!(
                "To answer this question you are required to call at least one \
                 function. If multiple functions are useful, call them. {}\n\n",
                TOOL_CALL_FORMAT_INSTRUCTION
            )),
            _ => instructions.push_str(&format!("{}\n\n", TOOL_CALL_FORMAT_INSTRUCTION)),
        },
        None => {
            instructions.push_str(&format!("{}\n\n", TOOL_CALL_FORMAT_INSTRUCTION));
        }
    }

    format!("{instructions}User request:\n{prompt}")
}

/// Gemini-specific tool-use instruction injected into the prompt.
///
/// Gemini Web has no native structured tool call, so the model is told to emit
/// a `function_call` fenced block (the format gemini-web2api trains for):
///
/// ```function_call
/// {"name": "<function>", "args": {<arguments>}}
/// ```
///
/// The function list is also passed to `request[9]` for the native path; the
/// instruction makes emission reliable.
pub fn gemini_tool_use_prompt(prompt: &str, tools: &[Tool], tool_choice: Option<&ToolChoice>) -> String {
    let mut instructions = String::new();
    instructions.push_str(
        "You can call functions to answer the user. Available functions:\n",
    );
    for tool in tools {
        let params = tool
            .function
            .parameters
            .as_ref()
            .map(|p| p.to_string())
            .unwrap_or_else(|| "{}".to_string());
        instructions.push_str(&format!(
            "- {}: {}\n  parameters: {}\n",
            tool.function.name,
            tool.function.description.as_deref().unwrap_or(""),
            params
        ));
    }
    instructions.push_str(
        "\nTo call a function, reply with exactly one fenced code block:\n\
         ```function_call\n\
         {\"name\": \"<function_name>\", \"args\": {<arguments as a JSON object>}}\n\
         ```\n\
         Then, after the tool result is provided, answer the user.",
    );

    match tool_choice {
        Some(ToolChoice::Named { function, .. }) => {
            instructions.push_str(&format!(
                " You MUST call the function `{}`.",
                function.name
            ));
        }
        Some(ToolChoice::Mode(mode)) => match mode.as_str() {
            "none" => instructions.push_str(" Do NOT call any function this turn; answer directly."),
            "required" => instructions.push_str(
                " You MUST call at least one function this turn before answering.",
            ),
            _ => {}
        },
        None => {}
    }

    format!("{instructions}\n\nUser request:\n{prompt}")
}

/// Parse `<tool_call>`…`</tool_call>` markers from response text.
///
/// Returns `None` (not an empty Vec) when no tool calls are found, so
/// callers can distinguish "no tools used" from "tools were used".
pub fn parse_tool_calls_from_content(content: &str) -> Option<Vec<ToolCall>> {
    // (?s) makes '.' match newlines so multi-line JSON inside <tool_call>
    // tags is parsed correctly.
    let re = regex::Regex::new(r"(?s)<tool_call>\s*(.*?)\s*</tool_call>").ok()?;
    let mut calls = Vec::new();
    for cap in re.captures_iter(content) {
        let raw = cap.get(1)?.as_str();
        if let Some(tc) = parse_manual_tool_call(raw) {
            calls.push(tc);
        }
    }
    if calls.is_empty() {
        None
    } else {
        Some(calls)
    }
}

/// Remove `<tool_call>`…`</tool_call>` markers from text, returning the
/// cleaned content.
pub fn strip_tool_call_markers(content: &str) -> String {
    let re = regex::Regex::new(r"(?s)<tool_call>\s*.*?\s*</tool_call>").ok();
    match re {
        Some(re) => re.replace_all(content, "").trim().to_string(),
        None => content.to_string(),
    }
}

/// Convert XML `<tool_call>` markers in response text into native `tool_calls`.
///
/// When `request_had_tools` is true and the text contains `<tool_call>` markers,
/// this parses them into structured `ToolCall` objects and strips the markers
/// from the text. Returns `(cleaned_text, Some(calls))` if markers were found,
/// or `(text, None)` if the text has no markers.
///
/// When `request_had_tools` is false, the text is returned unchanged — this
/// prevents mangling model responses that merely describe XML syntax.
pub fn convert_xml_tool_calls(text: &str, request_had_tools: bool) -> (String, Option<Vec<ToolCall>>) {
    if !request_had_tools {
        return (text.to_string(), None);
    }
    let Some(calls) = parse_tool_calls_from_content(text) else {
        return (text.to_string(), None);
    };
    if calls.is_empty() {
        return (text.to_string(), None);
    }
    let cleaned = strip_tool_call_markers(text);
    (cleaned, Some(calls))
}

/// Parse a single raw `<tool_call>` payload into a `ToolCall`.
pub fn parse_manual_tool_call(raw: &str) -> Option<ToolCall> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let obj = value.as_object()?;
    let name = obj.get("name").and_then(|v| v.as_str())?.to_string();
    let arguments = obj
        .get("arguments")
        .or_else(|| obj.get("args"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let arguments_str = if let Some(s) = arguments.as_str() {
        s.to_string()
    } else {
        serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".to_string())
    };
    let id = obj
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| stable_call_id(&name, &arguments_str));
    Some(ToolCall {
        id,
        r#type: "function".to_string(),
        function: FunctionCall {
            name,
            arguments: arguments_str,
        },
    })
}

/// Deterministic tool-call id derived from the call content.
///
/// Gemini re-emits the full response text on every streaming frame, so parsing
/// the same call repeatedly must yield the same id or dedup (by id) would
/// emit duplicate tool_calls. Streaming frame texts that differ only in a
/// partial-vs-complete argument set produce distinct ids, which is correct.
fn stable_call_id(name: &str, arguments: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in format!("{name}\u{0}{arguments}").bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("call_{:016x}", h)
}

/// Parse Gemini Web tool calls that arrive as plain text in the response.
///
/// Gemini's StreamGenerate API has no native structured tool-call slot
/// (`candidate[28]` is always empty); functions are invoked by emitting text.
/// Live-verified formats (gemini-3.5-flash, 2026-08-07):
///
/// 1. `http://googleusercontent.com/card_content/0\nget_weather(location='Paris')`
/// 2. ` ```json\n{"name": "get_weather", "arguments": {"location": "Paris"}}\n``` `
/// 3. ` ```function_call\n{"name": "get_weather", "args": {"location": "Paris"}}\n``` `
/// 4. `<tool_call>{"name":...,"arguments":...}</tool_call>` (shared XML fallback)
///
/// Returns `Some(calls)` when at least one call parses out of the text.
pub fn parse_gemini_tool_calls(text: &str) -> Option<Vec<ToolCall>> {
    let mut calls = Vec::new();

    // Strip card-content prefixes and googleusercontent artifacts first so
    // inline calls sit alone on their line.
    let cleaned = strip_gemini_card_prefix(text);

    // Fenced blocks: ```json / ```function_call / ```tool_call with { ... }.
    let fence_re = regex::Regex::new(
        r"(?s)```(?:json|function_call|tool_call)?\s*\n?\s*(\{.*?\})\s*\n?```",
    )
    .ok()?;
    for cap in fence_re.captures_iter(&cleaned) {
        if let Some(raw) = cap.get(1) {
            if let Some(tc) = parse_manual_tool_call(raw.as_str()) {
                calls.push(tc);
            }
        }
    }

    // Inline function call: `get_weather(location='Paris')` or
    // `get_weather(location="Paris, France")`.
    if let Some(tc) = parse_inline_function_call(&cleaned) {
        calls.push(tc);
    }

    // Shared XML marker fallback.
    if let Some(xml_calls) = parse_tool_calls_from_content(&cleaned) {
        calls.extend(xml_calls);
    }

    if calls.is_empty() {
        None
    } else {
        Some(calls)
    }
}

/// Strip Gemini `card_content` and other `googleusercontent.com` artifacts
/// from response text.
pub fn strip_gemini_card_prefix(text: &str) -> String {
    let card_re = regex::Regex::new(
        r"http://googleusercontent\.com/card_content/\d+\n?",
    )
    .ok();
    let after_card = match &card_re {
        Some(re) => re.replace(text, "").to_string(),
        None => text.to_string(),
    };
    let artifacts_re = regex::Regex::new(r"http://googleusercontent\.com/\w+/\d+\n*").ok();
    let after_urls = match artifacts_re {
        Some(re) => re.replace_all(&after_card, "").to_string(),
        None => after_card,
    };
    // Gemini attaches `?code_reference&code_event_index=N` /
    // `?code_stdout&code_event_index=N` to code-fence markers it renders as
    // cards; strip the query part from the fence.
    let code_ref_re = regex::Regex::new(r"\?code_\w+&code_event_index=\d+").ok();
    match code_ref_re {
        Some(re) => re.replace_all(&after_urls, "").to_string(),
        None => after_urls,
    }
}

/// True when `text` looks like a Gemini tool call still being generated.
///
/// Gemini streams calls as plain text: either a `card_content`-prefixed inline
/// `name(args)` or a fenced ` ```json {...} ``` ` block. Until the call is
/// complete the text must not leak into content. This is stateless because
/// Gemini re-emits the full cumulative text on every frame.
pub fn is_tool_call_in_progress(text: &str) -> bool {
    let stripped = strip_gemini_card_prefix(text);
    let t = stripped.trim();
    if t.is_empty() {
        return false;
    }
    // Unclosed fenced block: the call object is still streaming.
    if t.matches("```").count() % 2 == 1 {
        return true;
    }
    // Inline `name(` with no matching close paren yet.
    for open in t.match_indices('(').map(|(i, _)| i) {
        let name = t[..open].trim();
        if is_identifier(name) && find_matching_paren(t.as_bytes(), open).is_none() {
            return true;
        }
    }
    false
}

/// Parse a single inline function call like `get_weather(location='Paris')`
/// into a `ToolCall`. Handles Python-style keyword arguments with single or
/// double quoted strings, numbers, booleans, nested dicts and lists.
pub fn parse_inline_function_call(text: &str) -> Option<ToolCall> {
    let text = text.trim();

    // After card-content stripping the call may sit alone on a line, possibly
    // preceded by the URL line that was already removed. Find `name(...)`.
    let open = text.find('(')?;
    let close = find_matching_paren(text.as_bytes(), open)?;
    let name = text[..open].trim();
    if !is_identifier(name) {
        return None;
    }
    let args = &text[open + 1..close];

    let mut obj = serde_json::Map::new();
    for (key, val) in split_key_value_args(args) {
        let parsed = parse_python_value(val)?;
        obj.insert(key.to_string(), parsed);
    }
    if obj.is_empty() && !args.trim().is_empty() {
        return None;
    }

    let arguments = serde_json::Value::Object(obj);
    let arguments_str = serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".to_string());
    Some(ToolCall {
        id: stable_call_id(name, &arguments_str),
        r#type: "function".to_string(),
        function: FunctionCall {
            name: name.to_string(),
            arguments: arguments_str,
        },
    })
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// Find the index of the paren that matches the open paren at `open`,
/// respecting nested parens, brackets, braces and string literals.
fn find_matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_str: Option<u8> = None;
    let mut escaped = false;
    let mut i = open;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == q {
                in_str = None;
            }
        } else {
            match b {
                b'\'' | b'"' => in_str = Some(b),
                b'(' | b'[' | b'{' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                b']' | b'}' => {
                    depth = depth.saturating_sub(1);
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Split `key=value, key2=value2` into (key, value) pairs at the top level,
/// respecting nesting and string literals.
fn split_key_value_args(args: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    for seg in split_top_level(args, b',') {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        if let Some(eq) = find_top_level_eq(seg) {
            let key = seg[..eq].trim();
            let val = seg[eq + 1..].trim();
            if !key.is_empty() {
                out.push((key, val));
            }
        }
    }
    out
}

/// Find the first top-level `=` in a single `key=value` segment.
fn find_top_level_eq(seg: &str) -> Option<usize> {
    let bytes = seg.as_bytes();
    let mut depth = 0i32;
    let mut in_str: Option<u8> = None;
    let mut escaped = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == q {
                in_str = None;
            }
        } else {
            match b {
                b'\'' | b'"' => in_str = Some(b),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth -= 1,
                b'=' if depth == 0 => return Some(i),
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Parse a Python-style literal (string, number, bool, None, dict, list) into
/// a `serde_json::Value`.
fn parse_python_value(raw: &str) -> Option<serde_json::Value> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let bytes = raw.as_bytes();

    // Quoted string (single or double).
    if (bytes[0] == b'\'' || bytes[0] == b'"') && find_string_end(raw, 0) == Some(raw.len() - 1) {
        let inner = &raw[1..raw.len() - 1];
        return Some(serde_json::Value::String(unescape_python_string(inner)));
    }

    // Dict `{ ... }`.
    if bytes[0] == b'{' && find_matching(bytes, 0, b'{', b'}') == Some(raw.len() - 1) {
        let inner = &raw[1..raw.len() - 1];
        let mut obj = serde_json::Map::new();
        for (k, v) in split_dict_entries(inner) {
            let key = parse_python_value(k)?.as_str()?.to_string();
            obj.insert(key, parse_python_value(v)?);
        }
        return Some(serde_json::Value::Object(obj));
    }

    // List `[ ... ]`.
    if bytes[0] == b'[' && find_matching(bytes, 0, b'[', b']') == Some(raw.len() - 1) {
        let inner = &raw[1..raw.len() - 1];
        let mut arr = Vec::new();
        for item in split_top_level(inner, b',') {
            arr.push(parse_python_value(item)?);
        }
        return Some(serde_json::Value::Array(arr));
    }

    // Numbers.
    if let Ok(n) = raw.parse::<i64>() {
        return Some(serde_json::Value::Number(n.into()));
    }
    if let Ok(f) = raw.parse::<f64>() {
        return serde_json::Number::from_f64(f).map(serde_json::Value::Number);
    }

    // Booleans / null.
    match raw {
        "True" | "true" => return Some(serde_json::Value::Bool(true)),
        "False" | "false" => return Some(serde_json::Value::Bool(false)),
        "None" | "null" => return Some(serde_json::Value::Null),
        _ => {}
    }

    None
}

/// Find the end index (exclusive) of the string literal starting at `start`.
fn find_string_end(raw: &str, start: usize) -> Option<usize> {
    let bytes = raw.as_bytes();
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn unescape_python_string(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('\'') => out.push('\''),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Find matching closing bracket, respecting strings.
fn find_matching(bytes: &[u8], start: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_str: Option<u8> = None;
    let mut escaped = false;
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == q {
                in_str = None;
            }
        } else {
            match b {
                b'\'' | b'"' => in_str = Some(b),
                x if x == open => depth += 1,
                x if x == close => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Split a dict body on top-level commas, then split each on `:` or `=`.
fn split_dict_entries(inner: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    for seg in split_top_level(inner, b',') {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        let sep = seg.find(':').or_else(|| find_top_level_eq(seg));
        if let Some(idx) = sep {
            let k = seg[..idx].trim();
            let v = seg[idx + 1..].trim();
            if !k.is_empty() {
                out.push((k, v));
            }
        }
    }
    out
}

/// Split `inner` on the given separator at the top level.
fn split_top_level(inner: &str, sep: u8) -> Vec<&str> {
    let bytes = inner.as_bytes();
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_str: Option<u8> = None;
    let mut escaped = false;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == q {
                in_str = None;
            }
        } else {
            match b {
                b'\'' | b'"' => in_str = Some(b),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth -= 1,
                x if x == sep && depth == 0 => {
                    parts.push(&inner[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
        i += 1;
    }
    parts.push(&inner[start..]);
    parts
}

/// Format one or more tool results into a prompt that reminds the model which
/// calls were made and what they returned.
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
        let part = if let Some(call) = call {
            format!(
                "<tool_call>{{\"name\":\"{}\",\"arguments\":{}}}</tool_call>\n\nHere is its output:\n{}",
                call.function.name,
                call.function.arguments,
                output
            )
        } else if let Some(id) = fallback_id {
            format!("Tool call {} result:\n{}", id, output)
        } else {
            format!("Tool result:\n{}", output)
        };
        parts.push(part);
    }

    format!("{}{}", header, parts.join("\n\n"))
}

/// Streaming state that strips `<tool_call>`…`</tool_call>` XML markers from
/// model output deltas as they arrive, so raw markers never leak into
/// OpenAI-compatible `content` chunks.
///
/// Providers that use prompt-injected tool definitions (DeepSeek, ChatGPT web
/// endpoint, Kimi, Gemini, GLM) receive tool invocations as XML markers
/// embedded in the response text. Stripping only after the stream completes
/// is too late: the raw markers have already been forwarded to the client.
/// This state machine removes markers incrementally and returns each complete
/// tool call as it finishes, so callers can emit a structured `tool_calls`
/// chunk instead.
#[derive(Debug, Default)]
pub struct XmlToolCallStripper {
    in_tool_call: bool,
    tool_call_buffer: String,
    suffix: String,
}

impl XmlToolCallStripper {
    /// Create a new stripper with no in-flight tool call.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one delta chunk.
    ///
    /// Returns `(cleaned_text, tool_call)` where `cleaned_text` is the delta
    /// with any XML markers removed (safe to forward as `content`) and
    /// `tool_call` is `Some` exactly when a complete `<tool_call>` block
    /// finished within this chunk. Markers split across arbitrary chunk
    /// boundaries are handled by an internal carry buffer.
    pub fn process(&mut self, delta: &str) -> (String, Option<ToolCall>) {
        const START: &str = "<tool_call>";
        const END: &str = "</tool_call>";
        const MAX_MARKER: usize = 12;

        let mut clean = String::new();
        let mut tool_call = None;

        let mut combined = std::mem::take(&mut self.suffix);
        combined.push_str(delta);

        let combined_len = combined.len();
        let mut scan = 0usize;

        while scan < combined_len {
            let remaining = &combined[scan..];

            if self.in_tool_call {
                if let Some(end_pos) = remaining.find(END) {
                    self.tool_call_buffer.push_str(&remaining[..end_pos]);
                    if let Some(tc) = parse_manual_tool_call(&self.tool_call_buffer) {
                        tool_call = Some(tc);
                    }
                    self.tool_call_buffer.clear();
                    self.in_tool_call = false;
                    scan += end_pos + END.len();
                } else {
                    let rlen = remaining.len();
                    let check_start = if rlen > MAX_MARKER { rlen - MAX_MARKER } else { 0 };
                    let mut prefix_end = rlen;
                    for cl in (1..=rlen - check_start).rev() {
                        if END.starts_with(&remaining[rlen - cl..]) {
                            prefix_end = rlen - cl;
                            break;
                        }
                    }
                    self.tool_call_buffer.push_str(&remaining[..prefix_end]);
                    self.suffix = remaining[prefix_end..].to_string();
                    break;
                }
            } else if let Some(start_pos) = remaining.find(START) {
                clean.push_str(&remaining[..start_pos]);
                scan += start_pos + START.len();
                self.in_tool_call = true;
            } else {
                let rlen = remaining.len();
                let check_start = if rlen > MAX_MARKER { rlen - MAX_MARKER } else { 0 };
                let mut prefix_end = rlen;
                for cl in (1..=rlen - check_start).rev() {
                    let trail = &remaining[rlen - cl..];
                    if START.starts_with(trail) || END.starts_with(trail) {
                        prefix_end = rlen - cl;
                        break;
                    }
                }
                clean.push_str(&remaining[..prefix_end]);
                self.suffix = remaining[prefix_end..].to_string();
                break;
            }
        }

        (clean, tool_call)
    }

    /// True when a `<tool_call>` block is open at the end of the last
    /// processed delta. Callers should flush `finish_pending()` after the
    /// stream ends to recover a tool call whose closing tag was truncated.
    pub fn in_tool_call(&self) -> bool {
        self.in_tool_call
    }

    /// The partial payload of an in-flight tool call. Empty when idle.
    pub fn pending_buffer(&self) -> &str {
        &self.tool_call_buffer
    }

    /// Flush a tool call that is still open when the stream ends.
    /// Returns the parsed call if the buffered payload is complete JSON.
    pub fn finish_pending(&mut self) -> Option<ToolCall> {
        if !self.in_tool_call || self.tool_call_buffer.is_empty() {
            return None;
        }
        let tc = parse_manual_tool_call(&self.tool_call_buffer);
        self.tool_call_buffer.clear();
        self.in_tool_call = false;
        self.suffix.clear();
        tc
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{FunctionCall, FunctionDefinition};

    #[test]
    fn inject_tool_prompt_basic() {
        let tools = vec![Tool {
            r#type: "function".to_string(),
            function: FunctionDefinition {
                name: "read_file".to_string(),
                description: None,
                parameters: None,
                strict: None,
            },
        }];
        let result = inject_tool_prompt("Read main.py", &tools, None);
        assert!(result.contains("read_file"));
        assert!(result.contains("Read main.py"));
        assert!(result.contains("<tool_call>"));
    }

    #[test]
    fn inject_tool_prompt_none() {
        let tools = vec![Tool {
            r#type: "function".to_string(),
            function: FunctionDefinition {
                name: "search".to_string(),
                description: None,
                parameters: None,
                strict: None,
            },
        }];
        let result = inject_tool_prompt("Hi", &tools, Some(&ToolChoice::Mode("none".to_string())));
        assert!(result.contains("Do not call any functions"));
        assert!(result.contains("Hi"));
    }

    #[test]
    fn parse_tool_calls_from_content_with_multiple_calls() {
        let content = r#"First I'll look up the weather.
<tool_call>{"name":"get_weather","arguments":{"city":"Paris"}}</tool_call>
And also check time.
<tool_call>{"name":"get_time","arguments":{"tz":"UTC"}}</tool_call>"#;
        let calls = parse_tool_calls_from_content(content).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "get_weather");
        assert_eq!(calls[1].function.name, "get_time");
    }

    #[test]
    fn strip_tool_call_markers_removes_tags() {
        let content = r#"<tool_call>{"name":"x","arguments":{}}</tool_call>Hello"#;
        let cleaned = strip_tool_call_markers(content);
        assert_eq!(cleaned, "Hello");
    }

    #[test]
    fn convert_xml_tool_calls_with_markers() {
        let text = r#"First I'll look up the weather.
<tool_call>{"name":"get_weather","arguments":{"city":"Paris"}}</tool_call>
And also check time.
<tool_call>{"name":"get_time","arguments":{"tz":"UTC"}}</tool_call>"#;
        let (cleaned, calls) = convert_xml_tool_calls(text, true);
        let calls = calls.expect("should have tool calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "get_weather");
        assert_eq!(calls[1].function.name, "get_time");
        assert!(!cleaned.contains("<tool_call>"));
        assert!(cleaned.contains("First I'll look up the weather."));
        assert!(cleaned.contains("And also check time."));
    }

    #[test]
    fn convert_xml_tool_calls_without_markers() {
        let text = "Hello, how can I help you?";
        let (cleaned, calls) = convert_xml_tool_calls(text, true);
        assert!(calls.is_none());
        assert_eq!(cleaned, text);
    }

    #[test]
    fn convert_xml_tool_calls_empty() {
        let text = "";
        let (cleaned, calls) = convert_xml_tool_calls(text, true);
        assert!(calls.is_none());
        assert_eq!(cleaned, "");
    }

    #[test]
    fn convert_xml_tool_calls_skips_when_no_tools_in_request() {
        let text = r#"<tool_call>{"name":"search","arguments":{"q":"test"}}</tool_call>"#;
        let (cleaned, calls) = convert_xml_tool_calls(text, false);
        assert!(calls.is_none());
        assert_eq!(cleaned, text);
    }

    #[test]
    fn convert_xml_tool_calls_single_call() {
        let text = r#"<tool_call>{"name":"search","arguments":{"q":"weather"}}</tool_call>"#;
        let (cleaned, calls) = convert_xml_tool_calls(text, true);
        let calls = calls.expect("should have tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "search");
        assert!(cleaned.is_empty());
    }

    #[test]
    fn convert_xml_tool_calls_with_id() {
        let text =
            r#"<tool_call>{"name":"read_file","arguments":{"path":"/etc/hosts"},"id":"call_abc"}</tool_call>"#;
        let (_, calls) = convert_xml_tool_calls(text, true);
        let calls = calls.expect("should have tool calls");
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].function.name, "read_file");
    }

    #[test]
    fn convert_xml_tool_calls_multiline_json() {
        let text = r#"<tool_call>{
            "name":"complex",
            "arguments":{"nested":"value"}
        }</tool_call>"#;
        let (_, calls) = convert_xml_tool_calls(text, true);
        let calls = calls.expect("should have tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "complex");
    }

    #[test]
    fn format_tool_results_single() {
        let call = ToolCall {
            id: "call_1".to_string(),
            r#type: "function".to_string(),
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: r#"{"path":"/etc/hosts"}"#.to_string(),
            },
        };
        let result = format_tool_results(&[(Some(&call), None, "127.0.0.1 localhost")]);
        assert!(result.contains("read_file"), "result: {result}");
        assert!(result.contains("127.0.0.1 localhost"), "result: {result}");
        assert!(result.contains("You used this tool"), "result: {result}");
    }

    #[test]
    fn parse_manual_tool_call_with_id() {
        let raw = r#"{"name":"search","arguments":"{\"q\":\"test\"}","id":"call_abc"}"#;
        let call = parse_manual_tool_call(raw).unwrap();
        assert_eq!(call.id, "call_abc");
        assert_eq!(call.function.name, "search");
    }

    #[test]
    fn parse_manual_tool_call_without_id() {
        let raw = r#"{"name":"search","arguments":{}}"#;
        let call = parse_manual_tool_call(raw).unwrap();
        assert!(call.id.starts_with("call_"));
        assert_eq!(call.function.name, "search");
    }

    #[test]
    fn stripper_single_chunk_tool_call() {
        let mut s = XmlToolCallStripper::new();
        let delta = r#"Sure, let me check.
<tool_call>{"name":"get_weather","arguments":{"city":"Paris"}}</tool_call>
Here is the result."#;
        let (clean, tc) = s.process(delta);
        let tc = tc.expect("should parse the tool call");
        assert_eq!(tc.function.name, "get_weather");
        assert_eq!(tc.function.arguments, r#"{"city":"Paris"}"#);
        assert!(!clean.contains("<tool_call>"));
        assert!(clean.contains("Sure, let me check."));
        assert!(clean.contains("Here is the result."));
        assert!(!s.in_tool_call());
    }

    #[test]
    fn stripper_marker_split_across_chunks() {
        let mut s = XmlToolCallStripper::new();
        let (clean1, tc1) = s.process(r#"<tool_call>{"name":"get_"#);
        assert!(tc1.is_none());
        assert!(clean1.is_empty());
        assert!(s.in_tool_call());

        let (clean2, tc2) = s.process(r#"weather","arguments":{"city":"Tok"#);
        assert!(tc2.is_none());
        assert!(s.in_tool_call());

        let (clean3, tc3) = s.process(r#"yo"}}</tool_call> done"#);
        assert_eq!(tc3.unwrap().function.name, "get_weather");
        assert_eq!(clean3, " done");
        assert!(!s.in_tool_call());
    }

    #[test]
    fn stripper_plain_text_passes_through() {
        let mut s = XmlToolCallStripper::new();
        let (clean, tc) = s.process("Hello there");
        assert!(tc.is_none());
        assert_eq!(clean, "Hello there");
    }

    #[test]
    fn stripper_finish_pending_recovers_truncated_call() {
        let mut s = XmlToolCallStripper::new();
        let (_, tc) = s.process(r#"<tool_call>{"name":"x","arguments":{}}"#);
        assert!(tc.is_none());
        assert!(s.in_tool_call());
        let tc = s.finish_pending().expect("should recover pending call");
        assert_eq!(tc.function.name, "x");
        assert!(!s.in_tool_call());
        assert!(s.finish_pending().is_none());
    }

    #[test]
    fn stripper_multiple_tool_calls() {
        let mut s = XmlToolCallStripper::new();
        let (_, tc1) = s.process(r#"<tool_call>{"name":"a","arguments":{}}</tool_call>"#);
        let (_, tc2) = s.process(r#"<tool_call>{"name":"b","arguments":{"x":1}}</tool_call>"#);
        assert_eq!(tc1.unwrap().function.name, "a");
        assert_eq!(tc2.unwrap().function.name, "b");
    }

    #[test]
    fn gemini_card_prefix_inline_call() {
        let text = "http://googleusercontent.com/card_content/0\nget_weather(location='Paris')";
        let calls = parse_gemini_tool_calls(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "get_weather");
        assert_eq!(
            calls[0].function.arguments,
            r#"{"location":"Paris"}"#
        );
    }

    #[test]
    fn gemini_fenced_json_call() {
        let text = "```json\n{\"name\": \"get_weather\", \"arguments\": {\"location\": \"Paris\"}}\n```";
        let calls = parse_gemini_tool_calls(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "get_weather");
        assert_eq!(calls[0].function.arguments, r#"{"location":"Paris"}"#);
    }

    #[test]
    fn gemini_fenced_function_call_args_key() {
        let text = "```function_call\n{\"name\": \"get_weather\", \"args\": {\"location\": \"Paris\"}}\n```";
        let calls = parse_gemini_tool_calls(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "get_weather");
        assert_eq!(calls[0].function.arguments, r#"{"location":"Paris"}"#);
    }

    #[test]
    fn gemini_stable_id_across_identical_text() {
        let text = "http://googleusercontent.com/card_content/0\nget_weather(location='Paris')";
        let a = parse_gemini_tool_calls(text).unwrap();
        let b = parse_gemini_tool_calls(text).unwrap();
        assert_eq!(a[0].id, b[0].id);
        assert!(a[0].id.starts_with("call_"));
    }

    #[test]
    fn gemini_strip_card_prefix_removes_url() {
        let stripped = strip_gemini_card_prefix(
            "http://googleusercontent.com/card_content/0\nIn Paris, France",
        );
        assert_eq!(stripped, "In Paris, France");
    }

    #[test]
    fn gemini_strip_code_event_artifacts() {
        let stripped = strip_gemini_card_prefix("```text?code_stdout&code_event_index=1\n4\n```");
        assert_eq!(stripped, "```text\n4\n```");
    }

    #[test]
    fn gemini_prose_with_parens_is_not_a_call() {
        let text = "In Paris, France, current conditions are sunny with a temperature of 20°C (feels like 24°C).";
        assert!(parse_gemini_tool_calls(text).is_none());
    }

    #[test]
    fn gemini_call_in_progress_inline() {
        assert!(is_tool_call_in_progress("http://googleusercontent.com/card_content/0\nget_weather(location='Par"));
    }

    #[test]
    fn gemini_call_in_progress_fence() {
        assert!(is_tool_call_in_progress("```json\n{\"name\": \"get_weat"));
    }

    #[test]
    fn gemini_answer_not_in_progress() {
        assert!(!is_tool_call_in_progress(
            "In Paris, France, current conditions are sunny with a temperature of 20°C (feels like 24°C)."
        ));
        assert!(!is_tool_call_in_progress("```python\nprint(2 + 2)\n```"));
    }

    #[test]
    fn gemini_inline_call_nested_args() {
        let text = "get_weather(location='Paris', units={'temp': 'c'}, days=[1, 2])";
        let calls = parse_gemini_tool_calls(text).unwrap();
        assert_eq!(calls[0].function.name, "get_weather");
        let args: serde_json::Value =
            serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["location"], "Paris");
        assert_eq!(args["units"]["temp"], "c");
        assert_eq!(args["days"][1], 2);
    }

    #[test]
    fn gemini_card_content_with_fenced_json() {
        // This is the exact format reported in BROKEN_FEATURES.md 1.9
        let text = "http://googleusercontent.com/card_content/0\n```json\n{\"name\": \"get_weather\", \"arguments\": {\"location\": \"Paris\"}}\n```";
        let calls = parse_gemini_tool_calls(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "get_weather");
        assert_eq!(calls[0].function.arguments, r#"{"location":"Paris"}"#);
    }
}
