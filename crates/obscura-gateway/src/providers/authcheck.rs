//! Provider credential validation against the imported profile.
//!
//! Each provider declares the cookies and/or localStorage entries its direct
//! adapter requires. `check` probes the session's jar and storage and returns
//! a verdict listing exactly what is missing, so the API layer can fail fast
//! with a directed message instead of burning a browser session on a provider
//! the user is not logged into. Verdicts are cached briefly because the
//! underlying jar never changes mid-process.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::auth_state::find_cookies_by_domain;
use crate::browser::LocalStorageEntry;

/// Description of the credentials one provider's adapter needs.
#[derive(Debug, Clone, Copy)]
pub struct AuthProbe {
    pub provider: &'static str,
    /// Cookie domains (either suffix match or exact) to probe.
    pub cookie_domains: &'static [&'static str],
    /// Every one of these must be present.
    pub required_cookies: &'static [&'static str],
    /// At least one of these must be present.
    pub any_of_cookies: &'static [&'static str],
    /// Every `(origin, key)` must be present in local storage.
    pub required_local_storage: &'static [(&'static str, &'static str)],
    /// At least one `(origin, key)` must be present in local storage.
    pub any_local_storage: &'static [(&'static str, &'static str)],
    /// Optional environment variable that satisfies auth (e.g. MINIMAX_JWT).
    pub env_var: Option<&'static str>,
}

impl AuthProbe {
    const fn none() -> AuthProbe {
        AuthProbe {
            provider: "",
            cookie_domains: &[],
            required_cookies: &[],
            any_of_cookies: &[],
            required_local_storage: &[],
            any_local_storage: &[],
            env_var: None,
        }
    }
}

/// Declarative credential requirements per provider. Kept in sync with the
/// extraction functions each adapter ships in its `auth.rs` / `upload.rs`.
pub fn probes() -> &'static [AuthProbe] {
    const PROBES: &[AuthProbe] = &[
        AuthProbe {
            provider: "chatgpt",
            cookie_domains: &["chatgpt.com"],
            // chatgpt.com splits large session tokens across `.0`/`.1` chunk
            // cookies; the unsplit legacy name is kept for older profiles.
            any_of_cookies: &[
                "__Secure-next-auth.session-token",
                "__Secure-next-auth.session-token.0",
                "__Host-next-auth.csrf-token",
            ],
            ..AuthProbe::none()
        },
        AuthProbe {
            provider: "claude",
            cookie_domains: &["claude.ai"],
            required_cookies: &["sessionKey", "lastActiveOrg"],
            ..AuthProbe::none()
        },
        AuthProbe {
            provider: "deepseek",
            required_local_storage: &[("https://chat.deepseek.com", "userToken")],
            ..AuthProbe::none()
        },
        AuthProbe {
            provider: "gemini",
            cookie_domains: &["gemini.google.com", ".google.com"],
            any_of_cookies: &["__Secure-1PSID", "SID", "HSID"],
            ..AuthProbe::none()
        },
        AuthProbe {
            provider: "glm",
            cookie_domains: &["chat.z.ai"],
            any_of_cookies: &["token"],
            any_local_storage: &[("https://chat.z.ai", "token"), ("https://chat.z.ai", "access_token")],
            ..AuthProbe::none()
        },
        AuthProbe {
            provider: "grok",
            cookie_domains: &["x.ai"],
            required_cookies: &["sso", "sso-rw"],
            ..AuthProbe::none()
        },
        AuthProbe {
            provider: "kimi",
            cookie_domains: &["kimi.ai", "www.kimi.ai", "kimi.com", "www.kimi.com"],
            any_of_cookies: &["kimi-auth"],
            any_local_storage: &[
                ("https://www.kimi.ai", "access_token"),
                ("https://www.kimi.ai", "refresh_token"),
                ("https://www.kimi.com", "access_token"),
                ("https://www.kimi.com", "refresh_token"),
            ],
            ..AuthProbe::none()
        },
        AuthProbe {
            provider: "metaai",
            cookie_domains: &["meta.ai"],
            any_of_cookies: &["ecto_1_sess", "datr"],
            ..AuthProbe::none()
        },
        AuthProbe {
            provider: "mimo",
            cookie_domains: &["xiaomimimo.com"],
            required_cookies: &["xiaomichatbot_serviceToken", "userId", "xiaomichatbot_ph"],
            ..AuthProbe::none()
        },
        AuthProbe {
            provider: "minimax",
            env_var: Some("MINIMAX_JWT"),
            any_local_storage: &[
                ("https://agent.minimax.io", "_token"),
                ("https://agent.minimax.io", "mavis:token"),
            ],
            ..AuthProbe::none()
        },
        AuthProbe {
            provider: "mistral",
            cookie_domains: &["mistral.ai"],
            any_of_cookies: &["ory_kratos_continuity"],
            ..AuthProbe::none()
        },
        AuthProbe {
            provider: "qwen",
            required_local_storage: &[("https://chat.qwen.ai", "token")],
            ..AuthProbe::none()
        },
    ];
    PROBES
}

fn probe_for(provider: &str) -> Option<&'static AuthProbe> {
    probes().iter().find(|p| p.provider == provider)
}

fn find_local_storage_entry<'a>(entries: &'a [LocalStorageEntry], origin: &str, key: &str) -> Option<&'a str> {
    entries
        .iter()
        .find(|e| e.origin == origin && e.key == key)
        .map(|e| e.value.as_str())
}

/// The imported profile credentials probed by the pre-flight check.
pub struct ImportedSnapshot {
    pub cookie_jar: obscura_net::CookieJar,
    pub local_storage: Vec<LocalStorageEntry>,
}

/// Result of a credential probe.
#[derive(Debug, Clone)]
pub struct AuthVerdict {
    pub ok: bool,
    /// Exact names that are missing, for a directed error message.
    pub missing: Vec<String>,
}

impl AuthVerdict {
    fn ok() -> AuthVerdict {
        AuthVerdict {
            ok: true,
            missing: Vec::new(),
        }
    }
}

/// Probe the imported profile for the credentials `provider` needs.
pub fn check(
    provider: &str,
    cookie_jar: &obscura_net::CookieJar,
    local_storage: &[LocalStorageEntry],
) -> AuthVerdict {
    let Some(probe) = probe_for(provider) else {
        // Providers with no declared probe are not gated by auth checks.
        return AuthVerdict::ok();
    };

    if let Some(var) = probe.env_var {
        if std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false) {
            return AuthVerdict::ok();
        }
    }

    let mut missing: Vec<String> = Vec::new();
    let mut missing_cookies = false;
    let mut missing_local_storage = false;

    // Cookie probes.
    let mut any_cookie_hits = 0usize;
    for domain in probe.cookie_domains {
        let hits = find_cookies_by_domain(cookie_jar, domain, probe.required_cookies);
        for name in probe.required_cookies {
            if !hits.contains_key(*name) {
                let label = format!("{name}@{domain}");
                if !missing.contains(&label) {
                    missing.push(label);
                }
            }
        }
        let any_hits = find_cookies_by_domain(cookie_jar, domain, probe.any_of_cookies);
        if any_hits.values().any(|v| !v.is_empty()) {
            any_cookie_hits += 1;
        }
    }
    if !probe.any_of_cookies.is_empty() && any_cookie_hits == 0 {
        missing_cookies = true;
        missing.push(format!(
            "/{} cookie on {}",
            probe.any_of_cookies.join(", "),
            probe.cookie_domains.join(", ")
        ));
    }

    // Local storage probes.
    for (origin, key) in probe.required_local_storage {
        if find_local_storage_entry(local_storage, origin, key).map(str::is_empty).unwrap_or(true) {
            missing.push(format!("{key}@localStorage {origin}"));
        }
    }
    let any_ls_hits = probe.any_local_storage.iter().any(|(origin, key)| {
        find_local_storage_entry(local_storage, origin, key)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    });
    if !probe.any_local_storage.is_empty() && !any_ls_hits {
        missing_local_storage = true;
        missing.push(format!(
            "localStorage {}/{} on {}",
            probe.any_local_storage[0].1,
            probe.any_local_storage[0].0,
            "any"
        ));
    }

    // Alternative-evidence semantics: when a probe declares both cookie and
    // localStorage groups (GLM, Kimi), satisfying EITHER proves the login.
    // Requiring both produced false negatives for browsers that keep the
    // credential in only one of the two stores.
    if !probe.any_of_cookies.is_empty() && !probe.any_local_storage.is_empty() {
        if !(missing_cookies && missing_local_storage) {
            missing.retain(|m| {
                let is_cookie_diag = m.starts_with('/');
                let is_ls_diag = m.starts_with("localStorage ");
                !(is_cookie_diag || is_ls_diag)
            });
        }
    } else {
        // Single-group probes: required_* entries still apply verbatim.
    }

    if missing.is_empty() {
        AuthVerdict::ok()
    } else {
        AuthVerdict { ok: false, missing }
    }
}

/// Cache wrapper for `check` so repeated requests in a window don't re-scan
/// the jar. The jar is immutable from the gateway's point of view, so the
/// cache only needs to guard against repeated hot-path scans.
#[derive(Clone, Default)]
pub struct CachedAuthChecker {
    inner: Arc<Mutex<HashMap<String, (Instant, AuthVerdict)>>>,
}

impl CachedAuthChecker {
    pub fn new() -> Self {
        Self::default()
    }

    pub const TTL: Duration = Duration::from_secs(5 * 60);

    pub async fn check(
        &self,
        provider: &str,
        cookie_jar: &obscura_net::CookieJar,
        local_storage: &[LocalStorageEntry],
    ) -> AuthVerdict {
        let mut map = self.inner.lock().await;
        if let Some((at, verdict)) = map.get(provider) {
            if at.elapsed() < Self::TTL {
                return verdict.clone();
            }
        }
        let verdict = check(provider, cookie_jar, local_storage);
        map.insert(provider.to_string(), (Instant::now(), verdict.clone()));
        verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use obscura_net::CookieInfo;

    fn jar_with(cookies: &[(&str, &str, &str)]) -> obscura_net::CookieJar {
        let jar = obscura_net::CookieJar::new();
        let infos: Vec<CookieInfo> = cookies
            .iter()
            .map(|(domain, name, value)| CookieInfo {
                name: name.to_string(),
                value: value.to_string(),
                domain: domain.to_string(),
                path: "/".to_string(),
                secure: true,
                http_only: false,
                same_site: "Lax".to_string(),
                expires: None,
            })
            .collect();
        jar.set_cookies_from_cdp(infos);
        jar
    }

    fn storage_with(storage: &[(&str, &str, &str)]) -> Vec<LocalStorageEntry> {
        storage
            .iter()
            .map(|(origin, key, value)| LocalStorageEntry {
                origin: origin.to_string(),
                key: key.to_string(),
                value: value.to_string(),
            })
            .collect()
    }

    #[test]
    fn chatgpt_split_session_cookie_accepted() {
        // chatgpt.com chunks the NextAuth session token across `.0`/`.1`
        // cookies; the probe must treat their presence as logged-in.
        let jar = jar_with(&[
            ("chatgpt.com", "__Secure-next-auth.session-token.0", "part0"),
            ("chatgpt.com", "__Secure-next-auth.session-token.1", "part1"),
        ]);
        assert!(check("chatgpt", &jar, &[]).ok, "split-cookie login rejected");
    }

    #[test]
    fn claude_ok_when_both_cookies_present() {        let jar = jar_with(&[("claude.ai", "sessionKey", "s"), ("claude.ai", "lastActiveOrg", "o")]);
        let verdict = check("claude", &jar, &[]);
        assert!(verdict.ok, "{:?}", verdict.missing);
    }

    #[test]
    fn claude_missing_report_is_directed() {
        let jar = jar_with(&[]);
        let verdict = check("claude", &jar, &[]);
        assert!(!verdict.ok);
        assert!(verdict.missing.join(" ").contains("sessionKey"));
    }

    #[test]
    fn deepseek_requires_local_storage_token() {
        let storage = storage_with(&[("https://chat.deepseek.com", "userToken", "t")]);
        assert!(check("deepseek", &jar_with(&[]), &storage).ok);
        assert!(!check("deepseek", &jar_with(&[]), &[]).ok);
    }

    #[test]
    fn minimax_satisfied_by_env() {
        let jar = jar_with(&[]);
        std::env::set_var("MINIMAX_JWT", "jwt-here");
        assert!(check("minimax", &jar, &[]).ok);
        std::env::remove_var("MINIMAX_JWT");
        assert!(!check("minimax", &jar, &[]).ok);
    }

    #[tokio::test]
    async fn cached_check_returns_cached_verdict() {
        let checker = CachedAuthChecker::new();
        let jar = jar_with(&[("x.ai", "sso", "a"), ("x.ai", "sso-rw", "b")]);
        assert!(checker.check("grok", &jar, &[]).await.ok);
        // A fresh empty jar still returns the cached ok.
        let empty = jar_with(&[]);
        assert!(checker.check("grok", &empty, &[]).await.ok);
    }

    #[test]
    fn qwen_ok_with_local_storage_token() {
        let storage = storage_with(&[("https://chat.qwen.ai", "token", "eyJhbGciOiJIUzI1NiJ9.eyJpZCI6InRlc3QifQ.sig")]);
        assert!(check("qwen", &jar_with(&[]), &storage).ok);
    }

    #[test]
    fn qwen_fails_without_any_auth() {
        assert!(!check("qwen", &jar_with(&[]), &[]).ok);
    }

    #[test]
    fn kimi_ok_with_kimi_auth_cookie() {
        let jar = jar_with(&[("www.kimi.com", "kimi-auth", "test-token")]);
        assert!(check("kimi", &jar, &[]).ok);
    }

    #[test]
    fn glm_passes_on_local_storage_alone() {
        // Regression: GLM declares cookie AND localStorage evidence; the
        // browser keeps the token only in localStorage. Either must suffice.
        let ls = storage_with(&[("https://chat.z.ai", "token", "jwt-value")]);
        let verdict = check("glm", &jar_with(&[]), &ls);
        assert!(verdict.ok, "localStorage-only login rejected: {:?}", verdict.missing);
    }

    #[test]
    fn glm_passes_on_cookie_alone() {
        let jar = jar_with(&[("chat.z.ai", "token", "jwt-value")]);
        let verdict = check("glm", &jar, &[]);
        assert!(verdict.ok, "cookie-only login rejected: {:?}", verdict.missing);
    }

    #[test]
    fn glm_fails_when_both_groups_missing() {
        let verdict = check("glm", &jar_with(&[]), &[]);
        assert!(!verdict.ok);
        assert!(
            verdict.missing.iter().any(|m| m.contains("cookie")),
            "should report the missing cookie group: {:?}",
            verdict.missing
        );
        assert!(
            verdict.missing.iter().any(|m| m.contains("localStorage")),
            "should report the missing localStorage group: {:?}",
            verdict.missing
        );
    }

    #[test]
    fn kimi_ai_local_storage_accepted() {
        let ls = storage_with(&[("https://www.kimi.ai", "refresh_token", "rt")]);
        assert!(check("kimi", &jar_with(&[]), &ls).ok);
    }

    #[test]
    fn kimi_fails_without_auth() {
        assert!(!check("kimi", &jar_with(&[]), &[]).ok);
    }

    #[test]
    fn mistral_ok_with_ory_cookie() {
        let jar = jar_with(&[("mistral.ai", "ory_kratos_continuity", "abc")]);
        assert!(check("mistral", &jar, &[]).ok);
    }

    #[test]
    fn mistral_fails_without_any_cookie() {
        assert!(!check("mistral", &jar_with(&[]), &[]).ok);
    }

    #[test]
    fn metaai_ok_with_ecto_session() {
        let jar = jar_with(&[("meta.ai", "ecto_1_sess", "abc"), ("meta.ai", "datr", "xyz")]);
        assert!(check("metaai", &jar, &[]).ok);
    }

    #[test]
    fn metaai_ok_with_datr_only() {
        let jar = jar_with(&[("meta.ai", "datr", "xyz")]);
        assert!(check("metaai", &jar, &[]).ok);
    }

    #[test]
    fn metaai_fails_with_wrong_cookies() {
        let jar = jar_with(&[("meta.ai", "c_user", "123"), ("meta.ai", "xs", "abc")]);
        assert!(!check("metaai", &jar, &[]).ok);
    }

    #[test]
    fn grok_ok_with_sso_on_x_ai() {
        let jar = jar_with(&[("x.ai", "sso", "a"), ("x.ai", "sso-rw", "b")]);
        assert!(check("grok", &jar, &[]).ok);
    }

    #[test]
    fn grok_fails_without_sso() {
        let jar = jar_with(&[("x.ai", "cf_clearance", "abc")]);
        assert!(!check("grok", &jar, &[]).ok);
    }

    #[test]
    fn mimo_fails_without_userid() {
        let jar = jar_with(&[
            ("xiaomimimo.com", "xiaomichatbot_serviceToken", "a"),
            ("xiaomimimo.com", "xiaomichatbot_ph", "b"),
        ]);
        let verdict = check("mimo", &jar, &[]);
        assert!(!verdict.ok);
        assert!(verdict.missing.iter().any(|m| m.contains("userId")));
    }

    #[test]
    fn leading_dot_domain_matches_cookie() {
        let jar = jar_with(&[("example.com", "session", "abc")]);
        let found = find_cookies_by_domain(&jar, ".example.com", &["session"]);
        assert!(found.contains_key("session"));
    }
}