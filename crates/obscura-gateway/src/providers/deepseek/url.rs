//! DeepSeek session URL helpers.
//!
//! DeepSeek's web app uses a per-session page URL like:
//!   https://chat.deepseek.com/a/chat/s/<session_id>
//!
//! We reuse that shape but fill it with an opaque *branch token* (not the raw
//! DeepSeek chat session id). Each completed turn mints a fresh token that
//! maps to `(chat_session_id, message_id)` in the store, so concurrent
//! continuations from the same parent each get their own continuable branch
//! (message-tree semantics).

use crate::error::GatewayError;

const SESSION_URL_PREFIX: &str = "https://chat.deepseek.com/a/chat/s/";

/// Mint a fresh opaque branch token for a completed turn.
pub fn new_session_token() -> String {
    format!("deepseek_{}", uuid::Uuid::new_v4().simple())
}

/// Build the DeepSeek session URL for a branch token.
pub fn build_session_url(token: &str) -> String {
    format!("{SESSION_URL_PREFIX}{token}")
}

/// Parse a branch token out of a DeepSeek session URL.
pub fn parse_session_url(url: &str) -> Result<String, GatewayError> {
    let rest = url
        .strip_prefix(SESSION_URL_PREFIX)
        .ok_or_else(|| GatewayError::BadRequest(format!("invalid session_url: {url}")))?;

    let id = rest
        .split('/')
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| GatewayError::BadRequest(format!("invalid session_url: {url}")))?;

    Ok(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_session_url() {
        let id = "74a62384-97c9-4e09-a3f3-d315051ca121";
        let url = build_session_url(id);
        assert_eq!(parse_session_url(&url).unwrap(), id);
    }

    #[test]
    fn new_session_token_is_unique_and_prefixed() {
        let a = new_session_token();
        let b = new_session_token();
        assert!(a.starts_with("deepseek_"));
        assert_ne!(a, b);
        // Token round-trips through the URL shape.
        let url = build_session_url(&a);
        assert_eq!(parse_session_url(&url).unwrap(), a);
    }

    #[test]
    fn rejects_malformed_url() {
        assert!(parse_session_url("https://example.com/74a62384").is_err());
        assert!(parse_session_url("https://chat.deepseek.com/a/chat/s/").is_err());
    }
}
