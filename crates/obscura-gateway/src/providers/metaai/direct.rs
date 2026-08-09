//! Meta AI Ecto DGW WebSocket chat client.
//!
//! Talks directly to Meta's Ecto-era DGW WebSocket endpoint
//! (`wss://gateway.meta.ai/ws/clippy`) using native browser-session auth:
//! session cookies plus the page-injected `ecto1:` WebSocket token. There is
//! no anonymous or temp-user flow — the client fails closed whenever the
//! authenticated session state is missing (see [`super::auth`]).
//!
//! Turn flow (mirrors the meta.ai web app's `muse-spark-web-main.ts` bundle):
//!
//! 1. `POST https://www.meta.ai/api/graphql` warmup query
//!    (doc_id `e7f802582dbfed8e181b012e010993eb`) initializes the
//!    conversation on Meta's side.
//! 2. A GraphQL mode switch (doc_id `c32bbe999c48e64e855dc63177d5153f`) sets
//!    the conversation's reasoning level (`think_fast` / `think_hard`).
//! 3. The prompt is sent over the DGW WebSocket: a `0x0f` intro frame
//!    announces the conversation id, then a `0x0d` prompt frame carries a
//!    base64 protobuf template mutated at known field paths (conversation id,
//!    prompt text, timestamps, message ids).
//! 4. Responses arrive as `full` / `patch` JSON events, possibly several
//!    concatenated in one WS message. They are scanned by brace depth (a
//!    faithful port of `parseWsResponseEvents`) and re-assembled into content
//!    deltas.
//!
//! Conversation continuity is handled by [`super::state::MetaAiSessionStore`],
//! keyed by the gateway's `session_url` (`https://www.meta.ai/c/<id>`): a new
//! conversation uses the HOME template with `isNewConversation=true`, while a
//! continuation reuses the cached conversation id, sends only the latest user
//! turn with the CHAT template, and lets Meta append to the existing tree.
//!
//! Tool calling is prompt-injected via [`crate::providers::tool_call`]
//! (`<tool_call>` XML markers) and converted to native OpenAI `tool_calls` on
//! the way out — the same fallback used by the DeepSeek/Gemini/Grok adapters.
//! The DGW endpoint exposes no native function-calling channel.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::stream::{BoxStream, StreamExt};
use futures::SinkExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::GatewayError;
use crate::models::{
    ChatCompletionChunk, ChatCompletionChoice, ChatCompletionRequest, ChatCompletionResponse,
    ChatContent, ChatMessage, ChatMessageDelta, ChunkChoice, ToolCall, Usage,
};
use crate::providers::tokenizer::estimate_tokens;
use crate::providers::tool_call::{
    convert_xml_tool_calls, inject_tool_prompt, XmlToolCallStripper,
};
use crate::session::SessionHandle;

use super::auth::{build_cookie_header, extract_ecto_token, extract_meta_cookies, validate_meta_session};
use super::state::MetaAiSessionStore;
use super::templates::{CHAT_TEMPLATE_B64, HOME_TEMPLATE_B64};

/// GraphQL endpoint used for warmup and mode switch.
const GRAPHQL_API_URL: &str = "https://www.meta.ai/api/graphql";
/// DGW WebSocket endpoint.
const WS_URL: &str = "wss://gateway.meta.ai/ws/clippy";

/// Persisted-query doc ids from the meta.ai web app bundle. The previous
/// Abra send mutation was retired when Meta removed its input types from the
/// schema; warmup and mode switch are the only HTTP calls the DGW flow needs.
const DOC_WARMUP: &str = "e7f802582dbfed8e181b012e010993eb";
const DOC_MODE_SWITCH: &str = "c32bbe999c48e64e855dc63177d5153f";

/// DGW handshake constants (from `muse-spark-web-main.ts`).
const WS_APP_ID: &str = "1522763855472543";
const WS_APP_VERSION: &str = "1.0.0";
const WS_AUTHTYPE: &str = "15:0";
const WS_DGW_VERSION: &str = "5";
const WS_DGW_UUID: &str = "0";
const WS_TIER: &str = "prod";

/// WS frame types.
const WS_INTRO_FRAME_TYPE: u8 = 0x0f;
const WS_PROMPT_FRAME_TYPE: u8 = 0x0d;
const WS_PROMPT_FRAME_FLAG: u8 = 0x80;

/// Root conversation branch path (mirrors `META_AI_ROOT_BRANCH_PATH`).
#[allow(dead_code)]
const ROOT_BRANCH_PATH: &str = "0";

/// Browser fingerprint the web app expects from the WS client.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";

/// Idle timeout for one DGW WebSocket turn. `think_hard` turns can stream for
/// minutes, so the deadline is generous; the upstream also closes idle
/// connections on a similar window.
const WS_TIMEOUT: Duration = Duration::from_secs(300);

/// Response max for the brace-depth scanner (defensive cap; real messages are
/// a few KB).
const MAX_EVENT_PAYLOAD: usize = 16 * 1024 * 1024;

const BASE62_ALPHABET: &[u8] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Map a gateway model id to the DGW reasoning mode.
fn mode_for_model(model_id: &str) -> &'static str {
    match model_id {
        "muse-spark-thinking" | "muse-spark-contemplating" => "think_hard",
        _ => "think_fast",
    }
}

/// Effective DGW reasoning mode for a turn: the per-request `thinking`
/// override wins when present, otherwise the model's default mode. The DGW
/// mode-switch GraphQL is issued on every turn, so the toggle is honored
/// natively rather than silently dropped.
fn effective_mode(model_id: &str, thinking: Option<bool>) -> &'static str {
    match thinking {
        Some(true) => "think_hard",
        Some(false) => "think_fast",
        None => mode_for_model(model_id),
    }
}

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn current_timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn encode_base62(mut value: u128, pad_length: usize) -> String {
    let mut digits = Vec::new();
    while value > 0 {
        digits.push(BASE62_ALPHABET[(value % 62) as usize] as char);
        value /= 62;
    }
    digits.reverse();
    let mut out: String = digits.into_iter().collect();
    while out.len() < pad_length {
        out.insert(0, '0');
    }
    out
}

/// Client-generated conversation id (`c.<base62>`, 19 chars): 44-bit ms
/// timestamp + 64-bit random, packed like the web client's
/// `generateMetaConversationId`.
fn generate_conversation_id() -> String {
    let timestamp = (current_timestamp_ms() as u128) & ((1u128 << 44) - 1);
    let random = (rand::random::<u64>() as u128) & ((1u128 << 64) - 1);
    let packed = (timestamp << 64) | random;
    format!("c.{}", encode_base62(packed, 19))
}

/// Numeric message id used inside the proto template's `[2,1,2].3` field
/// (mirrors the web client's wsChat default:
/// `Number(\`${submittedMs}${String(Math.floor(Math.random()*10000)).padStart(4,"0")}\`)`).
fn generate_unique_message_id(submitted_ms: i64) -> i64 {
    let suffix = (rand::random::<u32>() % 10_000) as i64;
    format!("{submitted_ms}{suffix:04}")
        .parse()
        .unwrap_or(submitted_ms)
}

// ─── Protobuf helpers ────────────────────────────────────────────────────────
//
// The DGW prompt payload is a protobuf message with a few fields that must be
// replaced per conversation. Only length-delimited (wire type 2), varint
// (0) and fixed (1/5) fields appear in the captured templates.

#[derive(Debug, Clone)]
enum ProtoValue {
    Varint(u64),
    Fixed64(u64),
    Fixed32(u32),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone)]
struct ProtoField {
    number: u32,
    wire_type: u32,
    value: ProtoValue,
}

fn encode_varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        if value < 0x80 {
            out.push(value as u8);
            break;
        }
        out.push(((value & 0x7f) as u8) | 0x80);
        value >>= 7;
    }
    out
}

fn decode_varint(data: &[u8], offset: usize) -> Option<(u64, usize)> {
    let mut shift = 0u32;
    let mut value = 0u64;
    let mut off = offset;
    loop {
        let byte = *data.get(off)?;
        off += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((value, off));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

fn parse_proto_fields(data: &[u8]) -> Option<Vec<ProtoField>> {
    let mut fields = Vec::new();
    let mut offset = 0usize;
    while offset < data.len() {
        let (tag, next) = decode_varint(data, offset)?;
        offset = next;
        let number = (tag >> 3) as u32;
        let wire_type = (tag & 0x07) as u32;
        match wire_type {
            0 => {
                let (value, n) = decode_varint(data, offset)?;
                offset = n;
                fields.push(ProtoField {
                    number,
                    wire_type,
                    value: ProtoValue::Varint(value),
                });
            }
            1 => {
                if offset + 8 > data.len() {
                    return None;
                }
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&data[offset..offset + 8]);
                offset += 8;
                fields.push(ProtoField {
                    number,
                    wire_type,
                    value: ProtoValue::Fixed64(u64::from_le_bytes(buf)),
                });
            }
            2 => {
                let (len, n) = decode_varint(data, offset)?;
                offset = n;
                let len = len as usize;
                if offset + len > data.len() {
                    return None;
                }
                fields.push(ProtoField {
                    number,
                    wire_type,
                    value: ProtoValue::Bytes(data[offset..offset + len].to_vec()),
                });
                offset += len;
            }
            5 => {
                if offset + 4 > data.len() {
                    return None;
                }
                let mut buf = [0u8; 4];
                buf.copy_from_slice(&data[offset..offset + 4]);
                offset += 4;
                fields.push(ProtoField {
                    number,
                    wire_type,
                    value: ProtoValue::Fixed32(u32::from_le_bytes(buf)),
                });
            }
            _ => return None,
        }
    }
    Some(fields)
}

fn serialize_proto_fields(fields: &[ProtoField]) -> Vec<u8> {
    let mut out = Vec::new();
    for field in fields {
        out.extend_from_slice(&encode_varint(((field.number << 3) | field.wire_type) as u64));
        match &field.value {
            ProtoValue::Varint(v) => out.extend_from_slice(&encode_varint(*v)),
            ProtoValue::Fixed64(v) => out.extend_from_slice(&v.to_le_bytes()),
            ProtoValue::Fixed32(v) => out.extend_from_slice(&v.to_le_bytes()),
            ProtoValue::Bytes(bytes) => {
                out.extend_from_slice(&encode_varint(bytes.len() as u64));
                out.extend_from_slice(bytes);
            }
        }
    }
    out
}

/// Descend into `path` and run `mutator` on the target length-delimited field
/// (its raw payload bytes). Returns false when any hop is missing or not a
/// length-delimited submessage, so mutations never corrupt the template.
fn traverse_and_mutate(
    fields: &mut [ProtoField],
    path: &[u32],
    mutator: &mut dyn FnMut(&mut Vec<u8>),
) -> bool {
    let Some(first) = path.first() else {
        return false;
    };
    let Some(field) = fields.iter_mut().find(|f| f.number == *first) else {
        return false;
    };
    let ProtoValue::Bytes(raw) = &field.value else {
        return false;
    };
    if path.len() == 1 {
        let mut value = raw.clone();
        mutator(&mut value);
        field.value = ProtoValue::Bytes(value);
        return true;
    }
    let mut nested = match parse_proto_fields(raw) {
        Some(nested) => nested,
        None => return false,
    };
    if traverse_and_mutate(&mut nested, &path[1..], mutator) {
        field.value = ProtoValue::Bytes(serialize_proto_fields(&nested));
        return true;
    }
    false
}

/// Replace a nested length-delimited field's bytes.
fn set_nested_bytes(raw: &mut Vec<u8>, number: u32, replacement: &[u8]) {
    if let Some(mut nested) = parse_proto_fields(raw) {
        if let Some(field) = nested.iter_mut().find(|f| f.number == number) {
            field.value = ProtoValue::Bytes(replacement.to_vec());
        }
        *raw = serialize_proto_fields(&nested);
    }
}

fn write_u24_le(value: usize, out: &mut [u8]) {
    out[0] = (value & 0xff) as u8;
    out[1] = ((value >> 8) & 0xff) as u8;
    out[2] = ((value >> 16) & 0xff) as u8;
}

// ─── WS frame builders ───────────────────────────────────────────────────────

/// Intro frame announcing the conversation to the DGW endpoint.
fn build_ws_intro_frame(conversation_id: &str) -> Vec<u8> {
    let payload = serde_json::json!({
        "x-dgw-app-x-ecto-conversation-id": conversation_id,
        "x-dgw-app-client-payload-type": "PROTO_INSIDE_JSON",
    })
    .to_string();
    let mut frame = vec![0u8; 6 + payload.len()];
    frame[0] = WS_INTRO_FRAME_TYPE;
    write_u24_le(payload.len(), &mut frame[3..6]);
    frame[6..].copy_from_slice(payload.as_bytes());
    frame
}

/// Prompt frame carrying the mutated protobuf payload.
///
/// The mutation map is the faithful port of the web client's
/// `buildWsPromptFrame`: `[1,1,5]` conversation id, `[2,1,1]` user message
/// id, `[2,1,2]` conv id + submitted-ms + unique-message-id, `[2,2]` prompt
/// text, `[1,5]` timestamps, `[1,6]` request id, `[1,10,4]` conversation id.
#[allow(clippy::too_many_arguments)]
fn build_ws_prompt_frame(
    prompt: &str,
    conversation_id: &str,
    template_b64: &str,
    request_id: &str,
    user_message_id: &str,
    submitted_ms: i64,
    unique_message_id: i64,
) -> Result<Vec<u8>, GatewayError> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(template_b64)
        .map_err(|e| GatewayError::Internal(format!("meta.ai template decode failed: {e}")))?;
    let mut proto_fields = parse_proto_fields(&raw).ok_or_else(|| {
        GatewayError::Internal("meta.ai template is not valid protobuf".to_string())
    })?;

    // [1,1,5] conversationId
    let conv = conversation_id.as_bytes();
    let mut set_conv_at_115 = |raw: &mut Vec<u8>| set_nested_bytes(raw, 5, conv);
    if !traverse_and_mutate(&mut proto_fields, &[1, 1], &mut set_conv_at_115) {
        return Err(GatewayError::Internal(
            "meta.ai template: missing [1,1] for conversation id".to_string(),
        ));
    }
    // [2,1,1] userMessageId
    let user_id = user_message_id.as_bytes();
    let mut set_user_at_211 = |raw: &mut Vec<u8>| set_nested_bytes(raw, 1, user_id);
    if !traverse_and_mutate(&mut proto_fields, &[2, 1], &mut set_user_at_211) {
        return Err(GatewayError::Internal(
            "meta.ai template: missing [2,1] for user message id".to_string(),
        ));
    }
    // [2,1,2] convId + submittedMs + uniqueMessageId
    let mut set_conv_ts_at_212 = |raw: &mut Vec<u8>| {
        if let Some(mut nested) = parse_proto_fields(raw) {
            for field in nested.iter_mut() {
                match field.number {
                    1 => field.value = ProtoValue::Bytes(conv.to_vec()),
                    2 => field.value = ProtoValue::Varint(submitted_ms as u64),
                    3 => field.value = ProtoValue::Varint(unique_message_id as u64),
                    _ => {}
                }
            }
            *raw = serialize_proto_fields(&nested);
        }
    };
    if !traverse_and_mutate(&mut proto_fields, &[2, 1, 2], &mut set_conv_ts_at_212) {
        return Err(GatewayError::Internal(
            "meta.ai template: missing [2,1,2] for timestamps".to_string(),
        ));
    }
    // [2,2] prompt text
    let prompt_bytes = prompt.as_bytes();
    let mut set_prompt_at_22 = |raw: &mut Vec<u8>| set_nested_bytes(raw, 2, prompt_bytes);
    if !traverse_and_mutate(&mut proto_fields, &[2], &mut set_prompt_at_22) {
        return Err(GatewayError::Internal(
            "meta.ai template: missing [2] for prompt text".to_string(),
        ));
    }
    // [1,5] timestamps: field 1 = submitted_ms + 1, field 3 = submitted_ms
    let mut set_ts_at_15 = |raw: &mut Vec<u8>| {
        if let Some(mut nested) = parse_proto_fields(raw) {
            for field in nested.iter_mut() {
                match field.number {
                    1 => field.value = ProtoValue::Varint((submitted_ms + 1) as u64),
                    3 => field.value = ProtoValue::Varint(submitted_ms as u64),
                    _ => {}
                }
            }
            *raw = serialize_proto_fields(&nested);
        }
    };
    if !traverse_and_mutate(&mut proto_fields, &[1, 5], &mut set_ts_at_15) {
        return Err(GatewayError::Internal(
            "meta.ai template: missing [1,5] for timestamps".to_string(),
        ));
    }
    // [1,6] requestId
    let req_id = request_id.as_bytes();
    let mut set_req_at_16 = |raw: &mut Vec<u8>| set_nested_bytes(raw, 6, req_id);
    if !traverse_and_mutate(&mut proto_fields, &[1], &mut set_req_at_16) {
        return Err(GatewayError::Internal(
            "meta.ai template: missing [1] for request id".to_string(),
        ));
    }
    // [1,10,4] conversationId
    let mut set_conv_at_1104 = |raw: &mut Vec<u8>| set_nested_bytes(raw, 4, conv);
    if !traverse_and_mutate(&mut proto_fields, &[1, 10], &mut set_conv_at_1104) {
        return Err(GatewayError::Internal(
            "meta.ai template: missing [1,10] for conversation id".to_string(),
        ));
    }

    let updated_b64 =
        base64::engine::general_purpose::STANDARD.encode(serialize_proto_fields(&proto_fields));
    let outer = serde_json::json!({ "req-id": request_id, "payload": updated_b64 }).to_string();

    let mut msg_body = Vec::with_capacity(2 + outer.len());
    msg_body.push(0); // message sequence
    msg_body.push(WS_PROMPT_FRAME_FLAG);
    msg_body.extend_from_slice(outer.as_bytes());

    let mut frame = vec![0u8; 6 + msg_body.len()];
    frame[0] = WS_PROMPT_FRAME_TYPE;
    write_u24_le(msg_body.len(), &mut frame[3..6]);
    frame[6..].copy_from_slice(&msg_body);
    Ok(frame)
}

/// Build the DGW WebSocket URL with the handshake query parameters.
fn build_ws_url(authorization: &str, request_id: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("x-dgw-appid", WS_APP_ID),
            ("x-dgw-appversion", WS_APP_VERSION),
            ("x-dgw-authtype", WS_AUTHTYPE),
            ("x-dgw-version", WS_DGW_VERSION),
            ("x-dgw-uuid", WS_DGW_UUID),
            ("x-dgw-tier", WS_TIER),
            ("Authorization", authorization),
            ("x-dgw-app-origin", "meta.ai"),
            ("x-dgw-app-clippy-request-id", request_id),
            ("x-dgw-app-clippy-async", "true"),
        ])
        .finish();
    format!("{WS_URL}?{query}")
}

// ─── WS response parser ──────────────────────────────────────────────────────

/// A single event inside a DGW WS message.
#[derive(Debug, Clone, serde::Deserialize)]
struct WsResponseEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    response: Option<WsResponseBody>,
    #[serde(default)]
    operations: Option<Vec<WsPatchOperation>>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct WsResponseBody {
    #[serde(default)]
    sections: Vec<WsSection>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct WsSection {
    #[serde(rename = "view_model")]
    #[serde(default)]
    view_model: Option<WsViewModel>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct WsViewModel {
    #[serde(default)]
    primitive: Option<WsPrimitive>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct WsPrimitive {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct WsPatchOperation {
    #[serde(default)]
    op: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    value: Option<serde_json::Value>,
}

/// Scan a WS message for concatenated JSON events using a brace-depth scanner
/// (faithful port of the web client's `parseWsResponseEvents`).
fn parse_ws_response_events(payload: &str) -> Vec<WsResponseEvent> {
    let mut events = Vec::new();
    let mut start: Option<usize> = None;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (index, ch) in payload.char_indices() {
        if start.is_none() {
            if ch == '{' {
                start = Some(index);
                depth = 1;
                in_string = false;
                escape = false;
            }
            continue;
        }
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
        } else if ch == '{' {
            depth += 1;
        } else if ch == '}' {
            depth -= 1;
            if depth == 0 {
                if let Some(s) = start {
                    if index - s <= MAX_EVENT_PAYLOAD {
                        if let Ok(event) = serde_json::from_str(&payload[s..=index]) {
                            events.push(event);
                        }
                    }
                    start = None;
                }
            }
        }
    }
    events
}

/// Accumulator that reassembles `full`/`patch` events into content deltas.
#[derive(Debug, Default)]
struct WsAccumulator {
    content: String,
    deltas: Vec<String>,
}

impl WsAccumulator {
    fn apply(&mut self, raw: &str) {
        for event in parse_ws_response_events(raw) {
            if event.event_type == "full" {
                if let Some(body) = event.response {
                    for section in body.sections {
                        let text = section
                            .view_model
                            .and_then(|vm| vm.primitive)
                            .and_then(|p| p.text)
                            .unwrap_or_default();
                        if text.is_empty() || text == self.content {
                            continue;
                        }
                        let delta = if self.content.is_empty() {
                            text.clone()
                        } else if text.starts_with(&self.content) {
                            text[self.content.len()..].to_string()
                        } else {
                            // Server reset/refresh: treat the whole text as new.
                            text.clone()
                        };
                        if !delta.is_empty() {
                            self.deltas.push(delta);
                        }
                        self.content = text;
                    }
                }
            } else if event.event_type == "patch" {
                if let Some(operations) = event.operations {
                    for op in operations {
                        let is_text_path = op.path.as_deref()
                            == Some("/sections/0/view_model/primitive/text");
                        if op.op.as_deref() == Some("delta") && is_text_path {
                            if let Some(text) = op.value.as_ref().and_then(|v| v.as_str()) {
                                self.deltas.push(text.to_string());
                                self.content.push_str(text);
                            }
                        }
                    }
                }
            }
        }
    }
}

// ─── OpenAI message normalization ────────────────────────────────────────────

#[derive(Debug, Clone)]
struct NormalizedMessage {
    role: String,
    content: String,
}

/// Normalized history plus the two prompts the DGW flow needs (mirrors
/// `parseOpenAIMessages` in `muse-spark-web-main.ts`).
#[derive(Debug, Default)]
struct ParsedHistory {
    /// Whole history folded into one prompt (new conversations). The last
    /// user turn appears bare, without a `user:` prefix.
    folded_prompt: String,
    /// Just the last user turn, sent on its own when continuing a cached
    /// conversation.
    latest_user_content: String,
}

fn parse_openai_messages(messages: &[ChatMessage]) -> ParsedHistory {
    let mut extracted: Vec<NormalizedMessage> = Vec::new();
    for message in messages {
        let mut role = message.role.clone();
        if role == "developer" {
            role = "system".to_string();
        }
        let content = message.content.as_text().trim().to_string();
        if content.is_empty() {
            continue;
        }
        extracted.push(NormalizedMessage { role, content });
    }

    if extracted.is_empty() {
        return ParsedHistory::default();
    }

    let last_user_index = extracted
        .iter()
        .rposition(|m| m.role == "user")
        .map(|i| i as i64)
        .unwrap_or(-1);

    let folded_prompt = extracted
        .iter()
        .enumerate()
        .map(|(index, message)| {
            if index as i64 == last_user_index {
                message.content.clone()
            } else {
                format!("{}: {}", message.role, message.content)
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
        .trim()
        .to_string();

    let latest_user_content = if last_user_index >= 0 {
        extracted[last_user_index as usize].content.clone()
    } else {
        String::new()
    };

    ParsedHistory {
        folded_prompt,
        latest_user_content,
    }
}

// ─── WebSocket transport ─────────────────────────────────────────────────────

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Connect to the DGW endpoint and send the intro + prompt frames.
async fn ws_connect_and_send(
    prompt: &str,
    conversation_id: &str,
    authorization: &str,
    cookie_header: &str,
    template_b64: &str,
) -> Result<WsStream, GatewayError> {
    let request_id = new_uuid();
    let ws_url = build_ws_url(authorization, &request_id);
    tracing::debug!("metaai ws connect url={} authorization_preview={}..", ws_url, &authorization.get(..16).unwrap_or(""));

    let mut request = ws_url
        .into_client_request()
        .map_err(|e| GatewayError::Internal(format!("invalid Meta AI WS URL: {e}")))?;
    request
        .headers_mut()
        .insert(
            "Cookie",
            HeaderValue::from_str(cookie_header).map_err(|e| {
                GatewayError::Internal(format!("invalid Meta AI cookie header: {e}"))
            })?,
        );
    request
        .headers_mut()
        .insert("User-Agent", HeaderValue::from_static(USER_AGENT));
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_static("https://meta.ai"));

    let (mut ws, response) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        connect_async(request),
    )
    .await
    .map_err(|_| GatewayError::Provider("Meta AI WS handshake timed out".to_string()))?
    .map_err(|e| {
        // tungstenite returns Err(Error::Http) for any non-101 response, so an
        // HTTP-level auth rejection surfaces here rather than in the status
        // check below. Classify 401/403 so the session is marked dirty and the
        // user gets a re-login hint.
        if let tokio_tungstenite::tungstenite::Error::Http(resp) = &e {
            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                return GatewayError::Auth(format!(
                    "Meta AI WebSocket handshake rejected with HTTP {status}. Your meta.ai \
                     cookies or ecto1 token may be expired; re-login and re-run."
                ));
            }
        }
        GatewayError::Provider(format!("Meta AI WS handshake failed: {e}"))
    })?;
    let status = response.status().as_u16();
    if status != 101 {
        return Err(GatewayError::Auth(format!(
            "Meta AI WebSocket handshake rejected with HTTP {status}. Your meta.ai \
             cookies or ecto1 token may be expired; re-login and re-run."
        )));
    }

    ws.send(Message::Binary(build_ws_intro_frame(conversation_id).into()))
        .await
        .map_err(|e| GatewayError::Provider(format!("Meta AI WS intro send failed: {e}")))?;

    let user_message_id = new_uuid();
    let submitted_ms = current_timestamp_ms();
    let unique_message_id = generate_unique_message_id(submitted_ms);
    let prompt_frame = build_ws_prompt_frame(
        prompt,
        conversation_id,
        template_b64,
        &request_id,
        &user_message_id,
        submitted_ms,
        unique_message_id,
    )?;
    ws.send(Message::Binary(prompt_frame.into()))
        .await
        .map_err(|e| GatewayError::Provider(format!("Meta AI WS prompt send failed: {e}")))?;

    Ok(ws)
}

/// Read one WS message and feed it to the accumulator. Returns false when the
/// connection closed normally.
async fn ws_read_message(
    ws: &mut WsStream,
    acc: &mut WsAccumulator,
) -> Result<bool, GatewayError> {
    match ws.next().await {
        Some(Ok(Message::Text(text))) => {
            tracing::debug!("metaai ws read text len={} preview={}", text.len(), &text.get(..80).unwrap_or(""));
            acc.apply(text.as_str());
            Ok(true)
        }
        Some(Ok(Message::Binary(data))) => {
            tracing::debug!("metaai ws read binary len={} bytes={:02x?}", data.len(), &data[..data.len().min(48)]);
            let raw = String::from_utf8_lossy(&data);
            acc.apply(&raw);
            Ok(true)
        }
        Some(Ok(Message::Ping(payload))) => {
            let _ = ws.send(Message::Pong(payload)).await;
            Ok(true)
        }
        Some(Ok(Message::Close(frame))) => {
            if let Some(frame) = frame {
                let code: u16 = frame.code.into();
                if matches!(code, 4001..=4004) || code == 401 {
                    return Err(GatewayError::Auth(format!(
                        "Meta AI WebSocket rejected the session (close code {code}). \
                         Your meta.ai cookies or ecto1 token may be expired; re-login \
                         and re-run."
                    )));
                }
            }
            Ok(false)
        }
        Some(Ok(_)) => Ok(true),
        Some(Err(e)) => Err(GatewayError::Provider(format!("Meta AI WS error: {e}"))),
        None => Ok(false),
    }
}

/// Run one full DGW turn, collecting the assistant text.
async fn ws_chat(
    prompt: &str,
    conversation_id: &str,
    authorization: &str,
    cookie_header: &str,
    template_b64: &str,
) -> Result<WsAccumulator, GatewayError> {
    let mut ws =
        ws_connect_and_send(prompt, conversation_id, authorization, cookie_header, template_b64)
            .await?;
    let mut acc = WsAccumulator::default();
    let deadline = tokio::time::Instant::now() + WS_TIMEOUT;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                return Err(GatewayError::Provider(
                    "Meta AI WebSocket timed out waiting for a response".to_string(),
                ));
            }
            keep_going = ws_read_message(&mut ws, &mut acc) => {
                if !keep_going? {
                    break;
                }
            }
        }
    }
    Ok(acc)
}

// ─── Streaming emit context ──────────────────────────────────────────────────

/// Streaming state that strips tool-call XML and emits OpenAI chunks.
struct StreamEmitCtx {
    tx: mpsc::UnboundedSender<Result<ChatCompletionChunk, GatewayError>>,
    id: String,
    created: i64,
    model: String,
    session_url: String,
    request_had_tools: bool,
    sent_first_chunk: bool,
    stripper: XmlToolCallStripper,
    collected_calls: Vec<ToolCall>,
}

impl StreamEmitCtx {
    fn emit_delta(&mut self, delta: &str) {
        let clean = if self.request_had_tools {
            let (clean, tool_call) = self.stripper.process(delta);
            if let Some(tool_call) = tool_call {
                self.collected_calls.push(tool_call);
            }
            clean
        } else {
            delta.to_string()
        };
        if clean.is_empty() {
            return;
        }
        let role = if self.sent_first_chunk {
            None
        } else {
            self.sent_first_chunk = true;
            Some("assistant".to_string())
        };
        let _ = self.tx.send(Ok(ChatCompletionChunk {
            id: self.id.clone(),
            object: "chat.completion.chunk".to_string(),
            created: self.created,
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChatMessageDelta {
                    role,
                    content: Some(clean),
                    reasoning_content: None,
                    citations: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            session_url: Some(self.session_url.clone()),
        }));
    }

    fn emit_finish(&mut self, finish_reason: &str) {
        let role = if self.sent_first_chunk {
            None
        } else {
            Some("assistant".to_string())
        };
        let _ = self.tx.send(Ok(ChatCompletionChunk {
            id: self.id.clone(),
            object: "chat.completion.chunk".to_string(),
            created: self.created,
            model: self.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChatMessageDelta {
                    role,
                    content: None,
                    reasoning_content: None,
                    citations: None,
                    tool_calls: None,
                },
                finish_reason: Some(finish_reason.to_string()),
            }],
            session_url: Some(self.session_url.clone()),
        }));
    }

    fn emit_error(&self, error: GatewayError) {
        let _ = self.tx.send(Err(error));
    }
}

/// Run one full DGW turn, emitting content deltas as they arrive.
async fn ws_chat_emit(
    prompt: &str,
    conversation_id: &str,
    authorization: &str,
    cookie_header: &str,
    template_b64: &str,
    ctx: &mut StreamEmitCtx,
) -> Result<(), GatewayError> {
    let mut ws =
        ws_connect_and_send(prompt, conversation_id, authorization, cookie_header, template_b64)
            .await?;
    let mut acc = WsAccumulator::default();
    let deadline = tokio::time::Instant::now() + WS_TIMEOUT;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                return Err(GatewayError::Provider(
                    "Meta AI WebSocket timed out waiting for a response".to_string(),
                ));
            }
            keep_going = ws_read_message(&mut ws, &mut acc) => {
                if !keep_going? {
                    break;
                }
                for delta in &acc.deltas {
                    ctx.emit_delta(delta);
                }
                acc.deltas.clear();
            }
        }
    }
    Ok(())
}

// ─── Direct client ───────────────────────────────────────────────────────────

pub struct DirectClient {
    http: reqwest::Client,
    model_id: String,
    store: MetaAiSessionStore,
    cookie_header: String,
    /// `ecto1:<token>` WebSocket authorization value.
    authorization: String,
}

impl DirectClient {
    pub async fn new(
        session: SessionHandle,
        model_id: &str,
        store: MetaAiSessionStore,
    ) -> Result<Self, GatewayError> {
        let meta_cookies = extract_meta_cookies(&session);
        validate_meta_session(&meta_cookies)?;
        let cookie_header = build_cookie_header(&meta_cookies);
        let authorization = extract_ecto_token(&cookie_header).await?;
        let http = Self::build_http_client()?;
        Ok(Self {
            http,
            model_id: model_id.to_string(),
            store,
            cookie_header,
            authorization,
        })
    }

    fn build_http_client() -> Result<reqwest::Client, GatewayError> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static(USER_AGENT),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("multipart/mixed, application/json"),
        );
        headers.insert(
            reqwest::header::ACCEPT_LANGUAGE,
            HeaderValue::from_static("en-US,en;q=0.9"),
        );
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            reqwest::header::ORIGIN,
            HeaderValue::from_static("https://www.meta.ai"),
        );
        headers.insert(
            reqwest::header::REFERER,
            HeaderValue::from_static("https://www.meta.ai/"),
        );
        headers.insert("x-asbd-id", HeaderValue::from_static("129477"));

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| GatewayError::Internal(format!("failed to build HTTP client: {e}")))?;
        Ok(client)
    }

    /// GraphQL warmup / mode-switch POST. Cookie-authed; fails closed on HTTP
    /// or body-level GraphQL errors.
    async fn graphql_post(
        &self,
        doc_id: &str,
        variables: serde_json::Value,
        label: &str,
    ) -> Result<(), GatewayError> {
        let body = serde_json::json!({ "doc_id": doc_id, "variables": variables });
        let builder = self
            .http
            .post(GRAPHQL_API_URL)
            .header("Cookie", &self.cookie_header)
            .json(&body);
        let response = crate::providers::send_with_retry(builder).await?;
        let status = response.status().as_u16();
        if status != 200 {
            let text = response.text().await.unwrap_or_default();
            return Err(GatewayError::Provider(format!(
                "{label} failed: HTTP {status}: {text}"
            )));
        }
        let text = response.text().await.unwrap_or_default();
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(first_error) = json
                .get("errors")
                .and_then(|e| e.as_array())
                .and_then(|errors| errors.first())
            {
                let message = first_error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown GraphQL error");
                return Err(GatewayError::Provider(format!("{label} failed: {message}")));
            }
        }
        Ok(())
    }

    fn estimate_cost(&self, prompt_text: &str, completion_text: &str) -> Usage {
        let prompt_tokens = estimate_tokens("metaai", &self.model_id, prompt_text);
        let completion_tokens = estimate_tokens("metaai", &self.model_id, completion_text);
        Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
        }
    }

    /// Prepare the prompt, conversation context, and templates for a turn.
    ///
    /// Returns `(prompt, conversation_id, session_url, is_new, request_had_tools)`.
    fn prepare_turn(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<(String, String, String, bool, bool), GatewayError> {
        let parsed = parse_openai_messages(&request.messages);
        if parsed.folded_prompt.is_empty() {
            return Err(GatewayError::BadRequest(
                "empty query after processing messages".to_string(),
            ));
        }

        let (conversation, mut is_continuation) = self.store.get_or_create(
            request.session_url.as_deref(),
            &request.model,
            generate_conversation_id(),
        );
        // Never reuse a cached conversation when the folded-in latest user
        // turn is empty (e.g. an assistant-prefill payload): sending empty
        // content with isNewConversation=false is an avoidable upstream error.
        if is_continuation && parsed.latest_user_content.is_empty() {
            is_continuation = false;
        }

        let conversation_id = conversation.conversation_id;
        let session_url = format!("https://www.meta.ai/c/{conversation_id}");

        let mut prompt = if is_continuation {
            parsed.latest_user_content
        } else {
            parsed.folded_prompt
        };
        let tools = request.tools.clone().unwrap_or_default();
        let request_had_tools = !tools.is_empty();
        if request_had_tools {
            prompt = inject_tool_prompt(&prompt, &tools, request.tool_choice.as_ref());
        }

        Ok((prompt, conversation_id, session_url, !is_continuation, request_had_tools))
    }

    /// GraphQL warmup + mode switch for a conversation.
    async fn warmup_and_mode_switch(
        &self,
        conversation_id: &str,
        thinking: Option<bool>,
    ) -> Result<(), GatewayError> {
        self.graphql_post(
            DOC_WARMUP,
            serde_json::json!({ "conversationId": conversation_id }),
            "Warmup",
        )
        .await?;
        let mode = effective_mode(&self.model_id, thinking);
        self.graphql_post(
            DOC_MODE_SWITCH,
            serde_json::json!({
                "input": { "conversationId": conversation_id, "mode": mode },
            }),
            "Mode switch",
        )
        .await
    }

    pub async fn chat(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, GatewayError> {
        let (prompt, conversation_id, session_url, is_new, request_had_tools) =
            self.prepare_turn(&request)?;
        let template_b64 = if is_new { HOME_TEMPLATE_B64 } else { CHAT_TEMPLATE_B64 };

        self.warmup_and_mode_switch(&conversation_id, request.thinking).await?;

        let acc = ws_chat(
            &prompt,
            &conversation_id,
            &self.authorization,
            &self.cookie_header,
            template_b64,
        )
        .await?;

        let content = acc.content;
        if content.is_empty() && acc.deltas.is_empty() {
            return Err(GatewayError::Provider(
                "Meta AI returned no assistant content".to_string(),
            ));
        }

        let (clean_content, tool_calls) = convert_xml_tool_calls(&content, request_had_tools);
        let finish_reason = if tool_calls.is_some() {
            "tool_calls"
        } else {
            "stop"
        };

        Ok(ChatCompletionResponse {
            id: format!("chatcmpl-meta-{}", &new_uuid()[..12]),
            object: "chat.completion".to_string(),
            created: current_timestamp(),
            model: request.model.clone(),
            choices: vec![ChatCompletionChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: ChatContent::String(clean_content),
                    name: None,
                    reasoning_content: None,
                    citations: None,
                    tool_calls,
                    tool_call_id: None,
                },
                finish_reason: finish_reason.to_string(),
            }],
            usage: self.estimate_cost(&prompt, &content),
            session_url: Some(session_url),
        })
    }

    pub async fn chat_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, GatewayError>>, GatewayError> {
        let (prompt, conversation_id, session_url, is_new, request_had_tools) =
            self.prepare_turn(&request)?;
        let template_b64 = if is_new { HOME_TEMPLATE_B64 } else { CHAT_TEMPLATE_B64 };

        self.warmup_and_mode_switch(&conversation_id, request.thinking).await?;

        let (tx, rx) = mpsc::unbounded_channel();
        let rx_stream = UnboundedReceiverStream::new(rx);

        let authorization = self.authorization.clone();
        let cookie_header = self.cookie_header.clone();
        let ctx = StreamEmitCtx {
            tx: tx.clone(),
            id: format!("chatcmpl-meta-{}", &new_uuid()[..12]),
            created: current_timestamp(),
            model: request.model.clone(),
            session_url,
            request_had_tools,
            sent_first_chunk: false,
            stripper: XmlToolCallStripper::new(),
            collected_calls: Vec::new(),
        };

        tokio::spawn(async move {
            let mut ctx = ctx;
            match ws_chat_emit(
                &prompt,
                &conversation_id,
                &authorization,
                &cookie_header,
                template_b64,
                &mut ctx,
            )
            .await
            {
                Ok(()) => {
                    if let Some(tool_call) = ctx.stripper.finish_pending() {
                        ctx.collected_calls.push(tool_call);
                    }
                    if !ctx.sent_first_chunk && ctx.collected_calls.is_empty() {
                        // Empty upstream response: treat as a failure, not a
                        // successful empty completion (matches the non-streaming
                        // path and the web client).
                        ctx.emit_error(GatewayError::Provider(
                            "Meta AI returned no assistant content".to_string(),
                        ));
                        return;
                    }
                    if !ctx.collected_calls.is_empty() {
                        let _ = ctx.tx.send(Ok(ChatCompletionChunk {
                            id: ctx.id.clone(),
                            object: "chat.completion.chunk".to_string(),
                            created: ctx.created,
                            model: ctx.model.clone(),
                            choices: vec![ChunkChoice {
                                index: 0,
                                delta: ChatMessageDelta {
                                    role: None,
                                    content: None,
                                    reasoning_content: None,
                                    citations: None,
                                    tool_calls: Some(ctx.collected_calls.clone()),
                                },
                                finish_reason: None,
                            }],
                            session_url: Some(ctx.session_url.clone()),
                        }));
                    }
                    let finish = if ctx.collected_calls.is_empty() {
                        "stop"
                    } else {
                        "tool_calls"
                    };
                    ctx.emit_finish(finish);
                }
                Err(e) => {
                    ctx.emit_error(e);
                }
            }
        });

        Ok(rx_stream.boxed())
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;

    use super::*;

    fn message(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: ChatContent::String(content.to_string()),
            name: None,
            reasoning_content: None,
            citations: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    // ─── id helpers ─────────────────────────────────────────────────────────

    #[test]
    fn conversation_id_has_expected_shape() {
        let id = generate_conversation_id();
        assert!(id.starts_with("c."), "got {id}");
        assert_eq!(id.len(), 21, "c. + 19 base62 chars, got {id}");
        let payload = &id[2..];
        assert!(
            payload.bytes().all(|b| BASE62_ALPHABET.contains(&b)),
            "non-base62 char in {id}"
        );
    }

    #[test]
    fn base62_encode_pads_and_roundtrips() {
        let encoded = encode_base62(12_345_678_901_234_567u128, 19);
        assert_eq!(encoded.len(), 19);
        assert_eq!(encode_base62(0, 4), "0000");
        let alphabet_chars: String = BASE62_ALPHABET.iter().map(|&b| b as char).collect();
        assert!(
            encoded.chars().all(|c| alphabet_chars.contains(c)),
            "non-base62 char in {encoded}"
        );
    }

    #[test]
    fn unique_message_id_is_parseable_and_matches_reference_shape() {
        let submitted = 1_700_000_000_000_i64;
        let id = generate_unique_message_id(submitted);
        let s = id.to_string();
        assert!(s.starts_with("1700000000000"), "got {s}");
        // exactly 4-digit random suffix appended to the ms timestamp
        assert_eq!(s.len(), "1700000000000".len() + 4, "got {s}");
    }

    #[test]
    fn mode_for_model_maps_known_and_unknown() {
        assert_eq!(mode_for_model("muse-spark"), "think_fast");
        assert_eq!(mode_for_model("muse-spark-thinking"), "think_hard");
        assert_eq!(mode_for_model("muse-spark-contemplating"), "think_hard");
        assert_eq!(mode_for_model("totally-unknown"), "think_fast");
    }

    #[test]
    fn effective_mode_honors_thinking_override() {
        // Per-request thinking toggle wins over the model default.
        assert_eq!(effective_mode("muse-spark", Some(true)), "think_hard");
        assert_eq!(effective_mode("muse-spark-thinking", Some(false)), "think_fast");
        assert_eq!(effective_mode("muse-spark-contemplating", Some(false)), "think_fast");
        // No toggle: fall back to the model default.
        assert_eq!(effective_mode("muse-spark", None), "think_fast");
        assert_eq!(effective_mode("muse-spark-thinking", None), "think_hard");
        assert_eq!(effective_mode("muse-spark-contemplating", None), "think_hard");
    }

    // ─── proto template mutation ────────────────────────────────────────────

    #[test]
    fn proto_roundtrip_is_byte_stable() {
        use base64::Engine;
        for template in [HOME_TEMPLATE_B64, CHAT_TEMPLATE_B64] {
            let raw = base64::engine::general_purpose::STANDARD.decode(template).unwrap();
            let fields = parse_proto_fields(&raw).expect("template parses");
            let re_serialized = serialize_proto_fields(&fields);
            assert_eq!(re_serialized, raw, "proto roundtrip must be byte-stable");
        }
    }

    #[test]
    fn prompt_frame_mutates_all_expected_paths() {
        use base64::Engine;
        let conversation_id = generate_conversation_id();
        let request_id = new_uuid();
        let user_message_id = new_uuid();
        let submitted_ms = 1_700_000_000_123i64;
        let unique_message_id = 42i64;
        let prompt = "hello from the gateway";

        let frame = build_ws_prompt_frame(
            prompt,
            &conversation_id,
            HOME_TEMPLATE_B64,
            &request_id,
            &user_message_id,
            submitted_ms,
            unique_message_id,
        )
        .unwrap();

        // Frame layout: 6-byte header + [0x00, 0x80] + JSON body.
        assert_eq!(frame[0], WS_PROMPT_FRAME_TYPE);
        let body_len = (frame[3] as usize)
            | ((frame[4] as usize) << 8)
            | ((frame[5] as usize) << 16);
        assert_eq!(body_len, frame.len() - 6);
        assert_eq!(frame[6], 0);
        assert_eq!(frame[7], WS_PROMPT_FRAME_FLAG);

        let outer: serde_json::Value =
            serde_json::from_slice(&frame[8..]).expect("body is JSON");
        assert_eq!(outer["req-id"], request_id);
        let payload = outer["payload"].as_str().expect("payload is a string");
        let raw = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .expect("payload is base64");
        let fields = parse_proto_fields(&raw).expect("payload is protobuf");

        // [1,1,5] conversation id
        let field_1 = fields.iter().find(|f| f.number == 1).unwrap();
        let ProtoValue::Bytes(field_1_bytes) = &field_1.value else {
            panic!("field 1 is not length-delimited");
        };
        let nested_1 = parse_proto_fields(field_1_bytes).unwrap();
        let field_1_1 = nested_1.iter().find(|f| f.number == 1).unwrap();
        let ProtoValue::Bytes(field_1_1_bytes) = &field_1_1.value else {
            panic!("field [1,1] is not length-delimited");
        };
        let nested_1_1 = parse_proto_fields(field_1_1_bytes).unwrap();
        let field_5 = nested_1_1.iter().find(|f| f.number == 5).unwrap();
        let ProtoValue::Bytes(field_5_bytes) = &field_5.value else {
            panic!("field [1,1,5] is not length-delimited");
        };
        assert_eq!(String::from_utf8_lossy(field_5_bytes), conversation_id);

        // [1,6] request id
        let field_6 = nested_1.iter().find(|f| f.number == 6).unwrap();
        let ProtoValue::Bytes(field_6_bytes) = &field_6.value else {
            panic!("field [1,6] is not length-delimited");
        };
        assert_eq!(String::from_utf8_lossy(field_6_bytes), request_id);

        // [2,2] prompt text
        let field_2 = fields.iter().find(|f| f.number == 2).unwrap();
        let ProtoValue::Bytes(field_2_bytes) = &field_2.value else {
            panic!("field 2 is not length-delimited");
        };
        let nested_2 = parse_proto_fields(field_2_bytes).unwrap();
        let field_2_2 = nested_2.iter().find(|f| f.number == 2).unwrap();
        let ProtoValue::Bytes(prompt_bytes) = &field_2_2.value else {
            panic!("field [2,2] is not length-delimited");
        };
        assert_eq!(String::from_utf8_lossy(prompt_bytes), prompt);

        // [2,1,2] timestamp varints
        let field_2_1 = nested_2.iter().find(|f| f.number == 1).unwrap();
        let ProtoValue::Bytes(field_2_1_bytes) = &field_2_1.value else {
            panic!("field [2,1] is not length-delimited");
        };
        let nested_2_1 = parse_proto_fields(field_2_1_bytes).unwrap();
        let field_2_1_2 = nested_2_1.iter().find(|f| f.number == 2).unwrap();
        let ProtoValue::Bytes(field_2_1_2_bytes) = &field_2_1_2.value else {
            panic!("field [2,1,2] is not length-delimited");
        };
        let nested_212 = parse_proto_fields(field_2_1_2_bytes).unwrap();
        for f in &nested_212 {
            match f.number {
                1 => {
                    let ProtoValue::Bytes(b) = &f.value else {
                        panic!("[2,1,2].1 not bytes");
                    };
                    assert_eq!(String::from_utf8_lossy(b), conversation_id);
                }
                2 => {
                    let ProtoValue::Varint(v) = f.value else {
                        panic!("[2,1,2].2 not varint");
                    };
                    assert_eq!(v, submitted_ms as u64);
                }
                3 => {
                    let ProtoValue::Varint(v) = f.value else {
                        panic!("[2,1,2].3 not varint");
                    };
                    assert_eq!(v, unique_message_id as u64);
                }
                _ => {}
            }
        }
    }

    #[test]
    fn intro_frame_has_expected_layout() {
        let frame = build_ws_intro_frame("c.test123");
        assert_eq!(frame[0], WS_INTRO_FRAME_TYPE);
        assert_eq!(frame[1], 0);
        assert_eq!(frame[2], 0);
        let body_len = (frame[3] as usize)
            | ((frame[4] as usize) << 8)
            | ((frame[5] as usize) << 16);
        assert_eq!(body_len, frame.len() - 6);
        let body: serde_json::Value = serde_json::from_slice(&frame[6..]).unwrap();
        assert_eq!(body["x-dgw-app-x-ecto-conversation-id"], "c.test123");
        assert_eq!(body["x-dgw-app-client-payload-type"], "PROTO_INSIDE_JSON");
    }

    #[test]
    fn ws_url_includes_handshake_params() {
        let url = build_ws_url("ecto1:abc123", "req-123");
        assert!(url.starts_with("wss://gateway.meta.ai/ws/clippy?"));
        assert!(url.contains("x-dgw-appid=1522763855472543"));
        assert!(url.contains("x-dgw-version=5"));
        assert!(url.contains("x-dgw-authtype=15%3A0") || url.contains("x-dgw-authtype=15:0"));
        assert!(url.contains("Authorization=ecto1%3Aabc123") || url.contains("Authorization=ecto1:abc123"));
        assert!(url.contains("x-dgw-app-clippy-request-id=req-123"));
        assert!(url.contains("x-dgw-app-clippy-async=true"));
    }

    // ─── event parsing ──────────────────────────────────────────────────────

    #[test]
    fn parser_handles_concatenated_json_events() {
        let payload = format!(
            r#"{{"type":"full","response":{{"sections":[{{"view_model":{{"primitive":{{"text":"Hi"}}}}}}]}}}}{{"type":"patch","operations":[{{"op":"delta","path":"/sections/0/view_model/primitive/text","value":" there"}}]}}"#
        );
        let events = parse_ws_response_events(&payload);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "full");
        assert_eq!(events[1].event_type, "patch");
    }

    #[test]
    fn parser_ignores_non_object_garbage() {
        assert!(parse_ws_response_events("null").is_empty());
        assert!(parse_ws_response_events("[1,2,3]").is_empty());
        assert!(parse_ws_response_events("").is_empty());
    }

    #[test]
    fn parser_skips_unbalanced_brackets() {
        let payload = r#"{"type":"full","response":{"sections":[]}"#;
        assert!(parse_ws_response_events(payload).is_empty());
    }

    #[test]
    fn accumulator_full_event_tracks_growth() {
        let mut acc = WsAccumulator::default();
        acc.apply(r#"{"type":"full","response":{"sections":[{"view_model":{"primitive":{"text":"Hel"}}}]}}"#);
        acc.apply(r#"{"type":"full","response":{"sections":[{"view_model":{"primitive":{"text":"Hello"}}}]}}"#);
        assert_eq!(acc.content, "Hello");
        assert_eq!(acc.deltas, vec!["Hel", "lo"]);
    }

    #[test]
    fn accumulator_patch_event_appends() {
        let mut acc = WsAccumulator::default();
        acc.apply(r#"{"type":"full","response":{"sections":[{"view_model":{"primitive":{"text":"Hi"}}}]}}"#);
        acc.apply(r#"{"type":"patch","operations":[{"op":"delta","path":"/sections/0/view_model/primitive/text","value":" there"}]}"#);
        assert_eq!(acc.content, "Hi there");
        assert_eq!(acc.deltas, vec!["Hi", " there"]);
    }

    #[test]
    fn accumulator_ignores_unrelated_patch_paths() {
        let mut acc = WsAccumulator::default();
        acc.apply(
            r#"{"type":"patch","operations":[{"op":"delta","path":"/sections/0/view_model/primitive/other","value":"nope"}]}"#,
        );
        assert!(acc.content.is_empty());
        assert!(acc.deltas.is_empty());
    }

    // ─── message folding ────────────────────────────────────────────────────

    #[test]
    fn history_folds_roles_and_keeps_last_user_bare() {
        let messages = vec![
            message("system", "Be concise"),
            message("user", "Hello"),
            message("assistant", "Hi there"),
            message("user", "Tell me about Rust"),
        ];
        let parsed = parse_openai_messages(&messages);
        assert_eq!(
            parsed.folded_prompt,
            "system: Be concise\n\nuser: Hello\n\nassistant: Hi there\n\nTell me about Rust"
        );
        assert_eq!(parsed.latest_user_content, "Tell me about Rust");
    }

    #[test]
    fn history_skips_empty_content() {
        let messages = vec![
            message("user", "  "),
            message("user", "real content"),
            message("assistant", ""),
        ];
        let parsed = parse_openai_messages(&messages);
        assert_eq!(parsed.folded_prompt, "real content");
        assert_eq!(parsed.latest_user_content, "real content");
    }

    #[test]
    fn history_empty_returns_default() {
        let parsed = parse_openai_messages(&[]);
        assert!(parsed.folded_prompt.is_empty());
        assert!(parsed.latest_user_content.is_empty());
    }

    #[test]
    fn history_normalizes_developer_role() {
        let messages = vec![message("developer", "sys"), message("user", "hi")];
        let parsed = parse_openai_messages(&messages);
        assert_eq!(parsed.folded_prompt, "system: sys\n\nhi");
    }

    #[test]
    fn history_uses_full_fold_when_no_user_turn() {
        let messages = vec![message("assistant", "prefill"), message("assistant", "more")];
        let parsed = parse_openai_messages(&messages);
        assert_eq!(
            parsed.folded_prompt,
            "assistant: prefill\n\nassistant: more"
        );
        assert!(parsed.latest_user_content.is_empty());
    }

    // ─── traversal safety ───────────────────────────────────────────────────

    #[test]
    fn traverse_missing_path_returns_false() {
        let mut fields = parse_proto_fields(
            &base64::engine::general_purpose::STANDARD
                .decode(HOME_TEMPLATE_B64)
                .unwrap(),
        )
        .unwrap();
        let mut mutator = |raw: &mut Vec<u8>| raw.extend_from_slice(b"x");
        assert!(!traverse_and_mutate(&mut fields, &[99, 1], &mut mutator));
    }

    #[test]
    fn build_ws_prompt_frame_rejects_bad_template() {
        let result = build_ws_prompt_frame(
            "hi",
            "c.test123",
            "!!!not-base64!!!",
            "req",
            "user",
            1,
            2,
        );
        assert!(result.is_err());
    }
}

