//! GLM/Z.AI authentication helpers.
//!
//! The direct internal-API path needs a bearer token imported from the user's
//! real `chat.z.ai` session. We try, in order:
//!
//! 1. The `token` / `access_token` localStorage entry imported from the user's
//!    real `chat.z.ai` session.
//! 2. A `token` cookie for `chat.z.ai`.
//!
//! There is no anonymous/guest mode: without an imported token the provider
//! fails closed with an `Auth` error so the user knows to log in.
//!
//! The JWT payload carries the `user_id` required for request signing.

use std::time::{SystemTime, UNIX_EPOCH};

use obscura_net::CookieJar;
use tracing::{info, warn};

use crate::browser::LocalStorageEntry;
use crate::error::GatewayError;

use super::signature::{decode_jwt_payload, extract_user_id};

const ZAI_ORIGIN: &str = "https://chat.z.ai";

/// Auth context resolved for one direct-API request.
#[derive(Debug, Clone)]
pub struct AuthContext {
    /// Bearer token to send upstream.
    pub token: String,
    /// User id extracted from the token payload.
    pub user_id: String,
}

/// Resolve a usable auth context for the direct API from the imported browser
/// session (localStorage token, then cookie token). Fails closed when neither
/// is present or usable.
pub async fn resolve_auth(
    local_storage: &[LocalStorageEntry],
    cookie_jar: &CookieJar,
) -> Result<AuthContext, GatewayError> {
    // Priority 1: imported localStorage token.
    if let Some(token) = extract_local_storage_token(local_storage) {
        if let Some(ctx) = auth_context_from_token(token) {
            info!(source = "localStorage", "GLM auth resolved");
            return Ok(ctx);
        }
    }

    // Priority 2: imported cookie token.
    if let Some(token) = extract_cookie_token(cookie_jar) {
        if let Some(ctx) = auth_context_from_token(token) {
            info!(source = "cookie", "GLM auth resolved");
            return Ok(ctx);
        }
    }

    Err(GatewayError::Auth(
        "no Z.AI authentication token found; import a logged-in chat.z.ai session \
         (localStorage `token` or the `token` cookie) and retry"
            .to_string(),
    ))
}

/// Build an auth context from a token string, extracting the user id from the
/// JWT payload and checking expiry.
fn auth_context_from_token(token: String) -> Option<AuthContext> {
    let payload = decode_jwt_payload(&token).ok()?;
    let user_id = extract_user_id(&payload)?;
    if user_id.is_empty() {
        return None;
    }
    // Skip expired tokens so resolve_auth falls through to the cookie source.
    if let Some(exp) = payload.get("exp").and_then(|v| v.as_i64()) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if exp < now {
            warn!(
                token_exp = exp,
                now = now,
                source = "import",
                "GLM JWT token is expired, skipping",
            );
            return None;
        }
    }
    Some(AuthContext { token, user_id })
}

/// Extract a `token` or `access_token` localStorage entry for chat.z.ai.
fn extract_local_storage_token(local_storage: &[LocalStorageEntry]) -> Option<String> {
    find_local_storage(local_storage, ZAI_ORIGIN, "token")
        .or_else(|| find_local_storage(local_storage, ZAI_ORIGIN, "access_token"))
        .filter(|t| !t.is_empty())
}

/// Extract a `token` cookie for chat.z.ai.
fn extract_cookie_token(cookie_jar: &CookieJar) -> Option<String> {
    let cookies = cookie_jar.get_all_cookies();
    cookies
        .into_iter()
        .find(|c| {
            let domain = c.domain.to_lowercase();
            (domain == "chat.z.ai" || domain.trim_start_matches('.') == "chat.z.ai")
                && c.name == "token"
        })
        .map(|c| c.value)
        .filter(|t| !t.is_empty())
}

fn find_local_storage(
    local_storage: &[LocalStorageEntry],
    origin: &str,
    key: &str,
) -> Option<String> {
    local_storage
        .iter()
        .find(|e| e.origin == origin && e.key == key)
        .map(|e| e.value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::LocalStorageEntry;

    #[test]
    fn extract_finds_zai_token() {
        let ls = vec![LocalStorageEntry {
            origin: "https://chat.z.ai".to_string(),
            key: "token".to_string(),
            value: "eyJhbGciOiJFUzI1NiIsInR5cCI6IkpXVCJ9.eyJpZCI6InVzZXItMTIzIn0.test".to_string(),
        }];
        let result = extract_local_storage_token(&ls);
        assert!(result.is_some());
    }

    #[test]
    fn extract_returns_none_when_missing() {
        assert!(extract_local_storage_token(&[]).is_none());
    }

    #[test]
    fn auth_context_extracts_user_id() {
        // {"id":"user-123"}
        let token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpZCI6InVzZXItMTIzIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let ctx = auth_context_from_token(token.to_string()).unwrap();
        assert_eq!(ctx.user_id, "user-123");
    }

    #[tokio::test]
    async fn resolve_auth_fails_closed_without_imported_token() {
        let jar = CookieJar::default();
        let result = resolve_auth(&[], &jar).await;
        assert!(matches!(result, Err(GatewayError::Auth(_))));
    }
}
