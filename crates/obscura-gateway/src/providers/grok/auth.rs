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

pub fn build_cookie_header(cookies: &[CookieInfo]) -> String {
    cookies
        .iter()
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ")
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
