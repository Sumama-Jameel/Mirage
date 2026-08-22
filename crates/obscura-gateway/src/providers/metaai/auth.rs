//! Meta AI Ecto-auth helpers.
//!
//! The Ecto-era meta.ai WebSocket transport needs two pieces of native auth
//! state, both of which come from the user's real browser session (no
//! anonymous/temp-user flow):
//!
//! 1. **Session cookies** from the browser profile's cookie jar. The DGW
//!    handshake and the GraphQL warmup/mode-switch calls are cookie-authed;
//!    `ecto_1_sess` (the renamed `abra_sess`) plus `datr` are required.
//! 2. **The `ecto1:` WebSocket token**, a server-injected page-state value
//!    (the RSC `accessToken` prop). It is present in the raw homepage HTML
//!    only when the page is logged in, and no GraphQL query returns it. It is
//!    extracted from the warmed page via `execute_js` after navigating to
//!    meta.ai, or supplied directly through the `META_AI_ECTO1_TOKEN`
//!    environment variable (the same override pattern as Grok's challenge
//!    constants and Minimax's JWT).

use obscura_net::CookieInfo;

use crate::error::GatewayError;
use crate::session::SessionHandle;

const HOME_URL: &str = "https://www.meta.ai/";

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:140.12) Gecko/20100101 Firefox/140.12";

const META_AI_COOKIE_DOMAINS: &[&str] = &["meta.ai"];

/// Matches the `/__rd_verify_<token>` path in the 403 challenge page body.
const CHALLENGE_RE: &str = r#"/__rd_verify_[A-Za-z0-9_-]+"#;

/// Matches the full `ecto1:<base64url>` accessToken value in the RSC payload
/// (escaped-JSON HTML, so the literal text is `accessToken\":\"<token>`).
const TOKEN_RE: &str = r#"accessToken\\":\\"(ecto1:[^\\"]+)"#;

/// Matches a bare base64url accessToken (deployments that strip the prefix).
const TOKEN_RE_BARE: &str = r#"accessToken\\":\\"([A-Za-z0-9_-]+)"#;

fn token_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(TOKEN_RE).expect("static TOKEN_RE"))
}

fn token_re_bare() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(TOKEN_RE_BARE).expect("static TOKEN_RE_BARE"))
}

fn challenge_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(CHALLENGE_RE).expect("static CHALLENGE_RE"))
}

/// Cookies required to open a DGW session. `ecto_1_sess` carries the
/// authenticated session; `datr` is the standard Meta anti-abuse cookie the
/// endpoint checks alongside it.
const REQUIRED_COOKIES: &[&str] = &["ecto_1_sess", "datr"];

pub fn extract_meta_cookies(session: &SessionHandle) -> Vec<CookieInfo> {
    let all_cookies = session.cookie_jar.get_all_cookies();

    all_cookies
        .into_iter()
        .filter(|c| {
            META_AI_COOKIE_DOMAINS
                .iter()
                .any(|d| c.domain.contains(d) || c.domain.ends_with(d))
        })
        .collect()
}

pub fn build_cookie_header(cookies: &[CookieInfo]) -> String {
    cookies
        .iter()
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ")
}

pub fn get_cookie<'a>(cookies: &'a [CookieInfo], name: &str) -> Option<&'a str> {
    cookies
        .iter()
        .find(|c| c.name == name)
        .map(|c| c.value.as_str())
}

pub fn validate_meta_session(cookies: &[CookieInfo]) -> Result<(), GatewayError> {
    if cookies.is_empty() {
        return Err(GatewayError::Auth(
            "no meta.ai cookies found. Log in to meta.ai in your browser first and re-run"
                .to_string(),
        ));
    }

    let missing: Vec<&str> = REQUIRED_COOKIES
        .iter()
        .filter(|name| get_cookie(cookies, name).is_none())
        .copied()
        .collect();
    if !missing.is_empty() {
        let names: Vec<&str> = cookies.iter().map(|c| c.name.as_str()).collect();
        return Err(GatewayError::Auth(format!(
            "meta.ai cookies missing: [{}]. Found cookies: [{}]. \
             Log in to meta.ai in your browser and re-run.",
            missing.join(", "),
            names.join(", ")
        )));
    }

    Ok(())
}

/// Extract the `ecto1:` WebSocket token.
///
/// Sources, in order:
/// 1. `META_AI_ECTO1_TOKEN` env var. This lets an operator paste the live
///    token from DevTools without a browser session (OmniRoute-style). The
///    value may be the full `ecto1:<base64url>` form or the bare token.
/// 2. A pure-HTTP fetch of the homepage, entirely outside the browser. The
///    token lives in the RSC payload of the *initial* HTML response (the
///    `accessToken` prop, which already carries the `ecto1:` prefix).
///
///    meta.ai sits behind an rd_verify gate, so this is a two-step dance:
///    a. `GET https://www.meta.ai/` returns a 403 challenge page whose body
///       contains an `__rd_verify_<token>` path.
///    b. `POST https://www.meta.ai/__rd_verify_<token>?challenge=3` sets the
///       `rd_challenge` cookie.
///    c. `GET https://www.meta.ai/` again with the solved cookie returns the
///       real 200 homepage with the RSC payload.
///
///    Doing this over HTTP (not via the browser) is deliberate: the meta.ai SPA
///    loads hundreds of scripts that trip the V8 watchdog and leave the isolate
///    unusable, so a browser navigation either hangs or never surfaces the 200.
///
/// Fail closed with a descriptive error when no source yields a token.
pub async fn extract_ecto_token(
    cookie_header: &str,
) -> Result<String, GatewayError> {
    if let Ok(value) = std::env::var("META_AI_ECTO1_TOKEN") {
        let value = value.trim().to_string();
        if !value.is_empty() {
            if value.starts_with("ecto1:") {
                return Ok(value);
            }
            return Ok(format!("ecto1:{}", value));
        }
    }

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| GatewayError::Internal(format!("meta.ai http client: {e}")))?;

    // Step (a): fetch the homepage to trigger (and observe) the rd_verify gate.
    let resp = client
        .get(HOME_URL)
        .header(reqwest::header::COOKIE, cookie_header)
        .send()
        .await
        .map_err(|e| GatewayError::Auth(format!("meta.ai homepage fetch failed: {e}")))?;
    let status = resp.status().as_u16();
    let body = resp
        .text()
        .await
        .map_err(|e| GatewayError::Auth(format!("meta.ai homepage body read failed: {e}")))?;
    tracing::info!(status = status, body_len = body.len(), "meta.ai step a homepage");

    if status == 200 {
        if let Some(token) = extract_token_from_html(&body) {
            tracing::debug!("meta.ai ecto1 token extracted from homepage HTML (no challenge)");
            return Ok(token);
        }
        return Err(GatewayError::Auth(
            "meta.ai homepage loaded but no accessToken found in the RSC payload. \
             The session may be logged out; log in to meta.ai in your browser and re-run."
                .to_string(),
        ));
    }

    // Step (b): the 403 challenge body contains an `__rd_verify_<token>` path.
    let challenge_path = challenge_re().find(&body).map(|m| m.as_str().to_string());
    let Some(challenge_path) = challenge_path else {
        return Err(GatewayError::Auth(format!(
            "meta.ai homepage returned {status} but no rd_verify challenge path was found. \
             Re-login may be required."
        )));
    };
    let solve_url = format!("https://www.meta.ai{challenge_path}?challenge=3");
    let solve_resp = client
        .post(&solve_url)
        .header(reqwest::header::COOKIE, cookie_header)
        .send()
        .await
        .map_err(|e| GatewayError::Auth(format!("meta.ai rd_verify solve failed: {e}")))?;
    let solve_status = solve_resp.status().as_u16();

    // The POST response carries the `rd_challenge` cookie that unblocks the
    // next homepage GET, plus possibly refreshed nonce/ecto cookies. It is
    // HttpOnly and set on the meta.ai domain. Forward ALL of them: the first
    // one alone can leave the session stale and the step-c GET still 403s
    // (observed in live sessions, body_len=0 / token_found=false).
    let solved_cookies = solve_resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|s| s.split(';').next())
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    tracing::info!(solve_status = solve_status, solved_cookie_count = solved_cookies.len(), "meta.ai step b solve");

    // Step (c): re-fetch the homepage with every solved cookie appended.
    // Appending (rather than replacing) is deliberate: the solve response may
    // set cookies whose names already exist in the session, and the newest
    // occurrence is what the server honors for the follow-up GET.
    let solved_cookies = if solved_cookies.is_empty() {
        cookie_header.to_string()
    } else {
        let mut merged = cookie_header.to_string();
        for name_value in &solved_cookies {
            if !merged.is_empty() {
                merged.push_str("; ");
            }
            merged.push_str(name_value);
        }
        merged
    };
    let resp = client
        .get(HOME_URL)
        .header(reqwest::header::COOKIE, solved_cookies.as_str())
        .send()
        .await
        .map_err(|e| GatewayError::Auth(format!("meta.ai homepage re-fetch failed: {e}")))?;
    let resp_status = resp.status().as_u16();
    let body = resp
        .text()
        .await
        .map_err(|e| GatewayError::Auth(format!("meta.ai homepage re-fetch body failed: {e}")))?;
    tracing::info!(resp_status = resp_status, body_len = body.len(), token_found = extract_token_from_html(&body).is_some(), "meta.ai step c re-fetch");

    match extract_token_from_html(&body) {
        Some(token) => {
            tracing::debug!("meta.ai ecto1 token extracted from homepage HTML after rd_verify");
            Ok(token)
        }
        None => Err(GatewayError::Auth(
            "meta.ai homepage loaded after rd_verify but no accessToken was found in the \
             RSC payload. The session may be logged out; log in to meta.ai in your browser \
             and re-run."
                .to_string(),
        )),
    }
}

/// Extract the `ecto1:<base64url>` token (or a bare base64url token) from the
/// RSC payload in a raw meta.ai homepage HTML body.
fn extract_token_from_html(html: &str) -> Option<String> {
    if let Some(m) = token_re().captures(html) {
        return Some(m[1].to_string());
    }
    token_re_bare()
        .captures(html)
        .map(|m| format!("ecto1:{}", &m[1]))
}

/// Extract the `/__rd_verify_<token>` path from the 403 challenge page body.
#[cfg(test)]
fn extract_challenge_path(html: &str) -> Option<String> {
    challenge_re().find(html).map(|m| m.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use obscura_net::{CookieInfo, CookieJar};

    use super::*;

    fn cookie(name: &str, value: &str, domain: &str) -> CookieInfo {
        CookieInfo {
            name: name.to_string(),
            value: value.to_string(),
            domain: domain.to_string(),
            path: "/".to_string(),
            http_only: false,
            secure: false,
            same_site: String::new(),
            expires: None,
        }
    }

    fn session_with(cookies: Vec<CookieInfo>) -> SessionHandle {
        let jar = Arc::new(CookieJar::new());
        jar.set_cookies_from_cdp(cookies);
        SessionHandle {
            id: "test-session".to_string(),
            cookie_jar: jar,
            local_storage: vec![],
            user_agent: "test-ua".to_string(),
        }
    }

    #[test]
    fn extract_meta_cookies_filters_domains() {
        let session = session_with(vec![
            cookie("ecto_1_sess", "sess1", ".meta.ai"),
            cookie("datr", "d1", "www.meta.ai"),
            cookie("sso", "x", "grok.com"),
        ]);
        let cookies = extract_meta_cookies(&session);
        assert_eq!(cookies.len(), 2);
        assert!(cookies.iter().all(|c| c.domain.contains("meta.ai")));
    }

    #[test]
    fn build_cookie_header_joins_all() {
        let cookies = vec![
            cookie("ecto_1_sess", "sess1", ".meta.ai"),
            cookie("datr", "d1", "www.meta.ai"),
        ];
        assert_eq!(
            build_cookie_header(&cookies),
            "ecto_1_sess=sess1; datr=d1"
        );
    }

    #[test]
    fn validate_meta_session_requires_required_cookies() {
        let ok = vec![
            cookie("ecto_1_sess", "sess1", ".meta.ai"),
            cookie("datr", "d1", "www.meta.ai"),
        ];
        assert!(validate_meta_session(&ok).is_ok());

        let missing_datr = vec![cookie("ecto_1_sess", "sess1", ".meta.ai")];
        assert!(validate_meta_session(&missing_datr).is_err());

        let empty: Vec<CookieInfo> = vec![];
        assert!(validate_meta_session(&empty).is_err());
    }

    /// The live meta.ai RSC payload embeds the accessToken already prefixed
    /// with `ecto1:` and base64url-encoded (not hex). The extraction regex must
    /// capture the full `ecto1:<token>` value, never just a leading hex
    /// fragment like `ec` from `ecto1:...`.
    #[test]
    fn token_extraction_matches_live_prefixed_format() {
        // Regexes mirrored from extract_ecto_token (JS runs in-browser).
        let re_prefixed = regex_literal(TOKEN_RE);
        let re_bare = regex_literal(TOKEN_RE_BARE);

        // Live RSC payload shape: \"accessToken\":\"ecto1:Q8yEDAFNhwA...\"
        let html = r#"\"accessToken\":\"ecto1:Q8yEDAFNhwA78vvszBK7ZGo4XUGO_VRIwPF00AJMoxapMcJFqJph4hpNhSwOeUPI7D3LIdZbeb2B6RkdbfHnjvbvPNA6SUISbkoGgGlIzPFsOJyYy9J-tRdVU1-NFpH-a0bnoFREqRjLigzf5pHQWdPySbuFFnH7fXmFmBie8docTpaaYoArpoKMl4knfhAQ15YLP5L8pM7mVFELT-wPPEOw-zp4nlRWsxqyTuXAAOlD7Py7o-Jvb__cjgNyD-yklVWE8HCU85bJ5py6qCTmERDuQ7fJfH9BJyOJEfsyU8FqCsD75QkeZrBM440\""#;

        let m = re_prefixed.captures(html).expect("prefixed match");
        let token = &m[1];
        assert!(token.starts_with("ecto1:"), "got {token}");
        assert!(token.len() > 40, "token truncated: {token}");

        // The old hex-only pattern would have captured just "ec".
        let old = regex_literal(r#"accessToken\\":\\"([0-9a-fA-F]+)"#);
        let m_old = old.captures(html).unwrap();
        assert_ne!(&m_old[1], token, "old regex must not win");

        // Bare (unprefixed) fallback still works for stripped deployments.
        let bare_html = r#"\"accessToken\":\"Q8yEDAFNhwA78vvszBK7ZGo4XUGO_VRIwPF00AJMoxapMcJFqJph4hpNhSwOeUPI7D3LIdZbeb2B6RkdbfHnjvbvPNA6SUISbkoGgGlIzPFsOJyYy9J-tRdVU1-NFpH-a0bnoFREqRjLigzf5pHQWdPySbuFFnH7fXmFmBie8docTpaaYoArpoKMl4knfhAQ15YLP5L8pM7mVFELT-wPPEOw-zp4nlRWsxqyTuXAAOlD7Py7o-Jvb__cjgNyD-yklVWE8HCU85bJ5py6qCTmERDuQ7fJfH9BJyOJEfsyU8FqCsD75QkeZrBM440\""#;
        let m = re_bare.captures(bare_html).expect("bare match");
        assert!(!m[1].starts_with("ecto1:"));
        assert_eq!(m[1].len(), token.len() - "ecto1:".len());
    }

    /// Build a regex from a JS source literal (backslash sequences are the
    /// same in Rust string literals, so reuse them verbatim).
    fn regex_literal(src: &str) -> regex::Regex {
        regex::Regex::new(src).unwrap()
    }

    #[test]
    fn extract_token_from_html_handles_prefixed_and_bare() {
        let prefixed = r#"\"viewerId\":\"1275324725659799\",\"accessToken\":\"ecto1:Q8yEDAFNhwA78vvszBK7ZGo4XUGO_VRIwPF00AJMoxapMcJFqJph4hpNhSwOeUPI7D3LIdZbeb2B6RkdbfHnjvbvPNA6SUISbkoGgGlIzPFsOJyYy9J-tRdVU1-NFpH-a0bnoFREqRjLigzf5pHQWdPySbuFFnH7fXmFmBie8docTpaaYoArpoKMl4knfhAQ15YLP5L8pM7mVFELT-wPPEOw-zp4nlRWsxqyTuXAAOlD7Py7o-Jvb__cjgNyD-yklVWE8HCU85bJ5py6qCTmERDuQ7fJfH9BJyOJEfsyU8FqCsD75QkeZrBM440\","#;
        let token = extract_token_from_html(prefixed).expect("prefixed token");
        assert!(token.starts_with("ecto1:"));
        assert!(token.len() > 40);

        let bare = r#"\"accessToken\":\"Q8yEDAFNhwA78vvszBK7ZGo4XUGO_VRIwPF00AJMoxapMcJFqJph4hpNhSwOeUPI7D3LIdZbeb2B6RkdbfHnjvbvPNA6SUISbkoGgGlIzPFsOJyYy9J-tRdVU1-NFpH-a0bnoFREqRjLigzf5pHQWdPySbuFFnH7fXmFmBie8docTpaaYoArpoKMl4knfhAQ15YLP5L8pM7mVFELT-wPPEOw-zp4nlRWsxqyTuXAAOlD7Py7o-Jvb__cjgNyD-yklVWE8HCU85bJ5py6qCTmERDuQ7fJfH9BJyOJEfsyU8FqCsD75QkeZrBM440\","#;
        let token = extract_token_from_html(bare).expect("bare token");
        assert!(token.starts_with("ecto1:"));

        assert!(extract_token_from_html("no token here").is_none());
    }

    #[test]
    fn extract_challenge_path_matches_live_403_body() {
        let body = r#"<!DOCTYPE html><html><body><script>
        (function() {
          if (document.readyState !== 'loading') { executeChallenge(); }
          function executeChallenge() {
            fetch('/__rd_verify_Q_6hBQT4km6KK-vmbCR_YiRYJVexj-g5xT7lKrtfnsxC_kpIHA?challenge=3', { method: 'POST', })
            .finally(() => window.location.reload());
          }
        })();
        </script></body></html>"#;
        assert_eq!(
            extract_challenge_path(body).as_deref(),
            Some("/__rd_verify_Q_6hBQT4km6KK-vmbCR_YiRYJVexj-g5xT7lKrtfnsxC_kpIHA")
        );
        assert!(extract_challenge_path("no challenge").is_none());
    }
}
