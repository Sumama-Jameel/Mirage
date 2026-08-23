//! Kimi ConnectRPC transport (kimi.ai web app, capture-verified).
//!
//! The kimi.ai web client talks to
//! `POST https://www.kimi.ai/apiv2/kimi.gateway.chat.v1.ChatService/Chat`
//! with `content-type: application/connect+json` and ConnectRPC streaming
//! frames. Each frame is a 5-byte header (1 flag byte + 4-byte big-endian
//! payload length) followed by a JSON event object:
//!
//! ```text
//! {"op":"append","mask":"block.text.content","eventOffset":9,
//!  "block":{"id":"3","parentId":"2","text":{"content":" user"}}}
//! ```
//!
//! Event masks handled: `block.think.content` (reasoning delta),
//! `block.text.content` (content delta), `chat` / `chat.lastRequest`
//! (conversation ids), `message` (assistant message id for continuation),
//! `heartbeat`, and terminal frames.
//!
//! Evidence: `captures/kimi_chat_wire.txt`.

use serde_json::Value;

/// ConnectRPC chat endpoint on the kimi.ai host.
pub const CONNECT_CHAT_URL: &str =
    "https://www.kimi.ai/apiv2/kimi.gateway.chat.v1.ChatService/Chat";

/// Scenario id from the live capture (K2-family models).
pub const SCENARIO_K2D5: &str = "SCENARIO_K2D5";

/// Build the ConnectRPC request body for one turn.
///
/// `search` toggles the built-in `TOOL_TYPE_SEARCH` tool (the only way the
/// provider's search is engaged - never via client tool names). `thinking`
/// maps to `options.thinking`.
pub fn build_chat_body(
    content: &str,
    thinking: bool,
    search: bool,
    reasoning_effort: Option<&str>,
) -> Value {
    let tools = if search {
        serde_json::json!([{ "type": "TOOL_TYPE_SEARCH", "search": {} }])
    } else {
        serde_json::json!([])
    };
    serde_json::json!({
        "scenario": SCENARIO_K2D5,
        "tools": tools,
        "message": {
            "role": "user",
            "blocks": [{ "message_id": "", "text": { "content": content } }],
            "scenario": SCENARIO_K2D5,
            "is_goal": false,
        },
        "options": {
            "thinking": thinking,
            "enable_plugin": true,
            "reasoning_effort": reasoning_effort.unwrap_or("REASONING_EFFORT_LOW"),
        },
        "project_id": "",
    })
}

/// Required headers for the ConnectRPC endpoint (capture-verified).
///
/// `traffic_id` is the JWT `sub` claim; `device_id`/`session_id` are the
/// stable numeric browser identifiers persisted in the auth state.
pub fn build_headers(
    access_token: &str,
    device_id: &str,
    session_id: &str,
    traffic_id: &str,
) -> std::collections::HashMap<String, String> {
    let mut h = std::collections::HashMap::new();
    h.insert("Content-Type".to_string(), "application/connect+json".to_string());
    h.insert("connect-protocol-version".to_string(), "1".to_string());
    h.insert(
        "connect-accept-encoding".to_string(),
        "gzip".to_string(),
    );
    h.insert("authorization".to_string(), format!("Bearer {access_token}"));
    h.insert("x-msh-device-id".to_string(), device_id.to_string());
    h.insert("x-msh-platform".to_string(), "web".to_string());
    h.insert("x-msh-session-id".to_string(), session_id.to_string());
    h.insert("x-msh-version".to_string(), "2.0.0".to_string());
    h.insert("x-language".to_string(), "en-US".to_string());
    h.insert("origin".to_string(), "https://www.kimi.ai".to_string());
    h.insert("referer".to_string(), "https://www.kimi.ai/".to_string());
    if !traffic_id.is_empty() {
        h.insert("x-traffic-id".to_string(), traffic_id.to_string());
    }
    h
}

/// Incremental ConnectRPC frame decoder.
///
/// Feed raw network bytes; complete JSON payloads are drained via [`Self::drain`].
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append raw bytes from the stream.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Decode any buffered partial frame prefix into a carry buffer.
    ///
    /// Frames are `[flags:u8][len:u32 BE][payload]`; this returns how many
    /// bytes of a complete payload are currently available.
    fn next_frame(&mut self) -> Option<Vec<u8>> {
        const HEADER: usize = 5;
        if self.buf.len() < HEADER {
            return None;
        }
        let len = u32::from_be_bytes([
            self.buf[1], self.buf[2], self.buf[3], self.buf[4],
        ]) as usize;
        // Sanity cap: a corrupt length must not allocate unbounded memory.
        const MAX_FRAME: usize = 16 * 1024 * 1024;
        if len > MAX_FRAME {
            // Unrecoverable framing corruption; drop the buffer.
            self.buf.clear();
            return None;
        }
        if self.buf.len() < HEADER + len {
            return None;
        }
        let payload = self.buf[HEADER..HEADER + len].to_vec();
        self.buf.drain(..HEADER + len);
        Some(payload)
    }

    /// Drain every complete frame; payloads that parse as UTF-8 JSON are
    /// returned parsed. Non-JSON payloads (e.g. end-of-stream trailers) are
    /// skipped.
    pub fn drain(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        while let Some(payload) = self.next_frame() {
            match std::str::from_utf8(&payload).ok().and_then(|s| serde_json::from_str::<Value>(s).ok()) {
                Some(v) => out.push(v),
                None => continue,
            }
        }
        out
    }

    /// True when a partial frame is still buffered (mid-frame at EOF means a
    /// truncated upstream response). Consumed by stream diagnostics.
    #[allow(dead_code)]
    pub fn has_partial(&self) -> bool {
        !self.buf.is_empty()
    }
}

/// One parsed event from the ConnectRPC event stream.
#[derive(Debug, Clone, PartialEq)]
pub enum KimiEvent {
    /// Reasoning/thinking content delta.
    ThinkDelta(String),
    /// Assistant text content delta.
    TextDelta(String),
    /// Conversation id (`op:"set", mask:"chat"`).
    ChatId(String),
    /// Assistant message id (`mask:"message"` with role assistant).
    MessageId(String),
    /// Search result references (citation chunks).
    References(Value),
    /// Terminal frame: generation finished.
    Done,
    Heartbeat,
    Other,
}

/// Classify one decoded event frame by its `mask`.
pub fn classify_event(v: &Value) -> KimiEvent {
    let op = v.get("op").and_then(|o| o.as_str()).unwrap_or("");
    let block = v.get("block");
    // Mask-less frames first: bare heartbeat objects `{"heartbeat":{}}` and
    // done markers.
    if v.get("heartbeat").is_some() {
        return KimiEvent::Heartbeat;
    }
    if v.get("done").and_then(|d| d.as_bool()).unwrap_or(false)
        || v.get("status").and_then(|s| s.as_str()) == Some("STATUS_FINISHED")
    {
        return KimiEvent::Done;
    }
    if (op == "append" || op == "set") && v.get("mask").is_some() {
        let mask = v.get("mask").and_then(|m| m.as_str()).unwrap_or("");
        return match mask {
            "block.think.content" => {
                let c = block
                    .and_then(|b| b.get("think"))
                    .and_then(|t| t.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                KimiEvent::ThinkDelta(c.to_string())
            }
            "block.text.content" => {
                let c = block
                    .and_then(|b| b.get("text"))
                    .and_then(|t| t.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                KimiEvent::TextDelta(c.to_string())
            }
            "chat" | "chat.lastRequest" => {
                let id = v
                    .get("chat")
                    .and_then(|c| c.get("id"))
                    .and_then(|i| i.as_str())
                    .map(String::from);
                match id {
                    Some(id) => KimiEvent::ChatId(id),
                    None => KimiEvent::Other,
                }
            }
            "message" => {
                let msg = v.get("message");
                let role = msg.and_then(|m| m.get("role")).and_then(|r| r.as_str());
                let id = msg
                    .and_then(|m| m.get("id"))
                    .and_then(|i| i.as_str())
                    .map(String::from);
                if role == Some("assistant") {
                    match id {
                        Some(id) => KimiEvent::MessageId(id),
                        None => KimiEvent::Other,
                    }
                } else {
                    KimiEvent::Other
                }
            }
            "message.refs.searchChunks" | "message.references" => KimiEvent::References(v.clone()),
            "heartbeat" => KimiEvent::Heartbeat,
            _ => KimiEvent::Other,
        };
    }
    KimiEvent::Other
}

/// Encode one request payload as a single ConnectRPC unary frame.
pub fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(0u8); // flags: message
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_frame_codec() {
        let body = build_chat_body("what happened to netanyahu today?", true, true, None);
        assert_eq!(body["scenario"], SCENARIO_K2D5);
        assert_eq!(body["tools"][0]["type"], "TOOL_TYPE_SEARCH");
        assert_eq!(body["options"]["thinking"], true);
        assert_eq!(
            body["message"]["blocks"][0]["text"]["content"],
            "what happened to netanyahu today?"
        );

        let encoded = encode_frame(body.to_string().as_bytes());
        assert_eq!(encoded[0], 0u8);
        let len = u32::from_be_bytes([encoded[1], encoded[2], encoded[3], encoded[4]]) as usize;
        assert_eq!(len, body.to_string().len());

        let mut dec = FrameDecoder::new();
        dec.push(&encoded);
        let events = dec.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["scenario"], SCENARIO_K2D5);
        assert!(!dec.has_partial());
    }

    #[test]
    fn decode_capture_frames_split_across_chunks() {
        // Two frames mimicking the capture rendering: flags+BE-len prefixes.
        let p1 = br#"{"op":"append","mask":"block.think.content","eventOffset":9,"block":{"id":"3","parentId":"2","think":{"content":" user"}}}"#;
        let p2 = br#"{"op":"set","mask":"chat","eventOffset":1,"chat":{"id":"1a02578b-b0c2-8288-8000-09c4b52c85c2"}}"#;
        let mut wire = encode_frame(p1);
        wire.extend(encode_frame(p2));

        // Split at an arbitrary boundary inside the second header.
        let split_at = wire.len() - 10;
        let mut dec = FrameDecoder::new();
        dec.push(&wire[..split_at]);
        let mut events = dec.drain();
        assert!(!events.is_empty());
        dec.push(&wire[split_at..]);
        events.extend(dec.drain());

        let kinds: Vec<KimiEvent> = events.iter().map(classify_event).collect();
        assert!(kinds.iter().any(|k| matches!(k, KimiEvent::ThinkDelta(t) if t == " user")));
        assert!(kinds
            .iter()
            .any(|k| matches!(k, KimiEvent::ChatId(id) if id == "1a02578b-b0c2-8288-8000-09c4b52c85c2")));
    }

    #[test]
    fn classify_text_delta_and_done() {
        let text = classify_event(&serde_json::json!({
            "op":"append","mask":"block.text.content","eventOffset":30,
            "block":{"id":"4","parentId":"0","text":{"content":"Hello"}}
        }));
        assert_eq!(text, KimiEvent::TextDelta("Hello".to_string()));

        assert_eq!(classify_event(&serde_json::json!({"heartbeat":{}})), KimiEvent::Heartbeat);
        assert_eq!(classify_event(&serde_json::json!({"done":true})), KimiEvent::Done);
    }

    #[test]
    fn headers_include_connect_protocol() {
        let h = build_headers("tok", "dev", "sess", "sub");
        assert_eq!(h.get("Content-Type").unwrap(), "application/connect+json");
        assert_eq!(h.get("connect-protocol-version").unwrap(), "1");
        assert_eq!(h.get("authorization").unwrap(), "Bearer tok");
        assert_eq!(h.get("x-traffic-id").unwrap(), "sub");
    }
}
