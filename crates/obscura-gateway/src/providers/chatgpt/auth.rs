use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, AUTHORIZATION, USER_AGENT};
use tracing::info;

use crate::error::GatewayError;
use crate::session::SessionManager;

pub struct AuthData {
    pub access_token: String,
    #[allow(dead_code)]
    pub user_agent: String,
}

const CHATGPT_URL: &str = "https://chatgpt.com";

/// Navigate the session to chatgpt.com and wait for the page to load.
pub async fn navigate_to_chatgpt(sessions: &SessionManager, session_id: &str) -> Result<(), GatewayError> {
    info!(session_id = %session_id, url = %CHATGPT_URL, "Navigating session to ChatGPT");
    sessions.navigate(session_id, CHATGPT_URL).await?;
    tokio::time::sleep(Duration::from_secs(8)).await;
    Ok(())
}

/// Extract the Bearer access token from the chatgpt.com page WITHOUT navigating.
///
/// This is an optimization to avoid the 8-second navigation sleep on every request.
/// It attempts to extract the token directly from:
/// 1. Direct HTTP request to `/api/auth/session` using existing cookies (fastest)
/// 2. localStorage if accessible
///
/// Returns Ok(Some(token)) if successful, Ok(None) if cookies/token are stale,
/// Err if extraction fails entirely.
///
/// Use this for fast-path extraction. If it fails, fall back to
/// `navigate_to_chatgpt()` + `extract_bearer_token()` for guaranteed extraction.
pub async fn extract_bearer_token_direct(
    sessions: &SessionManager,
    session_id: &str,
    cookie_jar: &obscura_net::CookieJar,
) -> Result<Option<String>, GatewayError> {
    let chatgpt_url = url::Url::parse(CHATGPT_URL)
        .map_err(|e| GatewayError::Internal(format!("invalid URL: {e}")))?;

    // Fast-path: Direct HTTP request using existing cookies
    // (bypasses V8 JS runtime entirely)
    let auth_url = format!("{}/api/auth/session", CHATGPT_URL);
    let cookie_header = cookie_jar.get_cookie_header(&chatgpt_url);

    // If no cookies, return None immediately (session not authenticated yet)
    if cookie_header.is_empty() {
        return Ok(None);
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| GatewayError::Internal(format!("failed to build HTTP client: {e}")))?;

    match client
        .get(&auth_url)
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64; rv:140.12) Gecko/20100101 Firefox/140.12")
        .header("Cookie", &cookie_header)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            if let Ok(data) = r.json::<serde_json::Value>().await {
                let token = data
                    .get("accessToken")
                    .or_else(|| data.get("token"))
                    .or_else(|| data.get("jwt"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());

                if let Some(token) = token {
                    return Ok(Some(token));
                }
            }
        }
        Ok(r) if r.status() == 401 => {
            // Cookies are stale/invalid
            return Ok(None);
        }
        Ok(r) => {
            tracing::debug!(
                session_id = %session_id,
                status = %r.status(),
                "direct token extraction returned non-success status"
            );
            return Ok(None);
        }
        Err(e) => {
            tracing::debug!(
                session_id = %session_id,
                error = %e,
                "direct token extraction HTTP request failed (will retry with navigation)"
            );
            return Ok(None);
        }
    }

    Ok(None)
}

/// Extract the Bearer access token from the chatgpt.com page.
///
/// Sources (in order):
/// 1. `fetch('/api/auth/session')` from the page JS
/// 2. Direct HTTP request to `chatgpt.com/api/auth/session` using the cookie jar
/// 3. `window.__remixContext` SSR data
/// 4. `localStorage` (oai-access-token, nextauth.accessToken)
///
/// The `usc_` cookie and `__Secure-next-auth.session-token` are NOT
/// valid Bearer tokens — they must NOT be used as the Authorization
/// header. Cookies are for session identification; the Bearer token
/// is obtained from the page JS or localStorage.
pub async fn extract_bearer_token(
    sessions: &SessionManager,
    session_id: &str,
    cookie_jar: &obscura_net::CookieJar,
) -> Result<AuthData, GatewayError> {
    // Retry loop: the Remix SPA needs time to hydrate after navigation.
    // Try every 2s up to ~30s to give the page time to render.
    let max_attempts = 15;
    let mut last_error = None;

    let chatgpt_url = url::Url::parse(CHATGPT_URL)
        .map_err(|e| GatewayError::Internal(format!("invalid URL: {e}")))?;

    for attempt in 1..=max_attempts {
        // Primary: call the session endpoint from the page JS.
        let js = r#"
            (async function() {
                try {
                    const resp = await fetch('/api/auth/session');
                    const data = await resp.json();
                    const token = data.accessToken || data.token
                        || data.sessionToken || data.session_token
                        || data.jwt || data.bearer
                        || (data.user && (data.user.accessToken || data.user.token))
                        || null;
                    return { accessToken: token };
                } catch (e) {
                    return { accessToken: null, error: e.message };
                }
            })()
        "#;
        let value = sessions.execute_js(session_id, js).await;
        if let Ok(serde_json::Value::Object(map)) = &value {
            let token = map.get("accessToken").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
            if let Some(token) = token {
                info!(session_id = %session_id, attempt = %attempt, "ChatGPT access token extracted from /api/auth/session (JS)");
                return Ok(AuthData {
                    access_token: token.to_string(),
                    user_agent: "Mozilla/5.0 (X11; Linux x86_64; rv:140.12) Gecko/20100101 Firefox/140.12".to_string(),
                });
            }
            // Capture the full response body for diagnostics
            let _body = serde_json::to_string(map);
        }

        // Fallback: direct HTTP request using the cookie jar.
        // This bypasses the V8 JS runtime and tests the cookies directly.
        let auth_url = format!("{}/api/auth/session", CHATGPT_URL);
        let cookie_header = cookie_jar.get_cookie_header(&chatgpt_url);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| GatewayError::Internal(format!("failed to build HTTP client: {e}")))?;
        let resp = client
            .get(&auth_url)
            .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64; rv:140.12) Gecko/20100101 Firefox/140.12")
            .header("Cookie", &cookie_header)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                let token = data.get("accessToken")
                    .or_else(|| data.get("token"))
                    .or_else(|| data.get("jwt"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty());
                if let Some(token) = token {
                    info!(session_id = %session_id, attempt = %attempt, "ChatGPT access token extracted from direct HTTP /api/auth/session");
                    return Ok(AuthData {
                        access_token: token.to_string(),
                        user_agent: "Mozilla/5.0".to_string(),
                    });
                }
                // Log the response body for diagnostics
                info!(session_id = %session_id, attempt = %attempt, body = %serde_json::to_string(&data).unwrap_or_default(), "ChatGPT /api/auth/session returned no token");
            }
            Ok(r) => {
                info!(session_id = %session_id, attempt = %attempt, status = %r.status(), "ChatGPT direct HTTP /api/auth/session failed");
            }
            Err(e) => {
                info!(session_id = %session_id, attempt = %attempt, error = %e, "ChatGPT direct HTTP /api/auth/session request failed");
            }
        }

        // Fallback: extract from window.__remixContext (SSR data).
        let remix_js = r#"
            (function() {
                try {
                    const ctx = window.__remixContext || {};
                    const state = ctx.state || {};
                    const loaderData = state.loaderData || {};
                    const root = loaderData.root || {};
                    const token = root.accessToken || root.token
                        || root.sessionToken
                        || root.jwt || root.bearer
                        || (root.user && (root.user.accessToken || root.user.token))
                        || null;
                    return { accessToken: token };
                } catch (e) {
                    return { accessToken: null };
                }
            })()
        "#;
        let remix_value = sessions.execute_js(session_id, remix_js).await;
        if let Ok(serde_json::Value::Object(map)) = &remix_value {
            let token = map.get("accessToken").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
            if let Some(token) = token {
                info!(session_id = %session_id, attempt = %attempt, "ChatGPT access token extracted from remix context");
                return Ok(AuthData {
                    access_token: token.to_string(),
                    user_agent: "Mozilla/5.0".to_string(),
                });
            }
        }

        // Fallback: extract from localStorage (works after SPA hydrates).
        let ls_js = r#"
            (function() {
                try {
                    const t = localStorage.getItem('__oai_access_token');
                    const token = t || localStorage.getItem('nextauth.accessToken')
                        || localStorage.getItem('accessToken')
                        || localStorage.getItem('oai-access-token')
                        || null;
                    return { accessToken: token };
                } catch (e) {
                    return { accessToken: null };
                }
            })()
        "#;
        let ls_value = sessions.execute_js(session_id, ls_js).await;
        if let Ok(serde_json::Value::Object(map)) = &ls_value {
            let token = map.get("accessToken").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
            if let Some(token) = token {
                info!(session_id = %session_id, attempt = %attempt, "ChatGPT access token extracted from localStorage");
                return Ok(AuthData {
                    access_token: token.to_string(),
                    user_agent: "Mozilla/5.0".to_string(),
                });
            }
        }

        last_error = Some(format!(
            "attempt {attempt}/{max_attempts}: no access token found in page JS, remix context, or localStorage"
        ));
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    Err(GatewayError::Auth(format!(
        "ChatGPT auth data not found in page after {max_attempts} attempts. \
         Last error: {}. \
         Ensure the session is navigated to https://chatgpt.com \
         and the page is fully loaded with valid OpenAI cookies.",
        last_error.unwrap_or_default()
    )))
}

/// Build HTTP headers for a ChatGPT conversation request.
pub fn build_request_headers(
    access_token: &str,
    cookie_header: &str,
    user_agent: &str,
) -> Result<HeaderMap, GatewayError> {
    let mut headers = HeaderMap::new();

    headers.insert(
        HeaderName::from_static("authority"),
        HeaderValue::from_static("chatgpt.com"),
    );

    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));

    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("en-US,en;q=0.9"),
    );

    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );

    // Bearer token from the page JS session — required for API auth.
    if !access_token.is_empty() {
        let bearer = format!("Bearer {}", access_token);
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&bearer)
                .map_err(|e| GatewayError::Internal(format!("invalid bearer: {e}")))?,
        );
    }

    if !cookie_header.is_empty() {
        headers.insert(
            HeaderName::from_static("cookie"),
            HeaderValue::from_str(cookie_header)
                .map_err(|e| GatewayError::Internal(format!("invalid cookie: {e}")))?,
        );
    }

    headers.insert(
        HeaderName::from_static("oai-device-id"),
        HeaderValue::from_static("00000000-0000-0000-0000-000000000000"),
    );

    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(user_agent)
            .map_err(|e| GatewayError::Internal(format!("invalid user-agent: {e}")))?,
    );

    headers.insert(
        HeaderName::from_static("origin"),
        HeaderValue::from_static("https://chatgpt.com"),
    );

    headers.insert(
        HeaderName::from_static("referer"),
        HeaderValue::from_static("https://chatgpt.com/"),
    );

    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_headers_include_bearer() {
        let headers = build_request_headers("test_token", "cookie=abc", "Mozilla/5.0").unwrap();
        assert!(headers.contains_key("authorization"));
        assert_eq!(
            headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer test_token"
        );
        assert!(headers.contains_key("cookie"));
    }

    #[test]
    fn build_headers_with_empty_cookies() {
        let headers = build_request_headers("test_token", "", "Mozilla/5.0").unwrap();
        assert!(headers.contains_key("authorization"));
        assert!(!headers.contains_key("cookie"));
    }
}
