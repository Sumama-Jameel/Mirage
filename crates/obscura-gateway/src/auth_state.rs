use std::collections::HashMap;

use obscura_net::{CookieInfo, CookieJar};

use crate::browser::LocalStorageEntry;

/// Find a localStorage entry by origin and key.
pub fn find_local_storage(
    entries: &[LocalStorageEntry],
    origin: &str,
    key: &str,
) -> Option<String> {
    entries
        .iter()
        .find(|e| e.origin == origin && e.key == key)
        .map(|e| e.value.clone())
        .filter(|v| !v.is_empty())
}

/// Find a cookie by exact domain and name.
pub fn find_cookie(cookie_jar: &CookieJar, domain: &str, name: &str) -> Option<CookieInfo> {
    cookie_jar
        .get_all_cookies()
        .into_iter()
        .find(|c| c.domain == domain && c.name == name)
}

/// Find a cookie value by domain and name.
pub fn find_cookie_value(cookie_jar: &CookieJar, domain: &str, name: &str) -> Option<String> {
    find_cookie(cookie_jar, domain, name).map(|c| c.value)
}

/// Find multiple cookies by domain, returning a map of name→value.
pub fn find_cookies_by_domain(
    cookie_jar: &CookieJar,
    domain: &str,
    names: &[&str],
) -> HashMap<String, String> {
    let all = cookie_jar.get_all_cookies();
    let domain_lower = domain.trim_start_matches('.').to_lowercase();
    let mut result = HashMap::new();
    for cookie in &all {
        let cookie_domain = cookie.domain.trim_start_matches('.').to_lowercase();
        if cookie_domain == domain_lower
            || cookie.domain.to_lowercase() == domain_lower
        {
            if names.contains(&cookie.name.as_str()) {
                result.insert(cookie.name.clone(), cookie.value.clone());
            }
        }
    }
    result
}

/// Find all cookies for a domain pattern (domain or .domain).
#[allow(dead_code)]
pub fn find_all_cookies_for_domain(
    cookie_jar: &CookieJar,
    domain: &str,
) -> Vec<CookieInfo> {
    let all = cookie_jar.get_all_cookies();
    let domain_lower = domain.trim_start_matches('.').to_lowercase();
    all.into_iter()
        .filter(|c| {
            let d = c.domain.trim_start_matches('.').to_lowercase();
            d == domain_lower || c.domain.to_lowercase() == domain_lower
        })
        .collect()
}
