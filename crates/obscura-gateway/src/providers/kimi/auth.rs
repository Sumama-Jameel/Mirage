use base64::Engine;
use rand::Rng;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, USER_AGENT};
use serde::Deserialize;
use std::time::Duration;
use tracing::info;

use crate::auth_state::{find_cookie_value, find_local_storage};
use crate::browser::LocalStorageEntry;
use crate::error::GatewayError;
use crate::session::SessionManager;

pub struct AuthData {
    pub access_token: String,
    pub device_id: String,
}

#[derive(Deserialize)]
struct JwtClaims {
    exp: Option<u64>,
}

/// Reject an imported access token after its JWT expiry.
/// Non-JWT values are retained for compatibility with older Kimi sessions;
/// the server remains the authority for those token formats.
fn access_token_is_usable(token: &str) -> bool {
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() != 3 {
        return !token.trim().is_empty();
    }
    let Ok(payload) = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(parts[1]))
    else {
        return false;
    };
    let Ok(claims) = serde_json::from_slice::<JwtClaims>(&payload) else {
        return false;
    };
    claims.exp.map(|exp| exp > chrono::Utc::now().timestamp() as u64).unwrap_or(true)
}

/// Web app URL. The live web client runs on kimi.ai (cookies and localStorage
/// live here); kimi.com is an older surface kept as a fallback origin.
const KIMI_WEB_URL: &str = "https://www.kimi.ai";
/// All origins that may hold Kimi browser credentials, most-recent first.
const KIMI_ORIGINS: &[&str] = &[
    "https://www.kimi.ai",
    "https://kimi.ai",
    "https://www.kimi.com",
    "https://kimi.com",
];
/// Cookie domains that may hold the kimi-auth session cookie.
const KIMI_COOKIE_DOMAINS: &[&str] = &["www.kimi.ai", ".kimi.ai", "www.kimi.com", ".www.kimi.com"];
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
        let cookie_value = KIMI_COOKIE_DOMAINS.iter().find_map(|d| find_cookie_value(jar, d, "kimi-auth"));
        if let Some(cookie_value) = cookie_value {
            if !access_token_is_usable(&cookie_value) {
                info!("Ignoring expired Kimi auth cookie; refreshing from the live page");
            } else {
                let device_id = format!("{:016}", rand::thread_rng().gen_range(0..10_000_000_000_000_000_u64));
                info!("Kimi auth extracted from kimi-auth cookie");
                return Some(AuthData { access_token: cookie_value, device_id });
            }
        }
    }

    // Fallback: only an access token is valid in the API Authorization header.
    // kimi.ai is checked first (current web app), then older origins.
    for origin in KIMI_ORIGINS {
        if let Some(token) = find_local_storage(local_storage, origin, "access_token") {
            if !access_token_is_usable(&token) {
                info!(origin, "Ignoring expired Kimi local-storage access token; refreshing from the live page");
                return None;
            }
            let device_id = format!("{:016}", rand::thread_rng().gen_range(0..10_000_000_000_000_000_u64));
            info!(origin, "Kimi auth extracted from imported localStorage");
            return Some(AuthData { access_token: token, device_id });
        }
    }

    None
}

/// Refresh endpoints, most-recent first (auth.kimi.ai serves kimi.ai
/// accounts; auth.kimi.com remains for older sessions).
const KIMI_REFRESH_URLS: &[&str] = &[
    "https://auth.kimi.ai/api/account.gateway.v1.AuthService/RefreshToken",
    "https://auth.kimi.com/api/account.gateway.v1.AuthService/RefreshToken",
];

/// Exchange an imported refresh token for a fresh access token, calling the
/// same Connect endpoint the web client uses — server-side, without needing
/// the page. Used when the copied session's access token is expired and the
/// live page treats the automation profile as logged out (the login panel
/// wipes localStorage before page JS can rotate the token).
pub async fn refresh_access_token_via_api(
    refresh_token: &str,
    device_id: &str,
) -> Result<AuthData, GatewayError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| GatewayError::Internal(format!("client build failed: {e}")))?;
    let mut last_err: Option<GatewayError> = None;
    for url in KIMI_REFRESH_URLS {
        let resp = match client
            .post(*url)
            .header("content-type", "application/json")
            .header("x-msh-platform", "web")
            .header("x-language", "en-US")
            .json(&serde_json::json!({ "refreshToken": refresh_token }))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(GatewayError::Auth(format!("Kimi token refresh failed: {e}")));
                continue;
            }
        };
        if !resp.status().is_success() {
            tracing::debug!(url, status = resp.status().as_u16(), "Kimi refresh host rejected");
            last_err = Some(GatewayError::Auth(format!(
                "Kimi token refresh returned {}",
                resp.status()
            )));
            continue;
        }
        let data: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                last_err = Some(GatewayError::Auth(format!("Kimi token refresh decode failed: {e}")));
                continue;
            }
        };
        let access = data
            .get("accessToken")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        if let Some(access) = access {
            if !access_token_is_usable(access) {
                return Err(GatewayError::Auth(
                    "Kimi refresh returned an unusable access token".to_string(),
                ));
            }
            info!(url, "Kimi access token refreshed via imported refresh token");
            return Ok(AuthData {
                access_token: access.to_string(),
                device_id: device_id.to_string(),
            });
        }
        last_err = Some(GatewayError::Auth(
            "Kimi token refresh missing accessToken".to_string(),
        ));
    }
    Err(last_err.unwrap_or_else(|| GatewayError::Auth("Kimi token refresh failed".to_string())))
}

/// Try every known Kimi origin for a stored refresh token.
pub fn find_refresh_token_in_import(local_storage: &[LocalStorageEntry]) -> Option<String> {
    for origin in KIMI_ORIGINS {
        if let Some(rt) = find_local_storage(local_storage, origin, "refresh_token") {
            if !rt.is_empty() {
                return Some(rt);
            }
        }
    }
    None
}

/// Navigate the session to www.kimi.ai and wait for load.
pub async fn navigate_to_kimi(sessions: &SessionManager, session_id: &str) -> Result<(), GatewayError> {
    info!(session_id = %session_id, url = %KIMI_WEB_URL, "Navigating session to Kimi");
    sessions.navigate(session_id, KIMI_WEB_URL).await?;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    Ok(())
}

/// Extract or refresh Kimi auth from www.kimi.com (live, after navigation).
pub async fn extract_refresh_token(
    sessions: &SessionManager,
    session_id: &str,
    cookie_jar: Option<&obscura_net::CookieJar>,
) -> Result<AuthData, GatewayError> {
    let js = r#"
        (async function() {
            try {
                const at = localStorage.getItem('access_token');
                const rt = localStorage.getItem('refresh_token');
                // This is the same Connect endpoint and field spelling used by
                // the current Kimi web client. Never send the refresh token as
                // an API bearer token.
                let accessExpired = false;
                if (at) {
                    try {
                        const payload = at.split('.')[1].replace(/-/g, '+').replace(/_/g, '/');
                        const claims = JSON.parse(atob(payload + '='.repeat((4 - payload.length % 4) % 4)));
                        accessExpired = Number.isFinite(claims.exp) && claims.exp <= Date.now() / 1000;
                    } catch (_) {}
                }
                if ((accessExpired || !at) && rt) {
                    const response = await fetch(
                        'https://auth.kimi.ai/api/account.gateway.v1.AuthService/RefreshToken',
                        {
                            method: 'POST',
                            headers: {
                                'content-type': 'application/json',
                                'x-msh-platform': 'web',
                                'x-language': 'en-US'
                            },
                            body: JSON.stringify({ refreshToken: rt })
                        }
                    );
                    if (response.ok) {
                        const data = await response.json();
                        if (data.accessToken && data.refreshToken) {
                            localStorage.setItem('access_token', data.accessToken);
                            localStorage.setItem('refresh_token', data.refreshToken);
                            return { accessToken: data.accessToken };
                        }
                    }
                }
                if (at) return { accessToken: at };
                return { accessToken: null };
            } catch (e) {
                return { accessToken: null, error: e.message };
            }
        })()
    "#;

    // Run the page refresh path before consulting the imported cookie. The
    // imported cookie can have a future JWT exp while the server has already
    // invalidated that browser snapshot.
    let value = sessions.execute_js(session_id, js).await?;
    if let serde_json::Value::Object(map) = &value {
        let token = map.get("accessToken").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        if let Some(token) = token {
            if !access_token_is_usable(token) {
                return Err(GatewayError::Auth(
                    "Kimi page returned an expired access token; log in again at https://www.kimi.ai".to_string(),
                ));
            }
            info!(session_id = %session_id, "Kimi auth extracted from live page");
            let device_id = format!("{:016}", rand::thread_rng().gen_range(0..10_000_000_000_000_000_u64));
            return Ok(AuthData {
                access_token: token.to_string(),
                device_id,
            });
        }
    }

    // The page may keep auth in an HttpOnly cookie, so JavaScript cannot read
    // it. Use the cookie only after localStorage refresh has had a chance to
    // rotate the credentials.
    if let Some(jar) = cookie_jar {
        let cookie_value = KIMI_COOKIE_DOMAINS.iter().find_map(|d| find_cookie_value(jar, d, "kimi-auth"));
        if let Some(cookie_value) = cookie_value {
            if access_token_is_usable(&cookie_value) {
                info!(session_id = %session_id, "Kimi auth found in refreshed session cookie");
                let device_id = format!("{:016}", rand::thread_rng().gen_range(0..10_000_000_000_000_000_u64));
                return Ok(AuthData { access_token: cookie_value, device_id });
            }
        }
    }

    Err(GatewayError::Auth(format!(
        "Kimi auth token not found. \
         Ensure the session is navigated to https://www.kimi.ai \
         and the page is fully loaded with a valid login session."
    )))
}

/// Build HTTP headers for Kimi API requests. The API lives on kimi.moonshot.cn
/// even though the web app is at www.kimi.ai.
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
    fn extract_from_import_does_not_use_refresh_token_as_access_token() {
        let ls = vec![LocalStorageEntry {
            origin: "https://www.kimi.com".to_string(),
            key: "refresh_token".to_string(),
            value: "test-refresh-jwt".to_string(),
        }];
        let result = extract_from_import(&ls, None);
        assert!(result.is_none());
    }

    #[test]
    fn extract_from_import_returns_none_when_missing() {
        let ls = vec![];
        assert!(extract_from_import(&ls, None).is_none());
    }
}
