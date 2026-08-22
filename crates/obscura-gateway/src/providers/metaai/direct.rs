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
//! Tool calling uses the platform's native protocol: the DGW model invokes
//! server-side tools (web search, weather) by emitting a structured block
//! (`[<id>] <query> (search-results://query?query=<query>)` plus a line dump)
//! into a bare `{"text": ...}` event, delivered separately from the visible
//! section stream. Those blocks are collected and converted to OpenAI
//! `tool_calls`; the prompt-injected `<tool_call>` XML markers from
//! [`crate::providers::tool_call`] remain as a secondary fallback only.

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
use crate::session::SessionHandle;

use super::auth::{build_cookie_header, extract_ecto_token, extract_meta_cookies, validate_meta_session};
use super::state::MetaAiSessionStore;
use super::templates::{CHAT_TEMPLATE_B64, HOME_TEMPLATE_B64};

/// GraphQL endpoint used for warmup and mode switch.
const GRAPHQL_API_URL: &str = "https://www.meta.ai/api/graphql";
/// DGW WebSocket endpoint.
const WS_URL: &str = "wss://gateway.meta.ai/ws/clippy";
/// Resumable upload endpoint for image attachments (captured from the web
/// client). Each upload uses a fresh uuid path segment and returns a media id
/// that is embedded in the DGW prompt proto at `.2.3.1.1`.
const RUPLOAD_URL: &str = "https://rupload.meta.ai/gen_ai_document_gen_ai_tenant";
/// Upload handler the rupload endpoint requires (captured from the web
/// client's fetch headers).
const RUPLOAD_HANDLER: &str = "genai_document";

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

/// Insert a length-delimited field into a nested message, replacing it if it
/// already exists. The web client serializes submessages with fields in
/// ascending number order, so insert at the matching position to keep the
/// wire bytes identical to a real attachment turn.
fn insert_nested_field(raw: &mut Vec<u8>, number: u32, value: &[u8]) {
    if let Some(mut nested) = parse_proto_fields(raw) {
        if let Some(field) = nested.iter_mut().find(|f| f.number == number) {
            field.value = ProtoValue::Bytes(value.to_vec());
        } else {
            let pos = nested
                .iter()
                .position(|f| f.number > number)
                .unwrap_or(nested.len());
            nested.insert(
                pos,
                ProtoField {
                    number,
                    wire_type: 2,
                    value: ProtoValue::Bytes(value.to_vec()),
                },
            );
        }
        *raw = serialize_proto_fields(&nested);
    }
}

/// Build the DGW attachment submessage that lands at prompt-field `[2,3]`
/// (`.2.3`), matching the captured web-client wire format:
///
/// ```text
/// .3.1 msg { .1 varint = media_id }
/// .3.2 varint = 1
/// .3.3 str = ""
/// .3.5 varint = 0
/// .3.6 str = <mime>
/// .3.7 str = <filename>
/// ```
fn build_attachment_block(media_id: u64, mime: &str, filename: &str) -> Vec<u8> {
    let mut block = Vec::new();

    let mut media_id_msg = Vec::new();
    media_id_msg.extend_from_slice(&encode_varint((1 << 3) | 0));
    media_id_msg.extend_from_slice(&encode_varint(media_id));
    block.extend_from_slice(&encode_varint((1 << 3) | 2));
    block.extend_from_slice(&encode_varint(media_id_msg.len() as u64));
    block.extend_from_slice(&media_id_msg);

    block.extend_from_slice(&encode_varint((2 << 3) | 0));
    block.push(1);

    block.extend_from_slice(&encode_varint((3 << 3) | 2));
    block.push(0);

    block.extend_from_slice(&encode_varint((5 << 3) | 0));
    block.push(0);

    block.extend_from_slice(&encode_varint((6 << 3) | 2));
    block.extend_from_slice(&encode_varint(mime.len() as u64));
    block.extend_from_slice(mime.as_bytes());

    block.extend_from_slice(&encode_varint((7 << 3) | 2));
    block.extend_from_slice(&encode_varint(filename.len() as u64));
    block.extend_from_slice(filename.as_bytes());

    block
}

/// Split a `data:` URL into `(mime, payload, is_base64)`. Returns None for
/// anything that does not look like a data URL.
fn parse_data_url(url: &str) -> Option<(String, String, bool)> {
    let rest = url.strip_prefix("data:")?;
    let (head, data) = rest.split_once(',')?;
    let is_base64 = head.ends_with(";base64");
    let mime = head
        .trim_end_matches(";base64")
        .split(';')
        .next()
        .unwrap_or("")
        .to_string();
    let mime = if mime.is_empty() {
        "application/octet-stream".to_string()
    } else {
        mime
    };
    Some((mime, data.to_string(), is_base64))
}

/// Percent-decode a non-base64 data URL payload.
fn percent_decode(encoded: &str) -> Vec<u8> {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(((hi << 4) | lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Derive a filename from a mime type (data URLs carry no name).
fn attachment_filename(mime: &str) -> String {
    let ext = match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/heic" | "image/heif" => "heic",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        _ => "bin",
    };
    format!("attachment.{ext}")
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
/// An uploaded image attachment is injected at `[2,3]` when present.
#[allow(clippy::too_many_arguments)]
fn build_ws_prompt_frame(
    prompt: &str,
    conversation_id: &str,
    template_b64: &str,
    request_id: &str,
    user_message_id: &str,
    submitted_ms: i64,
    unique_message_id: i64,
    attachment_block: Option<&[u8]>,
) -> Result<Vec<u8>, GatewayError> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(template_b64)
        .map_err(|e| GatewayError::Internal(format!("meta.ai template decode failed: {e}")))?;
    let mut proto_fields = parse_proto_fields(&raw).ok_or_else(|| {
        GatewayError::Internal("meta.ai template is not valid protobuf".to_string())
    })?;

    // [1,1,5,5,1] conversationId (wrapped in a nested submessage)
    let conv = conversation_id.as_bytes();
    let mut set_conv_at_115 = |raw: &mut Vec<u8>| set_nested_bytes(raw, 1, conv);
    if !traverse_and_mutate(&mut proto_fields, &[1, 1, 5, 5], &mut set_conv_at_115) {
        return Err(GatewayError::Internal(
            "meta.ai template: missing [1,1,5,5] for conversation id".to_string(),
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
    // [2,3] attachment (rupload media id) when an image is attached
    if let Some(block) = attachment_block {
        let mut insert_attach_at_2 = |raw: &mut Vec<u8>| insert_nested_field(raw, 3, block);
        if !traverse_and_mutate(&mut proto_fields, &[2], &mut insert_attach_at_2) {
            return Err(GatewayError::Internal(
                "meta.ai template: missing [2] for attachment".to_string(),
            ));
        }
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
        ])
        .finish();
    format!("{WS_URL}?{query}")
}

// ─── WS response parser ──────────────────────────────────────────────────────

/// A single event inside a DGW WS message.
#[derive(Debug, Clone, serde::Deserialize)]
struct WsResponseEvent {
    #[serde(rename = "type")]
    #[serde(default)]
    event_type: String,
    #[serde(default)]
    response: Option<WsResponseBody>,
    #[serde(default)]
    operations: Option<Vec<WsPatchOperation>>,
    /// Terminal completion marker. The DGW ends a turn with a `full`-shaped
    /// event that carries the final `response_id` at the TOP level (no `seq`,
    /// no `type`), where the thinking section reports `is_in_progress: false`.
    #[serde(default)]
    response_id: Option<String>,
    /// Top-level `sections` (the terminal frame carries them directly, not
    /// under a `response` key like streaming `full` events).
    #[serde(default)]
    sections: Vec<WsSection>,
    /// Bare `{"text": ...}` events. The DGW delivers native tool results in a
    /// separate `web_search` sub-message (not the visible section stream): the
    /// model emits the invocation header plus the fetched line dump here, e.g.
    /// `[<id>] <query> (search-results://query?query=<query>)` followed by the
    /// `**viewing lines**` header and `L0:`..`L<N>:` dump.
    #[serde(default)]
    text: Option<String>,
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
    /// Present on the `GenAIBotThinkingStatusPrimitive` section. `false` on
    /// the terminal frame signals the turn is fully streamed.
    #[serde(rename = "is_in_progress")]
    #[serde(default)]
    is_in_progress: Option<bool>,
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

impl WsResponseEvent {
    fn sections(&self) -> &[WsSection] {
        self.response
            .as_ref()
            .map(|b| b.sections.as_slice())
            .unwrap_or(&self.sections)
    }
}

/// Accumulator that reassembles `full`/`patch` events into content deltas.
#[derive(Debug, Default)]
struct WsAccumulator {
    content: String,
    deltas: Vec<String>,
    /// Native tool-result blocks (`search-results://` invocation headers plus
    /// the fetched line dump), collected from bare `{"text": ...}` events and
    /// deduped. These arrive in a separate DGW channel from the visible
    /// section stream, so they never appear in `content`.
    tool_blocks: Vec<String>,
    /// Set when the terminal DGW frame (top-level `response_id`, thinking
    /// `is_in_progress: false`) arrives, signalling the turn is complete.
    done: bool,
}

impl WsAccumulator {
    fn apply(&mut self, raw: &str) {
        for event in parse_ws_response_events(raw) {
            // Terminal completion frame: the DGW ends a turn with a `full`-
            // shaped event carrying the final `response_id` at the top level
            // and no `seq`/`type`. The thinking section reports
            // `is_in_progress: false`.
            if event.response_id.is_some() && event.event_type.is_empty() {
                for section in event.sections() {
                    if section
                        .view_model
                        .as_ref()
                        .and_then(|vm| vm.primitive.as_ref())
                        .and_then(|p| p.is_in_progress)
                        == Some(false)
                        || section
                            .view_model
                            .as_ref()
                            .and_then(|vm| vm.primitive.as_ref())
                            .and_then(|p| p.text.as_ref())
                            .map(|t| !t.is_empty())
                            .unwrap_or(false)
                    {
                        self.done = true;
                    }
                }
            }
            if event.event_type == "full" || (event.response_id.is_some() && event.event_type.is_empty())
            {
                for section in event.sections() {
                    let text = section
                        .view_model
                        .as_ref()
                        .and_then(|vm| vm.primitive.as_ref())
                        .and_then(|p| p.text.clone())
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
            // Native tool results come through as bare `{"text": ...}` events
            // (the `web_search` sub-message inside the DGW protobuf), separate
            // from the visible section stream. Collect the invocation blocks,
            // deduped; callers turn them into OpenAI `tool_calls`.
            if let Some(tool_text) = &event.text {
                if tool_text.contains("search-results://query?query=")
                    && !self.tool_blocks.iter().any(|b| b == tool_text)
                {
                    self.tool_blocks.push(tool_text.clone());
                }
            }
        }
    }
}

// ─── Native tool-call extraction ─────────────────────────────────────────────
//
// The DGW model invokes server-side tools (web search, weather) by emitting a
// structured block into the markdown stream:
//
//   [<tool_id>] <query> (search-results://query?query=<query>)
//   **viewing lines [0 - 89] of 89**
//
//   L0: # Search Results
//   L1: ...
//
// This is the platform's native tool protocol (no `<tool_call>` prompt
// injection). We detect these blocks and convert them into OpenAI `tool_calls`,
// stripping the invocation header and the raw line-dump from the visible
// assistant content.

/// A native tool invocation parsed from the DGW text stream.
struct NativeToolCall {
    /// Server-side tool id (used as the OpenAI tool_call id).
    tool_id: String,
    /// Tool name to report. `web_search` for search-results invocations.
    name: String,
    /// Query string argument.
    query: String,
}

/// Detect the first `search-results://query?query=<q>)` invocation block header
/// at/after `from`, returning the parsed invocation and the byte offset just
/// past the whole block (header + `**viewing lines**` + line dump).
fn parse_native_tool_block(text: &str, from: usize) -> Option<(NativeToolCall, usize)> {
    let marker = "search-results://query?query=";
    let start = text[from..].find(marker)? + from;
    // The invocation header begins before the marker: `[<id>] <query> (`.
    // Recover it by scanning backward for `[`.
    let head_start = text[..start].rfind('[')?;
    let close_paren = text[start..].find(')')? + start;
    let query = &text[start + marker.len()..close_paren];
    if query.is_empty() {
        return None;
    }
    let head = &text[head_start..start];
    let tool_id = head
        .trim_start_matches('[')
        .split([']', ' '])
        .next()
        .unwrap_or_default()
        .to_string();
    // Skip the `**viewing lines [0 - N] of M**` line and the following blank
    // line plus the `L*:` dump that carries the fetched results.
    let mut end = close_paren + 1;
    while end < text.len() && !text[end..].starts_with('\n') {
        end += 1;
    }
    if end < text.len() {
        end += 1;
    }
    // Consume the `**viewing lines ...**` line (and blank) if present.
    if text[end..].trim_start().starts_with("**viewing lines") {
        let nl = text[end..].find('\n').map(|p| end + p + 1).unwrap_or(text.len());
        end = nl;
        if end < text.len() && text[end..].starts_with('\n') {
            end += 1;
        }
    }
    // Consume the `L0:`..`L<N>:` result dump.
    loop {
        let line_end = text[end..].find('\n').map(|p| end + p).unwrap_or(text.len());
        let line = text[end..line_end].trim_end();
        if line.starts_with("L") && line[1..].chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
        {
            if line_end == text.len() {
                end = line_end;
                break;
            }
            end = line_end + 1;
            continue;
        }
        break;
    }
    let call = NativeToolCall {
        tool_id,
        name: "web_search".to_string(),
        query: query.to_string(),
    };
    Some((call, end))
}

/// Convert a `NativeToolCall` into an OpenAI `ToolCall`.
fn native_call_to_openai(call: &NativeToolCall) -> ToolCall {
    let arguments = serde_json::json!({ "query": call.query }).to_string();
    ToolCall {
        id: if call.tool_id.is_empty() {
            crate::providers::tool_call::stable_call_id(&call.name, &arguments)
        } else {
            format!("call_{}", call.tool_id)
        },
        r#type: "function".to_string(),
        function: crate::models::FunctionCall {
            name: call.name.clone(),
            arguments,
        },
    }
}

/// Extract all native `search-results://` tool invocations from a full turn's
/// text. Returns `(cleaned_content, tool_calls)` where the cleaned content has
/// every invocation header and result-dump removed.
fn extract_native_tool_calls(content: &str) -> (String, Vec<ToolCall>) {
    let mut clean = String::new();
    let mut calls = Vec::new();
    let mut from = 0usize;
    loop {
        match parse_native_tool_block(content, from) {
            Some((call, end)) => {
                // The whole block (header + viewing-lines + result dump) is
                // tool machinery, not user-facing answer text: strip it all.
                calls.push(native_call_to_openai(&call));
                from = end;
            }
            None => {
                clean.push_str(&content[from..]);
                break;
            }
        }
    }
    (clean, calls)
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
    attachment_block: Option<&[u8]>,
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
        attachment_block,
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
    attachment_block: Option<&[u8]>,
) -> Result<WsAccumulator, GatewayError> {
    let mut ws = ws_connect_and_send(
        prompt,
        conversation_id,
        authorization,
        cookie_header,
        template_b64,
        attachment_block,
    )
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
                if !keep_going? || acc.done {
                    break;
                }
            }
        }
    }
    Ok(acc)
}

// ─── Streaming emit context ──────────────────────────────────────────────────

/// Streaming state that strips MTP tool blocks and emits OpenAI chunks.
struct StreamEmitCtx {
    tx: mpsc::UnboundedSender<Result<ChatCompletionChunk, GatewayError>>,
    id: String,
    created: i64,
    model: String,
    session_url: String,
    tool_defs: Vec<crate::models::Tool>,
    sent_first_chunk: bool,
    mtp_state: crate::providers::mtp::MtpStreamState,
    collected_calls: Vec<ToolCall>,
}

impl StreamEmitCtx {
    fn emit_delta(&mut self, delta: &str) {
        let clean = if !self.tool_defs.is_empty() {
            let clean = self.mtp_state.process_delta(delta, &self.tool_defs);
            if !self.mtp_state.collected_tool_calls.is_empty() {
                for call in self.mtp_state.collected_tool_calls.drain(..) {
                    if !self.collected_calls.iter().any(|c| c.id == call.id) {
                        self.collected_calls.push(call);
                    }
                }
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

    /// Push a tool call, deduped by id (the same native block is re-emitted
    /// across many DGW frames and both the content and tool channels can
    /// surface it).
    fn push_unique_call(&mut self, call: ToolCall) {
        if !self.collected_calls.iter().any(|c| c.id == call.id) {
            self.collected_calls.push(call);
        }
    }
}

/// Run one full DGW turn, emitting content deltas as they arrive.
async fn ws_chat_emit(
    prompt: &str,
    conversation_id: &str,
    authorization: &str,
    cookie_header: &str,
    template_b64: &str,
    attachment_block: Option<&[u8]>,
    ctx: &mut StreamEmitCtx,
) -> Result<(), GatewayError> {
    let mut ws = ws_connect_and_send(
        prompt,
        conversation_id,
        authorization,
        cookie_header,
        template_b64,
        attachment_block,
    )
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
                for block in std::mem::take(&mut acc.tool_blocks) {
                    let (_, calls) = extract_native_tool_calls(&block);
                    for call in calls {
                        ctx.push_unique_call(call);
                    }
                }
                if acc.done {
                    break;
                }
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

    /// Upload a file to the resumable rupload endpoint, mirroring the web
    /// client's two-step flow: a GET that announces the upload and returns
    /// the current offset, then a POST carrying the bytes. Returns the media
    /// id used by the DGW prompt proto attachment block.
    async fn upload_file(
        &self,
        data: &[u8],
        filename: &str,
        mime: &str,
    ) -> Result<u64, GatewayError> {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::engine::Engine;
        use sha2::Digest;

        let mut hasher = sha2::Sha256::new();
        hasher.update(data);
        let digest_b64 = B64.encode(hasher.finalize());
        let upload_id = new_uuid();
        let url = format!("{RUPLOAD_URL}/{upload_id}");

        let auth_value = format!("OAuth {}", self.authorization);
        let entity_len = data.len().to_string();
        let prepare = self
            .http
            .get(&url)
            .header("Authorization", &auth_value)
            .header("desired_upload_handler", RUPLOAD_HANDLER)
            .header("ecto_auth_token", "true")
            .header("is_abra_user", "true")
            .header("x-entity-digest", format!("sha256 {digest_b64}"))
            .header("x-entity-length", &entity_len);
        let prepare = crate::providers::send_with_retry(prepare).await?;
        let prepare_status = prepare.status().as_u16();
        let prepare_text = prepare.text().await.unwrap_or_default();
        if prepare_status != 200 {
            return Err(GatewayError::Provider(format!(
                "Meta AI upload prepare failed: HTTP {prepare_status}: {prepare_text}"
            )));
        }

        let upload = self
            .http
            .post(&url)
            .header("Authorization", &auth_value)
            .header("desired_upload_handler", RUPLOAD_HANDLER)
            .header("ecto_auth_token", "true")
            .header("is_abra_user", "true")
            .header("offset", "0")
            .header("x-entity-length", &entity_len)
            .header("x-entity-name", filename)
            .header("x-entity-type", mime)
            .body(data.to_vec());
        let upload = upload.send().await.map_err(|e| {
            GatewayError::Provider(format!("Meta AI upload failed: {e}"))
        })?;
        let status = upload.status().as_u16();
        let text = upload.text().await.unwrap_or_default();
        if status != 200 {
            return Err(GatewayError::Provider(format!(
                "Meta AI upload failed: HTTP {status}: {text}"
            )));
        }
        let media_id = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|json| json.get("media_id").and_then(|v| v.as_str()).map(str::to_string))
            .ok_or_else(|| {
                GatewayError::Provider(format!("Meta AI upload returned no media_id: {text}"))
            })?;
        media_id.parse::<u64>().map_err(|_| {
            GatewayError::Provider(format!("Meta AI upload returned invalid media_id: {media_id}"))
        })
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
            prompt = format!(
                "{}\n\nUser request:\n{}",
                crate::providers::mtp::build_mtp_system_prompt(
                    &tools,
                    request.tool_choice.as_ref(),
                    false
                ),
                prompt
            );
        }

        Ok((prompt, conversation_id, session_url, !is_continuation, request_had_tools))
    }

    /// Extract the first image attachment from the request. The latest user
    /// message with an image wins, matching how the web client attaches the
    /// current turn's file. Returns `(bytes, mime, filename)`.
    async fn extract_attachment(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<Option<(Vec<u8>, String, String)>, GatewayError> {
        for message in request.messages.iter().rev() {
            let urls = message.content.image_urls();
            if let Some(url) = urls.first() {
                return self.resolve_attachment(url).await.map(Some);
            }
        }
        Ok(None)
    }

    /// Resolve an image attachment reference to its bytes, mime type, and a
    /// filename. Accepts `data:` URLs and remote `http(s)://` URLs; remote
    /// fetches go through the shared SSRF gate.
    async fn resolve_attachment(
        &self,
        url: &str,
    ) -> Result<(Vec<u8>, String, String), GatewayError> {
        use base64::engine::general_purpose::STANDARD as B64;

        if let Some((mime, encoded, is_base64)) = parse_data_url(url) {
            let bytes = if is_base64 {
                use base64::engine::Engine;
                B64.decode(&encoded).map_err(|e| {
                    GatewayError::BadRequest(format!("invalid base64 image data URL: {e}"))
                })?
            } else {
                percent_decode(&encoded)
            };
            let filename = attachment_filename(&mime);
            return Ok((bytes, mime, filename));
        }

        let parsed = url::Url::parse(url).map_err(|e| {
            GatewayError::BadRequest(format!("invalid image URL: {e}"))
        })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(GatewayError::BadRequest(format!(
                "unsupported image URL scheme (expected data: or http(s)://): {url}"
            )));
        }
        crate::providers::gemini::upload::validate_remote_url(&parsed)?;

        let response = self.http.get(url).send().await.map_err(|e| {
            GatewayError::BadRequest(format!("failed to fetch image URL {url}: {e}"))
        })?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(GatewayError::BadRequest(format!(
                "failed to fetch image URL {url}: HTTP {status}"
            )));
        }
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let bytes = response
            .bytes()
            .await
            .map_err(|e| GatewayError::BadRequest(format!("failed to read image URL {url}: {e}")))?
            .to_vec();
        Ok((bytes, mime.clone(), attachment_filename(&mime)))
    }

    /// Upload any request attachment and build the `.2.3` proto block.
    /// Returns None when the request carries no image.
    async fn upload_attachment_block(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<Option<Vec<u8>>, GatewayError> {
        let Some((bytes, mime, filename)) = self.extract_attachment(request).await? else {
            return Ok(None);
        };
        let media_id = self.upload_file(&bytes, &filename, &mime).await?;
        Ok(Some(build_attachment_block(media_id, &mime, &filename)))
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

        let attachment_block = self.upload_attachment_block(&request).await?;
        let acc = ws_chat(
            &prompt,
            &conversation_id,
            &self.authorization,
            &self.cookie_header,
            template_b64,
            attachment_block.as_deref(),
        )
        .await?;

        let content = acc.content;
        if content.is_empty() && acc.deltas.is_empty() {
            return Err(GatewayError::Provider(
                "Meta AI returned no assistant content".to_string(),
            ));
        }

        // Native DGW tool protocol first (`search-results://` blocks). The
        // tool results arrive as bare `{"text": ...}` events separate from the
        // section stream, so collect calls from those blocks as well as from
        // any block that leaked into the visible content. Then fall back to
        // XML markers for prompt-injected tool definitions.
        let (clean_content, mut native_calls) = extract_native_tool_calls(&content);
        for block in &acc.tool_blocks {
            let (_, calls) = extract_native_tool_calls(block);
            for call in calls {
                if !native_calls.iter().any(|c| c.id == call.id) {
                    native_calls.push(call);
                }
            }
        }
        // MTP blocks from prompt-injected tool definitions: absorbed and
        // validated against the client definitions, never leaked.
        let (clean_content, mtp_calls) = {
            if request_had_tools {
                let defs = request.tools.clone().unwrap_or_default();
                let mut st = crate::providers::mtp::MtpStreamState::new();
                let cleaned = st.process_delta(&clean_content, &defs);
                st.finish(&defs);
                (cleaned, std::mem::take(&mut st.collected_tool_calls))
            } else {
                (clean_content, Vec::new())
            }
        };
        let tool_calls = if !native_calls.is_empty() {
            Some(native_calls)
        } else if !mtp_calls.is_empty() {
            Some(mtp_calls)
        } else {
            None
        };
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
        let (prompt, conversation_id, session_url, is_new, _request_had_tools) =
            self.prepare_turn(&request)?;
        let tools_for_stream = request.tools.clone().unwrap_or_default();
        let template_b64 = if is_new { HOME_TEMPLATE_B64 } else { CHAT_TEMPLATE_B64 };

        self.warmup_and_mode_switch(&conversation_id, request.thinking).await?;

        let attachment_block = self.upload_attachment_block(&request).await?;

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
            tool_defs: tools_for_stream,
            sent_first_chunk: false,
            mtp_state: crate::providers::mtp::MtpStreamState::new(),
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
                attachment_block.as_deref(),
                &mut ctx,
            )
            .await
            {
                Ok(()) => {
                    if !ctx.tool_defs.is_empty() {
                        ctx.mtp_state.finish(&ctx.tool_defs);
                        for call in ctx.mtp_state.collected_tool_calls.drain(..) {
                            if !ctx.collected_calls.iter().any(|c| c.id == call.id) {
                                ctx.collected_calls.push(call);
                            }
                        }
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
            None,
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

        // [1,1,5,5,1] conversation id
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
        let nested_115 = parse_proto_fields(field_5_bytes).unwrap();
        let field_115_5 = nested_115.iter().find(|f| f.number == 5).unwrap();
        let ProtoValue::Bytes(field_115_5_bytes) = &field_115_5.value else {
            panic!("field [1,1,5,5] is not length-delimited");
        };
        let nested_1155 = parse_proto_fields(field_115_5_bytes).unwrap();
        let field_1155_1 = nested_1155.iter().find(|f| f.number == 1).unwrap();
        let ProtoValue::Bytes(conv_bytes) = &field_1155_1.value else {
            panic!("field [1,1,5,5,1] is not length-delimited");
        };
        assert_eq!(String::from_utf8_lossy(conv_bytes), conversation_id);

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
        assert!(!url.contains("clippy-async"));
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
    fn accumulator_sets_done_on_terminal_response_id_frame() {
        // Streaming frames: partial text and in-progress thinking.
        let mut acc = WsAccumulator::default();
        acc.apply(
            r#"{"seq":0,"type":"full","response":{"response_id":"abc","sections":[
                {"view_model":{"primitive":{"__typename":"GenAIBotThinkingStatusPrimitive","is_in_progress":true}}},
                {"view_model":{"primitive":{"__typename":"GenAIMarkdownTextUXPrimitive","text":"yo, what's up"}}}
            ]}}"#,
        );
        assert!(!acc.done, "streaming in-progress frame must not complete the turn");
        assert_eq!(acc.content, "yo, what's up");

        // Terminal frame: top-level response_id, no type, is_in_progress:false.
        acc.apply(
            r#"{"response_id":"abc","sections":[
                {"view_model":{"primitive":{"__typename":"GenAIBotThinkingStatusPrimitive","is_in_progress":false}}},
                {"view_model":{"primitive":{"__typename":"GenAIMarkdownTextUXPrimitive","text":"yo, what's up? I'm here"}}}
            ]}"#,
        );
        assert!(acc.done, "terminal frame must mark the turn done");
        assert_eq!(acc.content, "yo, what's up? I'm here");
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

    // ─── native tool-call extraction ────────────────────────────────────────

    /// Real `search-results://` invocation block captured from a live DGW turn
    /// (`tool_capture/tool3/ws_1786706680778.json`, frame 8). The model emits
    /// the invocation header plus the fetched line dump as a bare
    /// `{"text": ...}` event, separate from the visible section stream.
    const CAPTURED_TOOL_BLOCK: &str = concat!(
        "[2252765915471961672] current weather Paris right now ",
        "(search-results://query?query=current weather Paris right now)\n",
        "**viewing lines [0 - 89] of 89**\n",
        "\n",
        "L0: # Search Results\n",
        "L1: \n",
        "L2: \n",
        "L3:   * \u{3010}0\u{2020}Paris Live Weather & Forecast\u{2020}weather.com\u{3011}\n",
        "L4: \n",
        "L5:   Today's Date: Friday, August 14, 2026 GMT\n",
        "L6:   \n",
        "L7:   In Paris, Ile-de-France, it is currently Sunny. Temperature is 39 C.\n",
        "L8:   Aug 14, Friday: High 40\u{00b0}, Low 22\u{00b0}.\n",
        "L89:   of 11, Visibility, 8.05 km, Moon Phase, Waning Crescent",
    );

    #[test]
    fn extract_native_tool_calls_parses_captured_block() {
        let content = format!(
            "{CAPTURED_TOOL_BLOCK}\nRight now in **Paris, France** it's extremely hot and sunny"
        );
        let (clean, calls) = extract_native_tool_calls(&content);
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.id, "call_2252765915471961672");
        assert_eq!(call.r#type, "function");
        assert_eq!(call.function.name, "web_search");
        let args: serde_json::Value = serde_json::from_str(&call.function.arguments).unwrap();
        assert_eq!(args["query"], "current weather Paris right now");
        // The block (header + viewing-lines + dump) is tool machinery, so the
        // visible content keeps only the answer text.
        assert_eq!(
            clean,
            "Right now in **Paris, France** it's extremely hot and sunny"
        );
    }

    #[test]
    fn parse_native_tool_block_consumes_dump_without_trailing_newline() {
        let (call, end) = parse_native_tool_block(CAPTURED_TOOL_BLOCK, 0).unwrap();
        assert_eq!(call.query, "current weather Paris right now");
        assert_eq!(end, CAPTURED_TOOL_BLOCK.len());
    }

    #[test]
    fn accumulator_collects_native_tool_blocks_deduped() {
        let mut acc = WsAccumulator::default();
        let event =
            serde_json::to_string(&serde_json::json!({ "text": CAPTURED_TOOL_BLOCK })).unwrap();
        acc.apply(&event);
        acc.apply(&event);
        assert_eq!(acc.tool_blocks.len(), 1, "duplicate blocks must be deduped");
        assert!(acc.content.is_empty(), "tool blocks must not touch visible content");
        assert!(!acc.done);
    }

    #[test]
    fn accumulator_separates_tool_blocks_from_answer_deltas() {
        let mut acc = WsAccumulator::default();
        acc.apply(
            r#"{"type":"full","response":{"sections":[{"view_model":{"primitive":{"text":"Right now in **Paris** it's hot"}}}]}}"#,
        );
        let tool =
            serde_json::to_string(&serde_json::json!({ "text": CAPTURED_TOOL_BLOCK })).unwrap();
        acc.apply(&tool);
        assert_eq!(acc.content, "Right now in **Paris** it's hot");
        assert_eq!(acc.deltas, vec!["Right now in **Paris** it's hot"]);
        assert_eq!(acc.tool_blocks.len(), 1);
        let (_, calls) = extract_native_tool_calls(&acc.tool_blocks[0]);
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn accumulator_ignores_non_invocation_text_events() {
        let mut acc = WsAccumulator::default();
        // The weather citation block carries a real URL, not a
        // `search-results://` invocation; it must not become a tool call.
        let weather = serde_json::to_string(&serde_json::json!({
            "text": "[6262884135635057517] Paris Live Weather & Forecast (https://weather.com/city/paris/today)\n**viewing lines [0 - 66] of 66**\nL0: Today's Date: Friday, August 14, 2026 GMT"
        }))
        .unwrap();
        acc.apply(&weather);
        assert!(acc.tool_blocks.is_empty());
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
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn attachment_block_matches_captured_wire_format() {
        // Ground truth from the image-attach capture (imgcap3): media_id
        // 1655280158904939, mime image/png, filename test.png.
        let block = build_attachment_block(1655280158904939, "image/png", "test.png");
        let fields = parse_proto_fields(&block).unwrap();
        let mut id_field = None;
        let mut flag = None;
        let mut empty = None;
        let mut n5 = None;
        let mut mime = None;
        let mut name = None;
        for f in &fields {
            match (f.number, &f.value) {
                (1, ProtoValue::Bytes(inner)) => id_field = Some(inner.clone()),
                (2, ProtoValue::Varint(v)) => flag = Some(*v),
                (3, ProtoValue::Bytes(inner)) => empty = Some(inner.clone()),
                (5, ProtoValue::Varint(v)) => n5 = Some(*v),
                (6, ProtoValue::Bytes(inner)) => mime = Some(inner.clone()),
                (7, ProtoValue::Bytes(inner)) => name = Some(inner.clone()),
                _ => {}
            }
        }
        let id_inner = parse_proto_fields(id_field.as_ref().unwrap()).unwrap();
        assert_eq!(id_inner.len(), 1);
        assert!(matches!(
            (&id_inner[0].number, &id_inner[0].value),
            (1, ProtoValue::Varint(1655280158904939))
        ));
        assert_eq!(flag, Some(1));
        assert_eq!(empty, Some(vec![]));
        assert_eq!(n5, Some(0));
        assert_eq!(mime, Some(b"image/png".to_vec()));
        assert_eq!(name, Some(b"test.png".to_vec()));
    }

    #[test]
    fn prompt_frame_injects_attachment_at_2_3() {
        let block = build_attachment_block(1655280158904939, "image/png", "test.png");
        let frame = build_ws_prompt_frame(
            "describe this image",
            "c.test123",
            HOME_TEMPLATE_B64,
            "req-123",
            "user-123",
            1_700_000_000_123,
            42,
            Some(&block),
        )
        .unwrap();
        let body_len = (frame[3] as usize)
            | ((frame[4] as usize) << 8)
            | ((frame[5] as usize) << 16);
        let body = &frame[6..6 + body_len];
        assert_eq!(body[0], 0);
        assert_eq!(body[1], WS_PROMPT_FRAME_FLAG);
        let outer: serde_json::Value = serde_json::from_slice(&body[2..]).unwrap();
        let payload = base64::engine::general_purpose::STANDARD
            .decode(outer["payload"].as_str().unwrap())
            .unwrap();
        let root = parse_proto_fields(&payload).unwrap();
        let field2 = root.iter().find(|f| f.number == 2).unwrap();
        let ProtoValue::Bytes(msg2) = &field2.value else {
            panic!("field 2 not bytes");
        };
        let fields2 = parse_proto_fields(msg2).unwrap();
        let numbers: Vec<u32> = fields2.iter().map(|f| f.number).collect();
        assert_eq!(
            numbers,
            vec![1, 2, 3, 4],
            "attachment must slot between text and branch"
        );
        let attachment = fields2.iter().find(|f| f.number == 3).unwrap();
        let ProtoValue::Bytes(injected) = &attachment.value else {
            panic!("field 3 not bytes");
        };
        assert_eq!(injected.as_slice(), block.as_slice());
    }
}

