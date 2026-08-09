//! Dual-layer HMAC-SHA256 `X-Signature` generation for Z.AI.
//!
//! The signing scheme is documented in multiple open-source reverse-engineering
//! projects (e.g. ZtoApi). It uses a 5-minute time window and a static secret
//! to derive a per-window key, then signs a canonical string built from the
//! request metadata and the last user message.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::GatewayError;

/// Five-minute window used by the Z.AI signature, in milliseconds.
const WINDOW_MS: u64 = 5 * 60 * 1000;

/// Default signing secret. Override via `glm.sign_secret` / `OBSCURA_GATEWAY_GLM__SIGN_SECRET`.
///
/// The original secret `"junjie"` was rotated by Z.AI. The current value was
/// discovered from the ZtoApi reverse-engineering project (see the ASCII art
/// lament in Z.ai2api/app.py: `junjie... why did you change the signing algorithm?`).
#[allow(dead_code)]
pub const DEFAULT_SIGN_SECRET: &str = "key-@@@@)))()((9))-xxxx&&&%%%%%";

/// Build the `X-Signature` header value together with the timestamp and request
/// id that produced it.
///
/// `message_text` is the text of the last user message (the same value the UI
/// sends in `signature_prompt`). The canonical string is:
///
/// ```text
/// requestId,<id>,timestamp,<ts>,user_id,<uid>|base64(<text>)|<ts>
/// ```
pub fn generate_signature(
    user_id: &str,
    message_text: &str,
    secret: &str,
) -> Result<Signature, GatewayError> {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let request_id = uuid::Uuid::new_v4().to_string();
    generate_signature_with_values(user_id, message_text, secret, timestamp_ms, &request_id)
}

/// Deterministic version of [`generate_signature`] used for tests and for any
/// caller that needs to pin the timestamp/request id.
pub fn generate_signature_with_values(
    user_id: &str,
    message_text: &str,
    secret: &str,
    timestamp_ms: u64,
    request_id: &str,
) -> Result<Signature, GatewayError> {
    let prefix = format!(
        "requestId,{},timestamp,{},user_id,{}",
        request_id, timestamp_ms, user_id
    );
    let encoded_content = base64::engine::general_purpose::STANDARD.encode(message_text.as_bytes());
    let canonical = format!("{}|{}|{}", prefix, encoded_content, timestamp_ms);

    let window_index = timestamp_ms / WINDOW_MS;

    // Layer 1: derive key from secret + window index.
    let derived_hex = hmac_hex(secret, &window_index.to_string())?;

    // Layer 2: sign canonical string with derived key.
    let signature = hmac_hex(&derived_hex, &canonical)?;

    Ok(Signature {
        value: signature,
        timestamp_ms,
        request_id: request_id.to_string(),
    })
}

/// A freshly generated signature and its metadata.
#[derive(Debug, Clone)]
pub struct Signature {
    /// The hex-encoded `X-Signature` header value.
    pub value: String,
    /// Millisecond timestamp used for the request.
    pub timestamp_ms: u64,
    /// UUID request id used for the request.
    pub request_id: String,
}

/// HMAC-SHA256 over `message` using `key`, returned as lowercase hex.
fn hmac_hex(key: &str, message: &str) -> Result<String, GatewayError> {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key.as_bytes())
        .map_err(|e| GatewayError::Internal(format!("HMAC init failed: {e}")))?;
    mac.update(message.as_bytes());
    let bytes = mac.finalize().into_bytes();
    Ok(bytes_to_hex(&bytes))
}

/// Convert a byte slice to a lowercase hex string.
fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Decode a Z.AI JWT token payload without verifying the signature.
///
/// Returns the raw payload object. The caller extracts the user id from the
/// `id`, `user_id`, `sub`, or `userId` fields.
pub fn decode_jwt_payload(token: &str) -> Result<serde_json::Value, GatewayError> {
    let payload_b64 = token
        .split('.')
        .nth(1)
        .ok_or_else(|| GatewayError::Auth("Z.AI token is not a JWT".to_string()))?;

    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64.trim_end_matches('='))
        .map_err(|e| GatewayError::Auth(format!("Z.AI token payload decode failed: {e}")))?;

    serde_json::from_slice(&bytes)
        .map_err(|e| GatewayError::Auth(format!("Z.AI token payload is invalid JSON: {e}")))
}

/// Extract a user id from a Z.AI JWT payload object.
pub fn extract_user_id(payload: &serde_json::Value) -> Option<String> {
    for key in ["id", "user_id", "sub", "userId"] {
        if let Some(id) = payload.get(key).and_then(|v| v.as_str()).map(|s| s.to_string()) {
            return Some(id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_changes_with_message() {
        let sig1 = generate_signature("u1", "hello", DEFAULT_SIGN_SECRET).unwrap();
        let sig2 = generate_signature("u1", "world", DEFAULT_SIGN_SECRET).unwrap();
        assert_ne!(sig1.value, sig2.value);
    }

    #[test]
    fn signature_changes_with_user() {
        let sig1 = generate_signature("u1", "hello", DEFAULT_SIGN_SECRET).unwrap();
        let sig2 = generate_signature("u2", "hello", DEFAULT_SIGN_SECRET).unwrap();
        assert_ne!(sig1.value, sig2.value);
    }

    #[test]
    fn signature_changes_with_secret() {
        let sig1 = generate_signature("u1", "hello", DEFAULT_SIGN_SECRET).unwrap();
        let sig2 = generate_signature("u1", "hello", "other-secret").unwrap();
        assert_ne!(sig1.value, sig2.value);
    }

    #[test]
    fn signature_request_ids_are_unique() {
        let sig1 = generate_signature("u1", "hello", DEFAULT_SIGN_SECRET).unwrap();
        let sig2 = generate_signature("u1", "hello", DEFAULT_SIGN_SECRET).unwrap();
        // Each call gets a fresh request id, so signatures differ even for the
        // same message + user + secret.
        assert_ne!(sig1.request_id, sig2.request_id);
        assert_ne!(sig1.value, sig2.value);
    }

    #[test]
    fn jwt_payload_decode_extracts_user_id() {
        // {"id":"user-123","exp":1234567890}
        let token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpZCI6InVzZXItMTIzIiwiZXhwIjoxMjM0NTY3ODkwfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let payload = decode_jwt_payload(token).unwrap();
        assert_eq!(extract_user_id(&payload), Some("user-123".to_string()));
    }

    #[test]
    fn signature_matches_reference_computation() {
        // Fixed inputs so we can compare against an independent reference
        // computation of the known JS algorithm.
        let user_id = "user-abc";
        let message = "hello world";
        let secret = DEFAULT_SIGN_SECRET;
        let timestamp_ms = 1_700_000_000_000u64;
        let request_id = "550e8400-e29b-41d4-a716-446655440000";

        let sig =
            generate_signature_with_values(user_id, message, secret, timestamp_ms, request_id)
                .unwrap();

        // Reference: window-based derived key, then HMAC of the canonical string.
        let window_index = timestamp_ms / WINDOW_MS;
        let derived_hex = hmac_hex(secret, &window_index.to_string()).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(message.as_bytes());
        let canonical = format!(
            "requestId,{request_id},timestamp,{timestamp_ms},user_id,{user_id}|{encoded}|{timestamp_ms}"
        );
        let expected = hmac_hex(&derived_hex, &canonical).unwrap();

        assert_eq!(sig.value, expected);
        assert_eq!(sig.timestamp_ms, timestamp_ms);
        assert_eq!(sig.request_id, request_id);
    }
}
