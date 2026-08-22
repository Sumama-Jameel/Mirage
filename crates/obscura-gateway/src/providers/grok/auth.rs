use obscura_net::CookieInfo;

use crate::error::GatewayError;
use crate::session::SessionHandle;

const GROK_COOKIE_DOMAINS: &[&str] = &["grok.com", "x.ai"];

pub fn extract_grok_cookies(session: &SessionHandle) -> Vec<CookieInfo> {
    let all_cookies = session.cookie_jar.get_all_cookies();

    all_cookies
        .into_iter()
        .filter(|c| {
            GROK_COOKIE_DOMAINS
                .iter()
                .any(|d| c.domain.contains(d) || c.domain.ends_with(d))
        })
        .collect()
}

/// Build a cookie jar containing only the cookies Grok's anti-bot requires.
///
/// The full profile jar also carries a `cf_clearance` cookie issued to the
/// Firefox browser that solved the challenge; replaying it from a different
/// TLS fingerprint (our Chrome-emulated stealth client) is a pinned-cookie
/// mismatch that grok.com's anti-bot rejects (OmniRoute documents this exact
/// failure). OmniRoute's GrokWebExecutor forwards exactly `sso` + `sso-rw`,
/// which grok.com's anti-bot requires as a pair.
pub fn filtered_grok_jar(session: &SessionHandle) -> std::sync::Arc<obscura_net::CookieJar> {
    const ALLOWED: &[&str] = &["sso", "sso-rw"];
    let jar = obscura_net::CookieJar::new();
    let filtered: Vec<CookieInfo> = session
        .cookie_jar
        .get_all_cookies()
        .into_iter()
        .filter(|c| ALLOWED.contains(&c.name.as_str()))
        .collect();
    jar.set_cookies_from_cdp(filtered);
    std::sync::Arc::new(jar)
}

pub fn get_cookie<'a>(cookies: &'a [CookieInfo], name: &str) -> Option<&'a str> {
    cookies.iter().find(|c| c.name == name).map(|c| c.value.as_str())
}

pub fn validate_grok_session(cookies: &[CookieInfo]) -> Result<(), GatewayError> {
    if cookies.is_empty() {
        return Err(GatewayError::Auth(
            "no Grok cookies found. Log in to grok.com in your browser first and re-run"
                .to_string(),
        ));
    }

    if get_cookie(cookies, "sso").is_none() {
        let names: Vec<&str> = cookies.iter().map(|c| c.name.as_str()).collect();
        return Err(GatewayError::Auth(format!(
            "Grok 'sso' cookie not found in profile. \
             Found cookies: [{}]. \
             Log in to grok.com in your browser and re-run.",
            names.join(", ")
        )));
    }

    Ok(())
}
