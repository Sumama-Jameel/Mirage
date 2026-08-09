use rand::Rng;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, USER_AGENT};
use tracing::info;

use crate::auth_state::{find_cookie_value, find_local_storage};
use crate::browser::LocalStorageEntry;
use crate::error::GatewayError;
use crate::session::SessionManager;

pub struct AuthData {
    pub access_token: String,
    pub device_id: String,
}

/// Web app URL (cookies and localStorage live here, not on kimi.moonshot.cn).
const KIMI_WEB_URL: &str = "https://www.kimi.com";
/// API base URL (the API stays on kimi.moonshot.cn even though the web app redirects).
pub const KIMI_API_URL: &str = "https://kimi.moonshot.cn";

/// Attempt to extract auth data from imported browser data.
///
/// Priority:
/// 1. `kimi-auth` cookie (most likely fresh — synced from active browser session)
/// 2. `access_token` / `refresh_token` from localStorage (might be stale)
pub fn extract_from_import(
    local_storage: &[LocalStorageEntry],
    cookie_jar: Option<&obscura_net::CookieJar>,
) -> Option<AuthData> {
    // Try kimi-auth cookie first — it's synced from the active browser session
    // and is more likely to be fresh than a stored localStorage token.
    if let Some(jar) = cookie_jar {
        if let Some(cookie_value) = find_cookie_value(jar, "www.kimi.com", "kimi-auth")
            .or_else(|| find_cookie_value(jar, ".www.kimi.com", "kimi-auth"))
        {
            let device_id = format!("{:016}", rand::thread_rng().gen_range(0..10_000_000_000_000_000_u64));
            info!("Kimi auth extracted from kimi-auth cookie");
            return Some(AuthData { access_token: cookie_value, device_id });
        }
    }

    // Fallback: access_token or refresh_token from imported localStorage
    if let Some(token) = find_local_storage(local_storage, "https://www.kimi.com", "access_token")
        .or_else(|| find_local_storage(local_storage, "https://www.kimi.com", "refresh_token"))
    {
        let device_id = format!("{:016}", rand::thread_rng().gen_range(0..10_000_000_000_000_000_u64));
        info!("Kimi auth extracted from imported localStorage");
        return Some(AuthData { access_token: token, device_id });
    }

    None
}

/// Navigate the session to www.kimi.com and wait for load.
pub async fn navigate_to_kimi(sessions: &SessionManager, session_id: &str) -> Result<(), GatewayError> {
    info!(session_id = %session_id, url = %KIMI_WEB_URL, "Navigating session to Kimi");
    sessions.navigate(session_id, KIMI_WEB_URL).await?;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    Ok(())
}

/// Extract the refresh_token from www.kimi.com localStorage (live, after navigation).
pub async fn extract_refresh_token(
    sessions: &SessionManager,
    session_id: &str,
) -> Result<AuthData, GatewayError> {
    let js = r#"
        (async function() {
            try {
                const at = localStorage.getItem('access_token');
                if (at) return { refreshToken: at };
                const rt = localStorage.getItem('refresh_token');
                if (rt) return { refreshToken: rt };
                return { refreshToken: null };
            } catch (e) {
                return { refreshToken: null, error: e.message };
            }
        })()
    "#;
    let value = sessions.execute_js(session_id, js).await?;
    if let serde_json::Value::Object(map) = &value {
        let token = map.get("refreshToken").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        if let Some(token) = token {
            info!(session_id = %session_id, "Kimi auth extracted from live page");
            let device_id = format!("{:016}", rand::thread_rng().gen_range(0..10_000_000_000_000_000_u64));
            return Ok(AuthData {
                access_token: token.to_string(),
                device_id,
            });
        }
    }

    Err(GatewayError::Auth(format!(
        "Kimi auth token not found. \
         Ensure the session is navigated to https://www.kimi.com \
         and the page is fully loaded with a valid login session."
    )))
}

/// Build HTTP headers for Kimi API requests. The API lives on kimi.moonshot.cn
/// even though the web app is at www.kimi.com.
pub fn build_request_headers(
    access_token: &str,
    device_id: &str,
) -> Result<HeaderMap, GatewayError> {
    let mut headers = HeaderMap::new();

    headers.insert(
        HeaderName::from_static("authority"),
        HeaderValue::from_static("kimi.moonshot.cn"),
    );

    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));

    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("en-US,en;q=0.9"),
    );

    headers.insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_str(&format!("Bearer {}", access_token))
            .map_err(|e| GatewayError::Internal(format!("invalid access token: {e}")))?,
    );

    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );

    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"),
    );

    headers.insert(
        HeaderName::from_static("origin"),
        HeaderValue::from_static("https://kimi.moonshot.cn"),
    );

    headers.insert(
        HeaderName::from_static("referer"),
        HeaderValue::from_static("https://kimi.moonshot.cn/"),
    );

    headers.insert(
        HeaderName::from_static("x-msh-device-id"),
        HeaderValue::from_str(device_id)
            .map_err(|e| GatewayError::Internal(format!("invalid device id: {e}")))?,
    );

    headers.insert(
        HeaderName::from_static("x-msh-platform"),
        HeaderValue::from_static("web"),
    );

    headers.insert(
        HeaderName::from_static("r-timezone"),
        HeaderValue::from_static("Asia/Shanghai"),
    );

    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::LocalStorageEntry;

    #[test]
    fn extract_from_import_finds_access_token() {
        let ls = vec![LocalStorageEntry {
            origin: "https://www.kimi.com".to_string(),
            key: "access_token".to_string(),
            value: "test-jwt-token".to_string(),
        }];
        let result = extract_from_import(&ls, None);
        assert!(result.is_some());
        assert_eq!(result.unwrap().access_token, "test-jwt-token");
    }

    #[test]
    fn extract_from_import_falls_back_to_refresh_token() {
        let ls = vec![LocalStorageEntry {
            origin: "https://www.kimi.com".to_string(),
            key: "refresh_token".to_string(),
            value: "test-refresh-jwt".to_string(),
        }];
        let result = extract_from_import(&ls, None);
        assert!(result.is_some());
        assert_eq!(result.unwrap().access_token, "test-refresh-jwt");
    }

    #[test]
    fn extract_from_import_returns_none_when_missing() {
        let ls = vec![];
        assert!(extract_from_import(&ls, None).is_none());
    }
}
