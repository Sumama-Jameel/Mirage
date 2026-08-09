use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, USER_AGENT};
use tracing::{info, warn};

use crate::error::GatewayError;
use crate::session::SessionManager;

/// Extract the `SNlM0e` CSRF token, `FdrFJe` session id, and `bl` BOQ parameter
/// from a warmed Gemini page.
///
/// The token is embedded in `window.WIZ_global_data` and is required
/// as the `at` parameter in every StreamGenerate RPC call. `FdrFJe` is sent
/// as the `f.sid` query parameter. The `bl` parameter identifies the BOQ
/// (Bundled Online Query) web server version.
pub async fn extract_auth_data(sessions: &SessionManager, session_id: &str) -> Result<AuthData, GatewayError> {
    // Try primary source: WIZ_global_data
    let js = r#"
        (function() {
            const data = window.WIZ_global_data || {};
            return {
                snlm0e: data.SNlM0e || null,
                sid: data.FdrFJe || null
            };
        })()
    "#;
    let value = sessions.execute_js(session_id, js).await?;
    if let serde_json::Value::Object(map) = &value {
        let snlm0e = map.get("snlm0e").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        let sid = map.get("sid").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        if let (Some(snlm0e), Some(sid)) = (snlm0e, sid) {
            tracing::debug!("SNlM0e and FdrFJe extracted from WIZ_global_data");
            // Try to get bl from WIZ_global_data as well.
            let bl = map
                .get("bl")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| bl_param_hardcoded());
            return Ok(AuthData {
                snlm0e: snlm0e.to_string(),
                sid: sid.to_string(),
                bl,
            });
        }
    }

    // Fallback: try to find values in the page's HTML source
    let html_js = r#"
        (function() {
            const html = document.documentElement.outerHTML;
            const snlm0e = (html.match(/"SNlM0e":"([^"]+)"/) || [])[1] || null;
            const sid = (html.match(/"FdrFJe":"([^"]+)"/) || [])[1] || null;
            const bl = (html.match(/boq_assistant-bard-web-server_[^"']+/)) ?
                html.match(/boq_assistant-bard-web-server_[^"']+/)[0] : null;
            return { snlm0e: snlm0e, sid: sid, bl: bl };
        })()
    "#;
    let html_match = sessions.execute_js(session_id, html_js).await?;
    if let serde_json::Value::Object(map) = &html_match {
        let snlm0e = map.get("snlm0e").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        let sid = map.get("sid").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        if let (Some(snlm0e), Some(sid)) = (snlm0e, sid) {
            tracing::debug!("SNlM0e and FdrFJe extracted from page HTML");
            let bl = map
                .get("bl")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| bl_param_hardcoded());
            return Ok(AuthData {
                snlm0e: snlm0e.to_string(),
                sid: sid.to_string(),
                bl,
            });
        }
    }

    // Check current page URL for diagnostics
    let url_js = "window.location.href";
    let current_url = sessions.execute_js(session_id, url_js).await?;
    let url_str = current_url.as_str().unwrap_or("unknown");

    Err(GatewayError::Auth(format!(
        "Gemini auth data not found in page. \
         Current URL: {url_str}. \
         Ensure the session is navigated to https://gemini.google.com/app \
         and the page is fully loaded with valid Google cookies."
    )))
}

#[derive(Debug, Clone)]
pub struct AuthData {
    pub snlm0e: String,
    pub sid: String,
    /// BOQ web server version (`bl` query parameter).
    pub bl: String,
}

/// Build the complete set of HTTP headers for a Gemini StreamGenerate request.
///
/// Modeled on the working gpt4free Gemini provider headers. The cookie header
/// is included explicitly because reqwest does not manage cookies from the
/// Obscura jar automatically.
pub fn build_request_headers(
    cookie_header: &str,
    user_agent: &str,
    model_header: Option<&str>,
) -> Result<HeaderMap, GatewayError> {
    let mut headers = HeaderMap::new();

    headers.insert(
        HeaderName::from_static("authority"),
        HeaderValue::from_static("gemini.google.com"),
    );

    headers.insert(
        ACCEPT,
        HeaderValue::from_static("*/*"),
    );

    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("en-US,en;q=0.9"),
    );

    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded;charset=UTF-8"),
    );

    headers.insert(
        HeaderName::from_static("origin"),
        HeaderValue::from_static("https://gemini.google.com"),
    );

    headers.insert(
        HeaderName::from_static("referer"),
        HeaderValue::from_static("https://gemini.google.com/"),
    );

    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(user_agent)
            .unwrap_or_else(|_| HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")),
    );

    headers.insert(
        HeaderName::from_static("x-same-domain"),
        HeaderValue::from_static("1"),
    );

    if let Some(header) = model_header {
        headers.insert(
            HeaderName::from_static("x-goog-ext-525001261-jspb"),
            HeaderValue::from_str(header).map_err(|e| {
                GatewayError::Internal(format!("invalid model header: {e}"))
            })?,
        );
    }

    if !cookie_header.is_empty() {
        headers.insert(
            HeaderName::from_static("cookie"),
            HeaderValue::from_str(cookie_header).map_err(|e| {
                GatewayError::Internal(format!("invalid cookie header: {e}"))
            })?,
        );
    }

    Ok(headers)
}

/// Navigate the session page to Gemini and ensure it has loaded.
pub async fn navigate_to_gemini(sessions: &SessionManager, session_id: &str) -> Result<(), GatewayError> {
    info!(session_id = %session_id, "Navigating session to Gemini");

    sessions
        .navigate(session_id, "https://gemini.google.com/app")
        .await
        .map_err(|e| {
            warn!(session_id = %session_id, error = %e, "Failed to navigate to Gemini");
            e
        })?;

    // Give the page JS a moment to initialize WIZ_global_data
    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;

    // Verify where we landed
    let url_js = "window.location.href";
    if let Ok(current_url) = sessions.execute_js(session_id, url_js).await {
        if let Some(url_str) = current_url.as_str() {
            info!(session_id = %session_id, url = %url_str, "Gemini page loaded");
            if !url_str.contains("gemini.google.com") {
                warn!(
                    session_id = %session_id,
                    url = %url_str,
                    "Session redirected away from Gemini - Google auth may be required"
                );
            }
        }
    }

    Ok(())
}

/// Return the hardcoded default BOQ web server version.
///
/// Used when the `bl` parameter cannot be extracted from the Gemini page.
/// This value rarely changes, but extracting it dynamically is preferred
/// for robustness.
pub fn bl_param_hardcoded() -> String {
    "boq_assistant-bard-web-server_20240519.16_p0".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_headers_include_cookies() {
        let cookie = "SAPISID=test; __Secure-1PSID=abc";
        let headers = build_request_headers(cookie, "Mozilla/5.0", Some("[1]")).unwrap();
        assert!(headers.contains_key("authority"));
        assert!(headers.contains_key("origin"));
        assert!(headers.contains_key("x-goog-ext-525001261-jspb"));
        assert!(headers.contains_key("cookie"));
    }

    #[test]
    fn build_headers_with_empty_cookies() {
        let headers = build_request_headers("", "Mozilla/5.0", None).unwrap();
        assert!(!headers.contains_key("cookie"));
    }
}
