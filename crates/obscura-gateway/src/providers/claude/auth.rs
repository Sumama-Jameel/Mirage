use reqwest::header::{HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use tracing::info;

use crate::auth_state::find_cookies_by_domain;
use crate::error::GatewayError;
use crate::session::SessionManager;

pub struct AuthData {
    #[allow(dead_code)]
    pub session_key: String,
    pub org_id: String,
    #[allow(dead_code)]
    pub cf_clearance: String,
    pub cookie_header: String,
}

pub const CLAUDE_URL: &str = "https://claude.ai";

const REQUIRED_COOKIE_NAMES: &[&str] = &["sessionKey", "lastActiveOrg"];

/// Extract auth data from imported browser cookies.
///
/// Returns `Some` if `sessionKey` is present (org_id may be empty, in which
/// case the caller should discover it from the live page). Returns `None`
/// only when no session is found, in which case the provider fails closed
/// with an auth error instead of falling back to a third-party proxy.
pub fn extract_from_import(
    cookie_jar: &obscura_net::CookieJar,
) -> Option<AuthData> {
    let cookies = find_cookies_by_domain(cookie_jar, "claude.ai", REQUIRED_COOKIE_NAMES);
    let session_key = cookies.get("sessionKey")?;
    let org_id = cookies.get("lastActiveOrg").cloned().unwrap_or_default();

    let cf_clearance = find_cookies_by_domain(cookie_jar, "claude.ai", &["cf_clearance"])
        .get("cf_clearance")
        .cloned()
        .unwrap_or_default();

    let cookie_str = format!(
        "sessionKey={}; lastActiveOrg={}; cf_clearance={}",
        session_key, org_id, cf_clearance
    );

    if org_id.is_empty() {
        info!("Claude sessionKey found but lastActiveOrg missing; will discover org_id from live page");
    } else {
        info!("Claude auth extracted from imported cookies (direct mode)");
    }
    Some(AuthData {
        session_key: session_key.clone(),
        org_id,
        cf_clearance,
        cookie_header: cookie_str,
    })
}

/// Discover the org_id by making a direct HTTP request to the Claude
/// organizations API, using the session cookies from the browser import.
/// Avoids browser navigation entirely.
pub async fn discover_org_id(
    auth_data: &AuthData,
) -> Result<String, GatewayError> {
    info!("Discovering Claude org_id via direct HTTP request");

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| GatewayError::Internal(format!("failed to build HTTP client: {e}")))?;

    let resp = http
        .get("https://claude.ai/api/organizations")
        .header("Cookie", &auth_data.cookie_header)
        .header("Accept", "application/json")
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64; rv:140.12) Gecko/20100101 Firefox/140.12")
        .header("Origin", "https://claude.ai")
        .header("Referer", "https://claude.ai/")
        .send()
        .await
        .map_err(|e| GatewayError::Provider(format!("Claude org API request failed: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(GatewayError::Auth(format!(
            "Claude organizations API returned {}: {}",
            status,
            body.chars().take(200).collect::<String>()
        )));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| GatewayError::Provider(format!("Claude org API parse failed: {e}")))?;

    let orgs = body.as_array().ok_or_else(|| {
        GatewayError::Auth("Claude organizations API returned non-array".to_string())
    })?;

    if let Some(org) = orgs.first() {
        let org_id = org.get("uuid")
            .or_else(|| org.get("id"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                GatewayError::Auth("Claude org entry has no uuid/id field".to_string())
            })?;
        info!(org_id = %org_id, "Discovered Claude org_id via organizations API");
        return Ok(org_id.to_string());
    }

    Err(GatewayError::Auth("Claude organizations API returned empty list".to_string()))
}

/// Navigate the session to claude.ai and wait for load.
#[allow(dead_code)]
pub async fn navigate_to_claude(sessions: &SessionManager, session_id: &str) -> Result<(), GatewayError> {
    info!(session_id = %session_id, url = %CLAUDE_URL, "Navigating session to Claude");
    sessions.navigate(session_id, CLAUDE_URL).await?;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    Ok(())
}

/// Extract authentication cookies from the claude.ai page.
///
/// Required cookies:
/// - `sessionKey`: the session authentication token (sk-ant-sid02-...)
/// - `lastActiveOrg`: the organization UUID
/// - `cf_clearance`: Cloudflare clearance cookie (TLS bypass proof)
#[allow(dead_code)]
pub async fn extract_auth_data(
    sessions: &SessionManager,
    session_id: &str,
) -> Result<AuthData, GatewayError> {
    let js = r#"
        (async function() {
            try {
                const cookies = document.cookie || '';
                const parts = cookies.split(';').map(c => c.trim());
                const map = {};
                for (const p of parts) {
                    const eq = p.indexOf('=');
                    if (eq > 0) {
                        map[p.substring(0, eq)] = p.substring(eq + 1);
                    }
                }
                return {
                    sessionKey: map['sessionKey'] || null,
                    lastActiveOrg: map['lastActiveOrg'] || null,
                    cf_clearance: map['cf_clearance'] || null,
                };
            } catch (e) {
                return { sessionKey: null, lastActiveOrg: null, cf_clearance: null, error: e.message };
            }
        })()
    "#;
    let value = sessions.execute_js(session_id, js).await?;
    if let serde_json::Value::Object(map) = &value {
        let session_key = map.get("sessionKey").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        let org_id = map.get("lastActiveOrg").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        let cf_clearance = map.get("cf_clearance").and_then(|v| v.as_str()).filter(|s| !s.is_empty());

        if let (Some(sk), Some(org)) = (session_key, org_id) {
            let cookie_str = format!(
                "sessionKey={}; lastActiveOrg={}; cf_clearance={}",
                sk,
                org,
                cf_clearance.unwrap_or("")
            );
            info!(session_id = %session_id, "Claude auth data extracted from live page");
            return Ok(AuthData {
                session_key: sk.to_string(),
                org_id: org.to_string(),
                cf_clearance: cf_clearance.unwrap_or("").to_string(),
                cookie_header: cookie_str,
            });
        }
    }

    Err(GatewayError::Auth(format!(
        "Claude auth data not found. \
         Ensure the session is navigated to https://claude.ai \
         with a valid login session. Required: sessionKey and lastActiveOrg cookies."
    )))
}

/// Build HTTP headers for Claude API requests.
/// Uses cookies for authentication (no Bearer token for claude.ai internal API).
pub fn build_request_headers(auth: &AuthData) -> Result<HeaderMap, GatewayError> {
    let mut headers = HeaderMap::new();

    headers.insert(
        HeaderName::from_static("accept"),
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36",
        ),
    );
    headers.insert(
        HeaderName::from_static("origin"),
        HeaderValue::from_static("https://claude.ai"),
    );
    headers.insert(
        HeaderName::from_static("referer"),
        HeaderValue::from_static("https://claude.ai/"),
    );
    headers.insert(
        HeaderName::from_static("cookie"),
        HeaderValue::from_str(&auth.cookie_header)
            .map_err(|e| GatewayError::Internal(format!("invalid cookie: {e}")))?,
    );

    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_headers_include_cookie() {
        let auth = AuthData {
            session_key: "sk-ant-sid02-test".to_string(),
            org_id: "org-123".to_string(),
            cf_clearance: "cf_test".to_string(),
            cookie_header: "sessionKey=sk-ant-sid02-test; lastActiveOrg=org-123; cf_clearance=cf_test".to_string(),
        };
        let headers = build_request_headers(&auth).unwrap();
        assert!(headers.contains_key("cookie"));
        let cookie = headers.get("cookie").unwrap().to_str().unwrap();
        assert!(cookie.contains("sessionKey"));
        assert!(cookie.contains("lastActiveOrg"));
        // Native claude.ai headers are always present.
        assert!(headers.contains_key("origin"));
        assert!(headers.contains_key("referer"));
        assert!(headers.contains_key("user-agent"));
    }
}
