use tracing::info;

use crate::browser::LocalStorageEntry;
use crate::error::GatewayError;

const QWEN_ORIGIN: &str = "https://chat.qwen.ai";

/// Resolve a Qwen JWT bearer token from imported browser localStorage.
///
/// Looks for key `token` at origin `https://chat.qwen.ai`. The value is a
/// raw JWT string (not JSON-wrapped). Returns an error if missing or empty.
pub fn resolve_token(local_storage: &[LocalStorageEntry]) -> Result<String, GatewayError> {
    let token = local_storage
        .iter()
        .find(|e| e.origin == QWEN_ORIGIN && e.key == "token")
        .map(|e| e.value.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            GatewayError::Auth(
                "Qwen token not found in localStorage; log in at chat.qwen.ai in Firefox and re-import"
                    .to_string(),
            )
        })?;

    // Validate it looks like a JWT (three dot-separated base64 segments).
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
        return Err(GatewayError::Auth(
            "Qwen token does not look like a valid JWT (expected 3 dot-separated segments)"
                .to_string(),
        ));
    }

    info!(len = token.len(), "Qwen JWT token resolved from localStorage");

    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(key: &str, value: &str) -> LocalStorageEntry {
        LocalStorageEntry {
            origin: QWEN_ORIGIN.to_string(),
            key: key.to_string(),
            value: value.to_string(),
        }
    }

    #[test]
    fn resolves_valid_jwt() {
        let ls = vec![make_entry(
            "token",
            "eyJhbGciOiJIUzI1NiJ9.eyJpZCI6InRlc3QifQ.signature",
        )];
        assert!(resolve_token(&ls).is_ok());
    }

    #[test]
    fn rejects_missing_key() {
        let ls = vec![make_entry("not_token", "foo")];
        assert!(resolve_token(&ls).is_err());
    }

    #[test]
    fn rejects_empty_value() {
        let ls = vec![make_entry("token", "")];
        assert!(resolve_token(&ls).is_err());
    }

    #[test]
    fn rejects_non_jwt() {
        let ls = vec![make_entry("token", "not-a-jwt")];
        assert!(resolve_token(&ls).is_err());
    }
}
