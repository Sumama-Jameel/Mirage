//! System browser authentication import.
//!
//! Detects the user's default browser on Linux/Windows (Firefox, Chrome, Edge)
//! and extracts DeepSeek authentication state (cookies + localStorage) from a
//! safe read-only snapshot of the browser profile.

use std::path::{Path, PathBuf};

use obscura_net::CookieInfo;
use tracing::warn;

use crate::error::GatewayError;

mod chromium;
mod detect;
mod firefox;
mod firefox_nss;
mod firefox_profile;
mod firefox_sqlite;
mod snapshot;
mod version;

pub use version::{build_firefox_ua, firefox_version_or_default};

/// A single localStorage entry imported from a browser.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LocalStorageEntry {
    /// Exact origin that owns this value. localStorage is origin-scoped and
    /// must never be replayed into a different provider's page.
    pub origin: String,
    pub key: String,
    pub value: String,
}

impl LocalStorageEntry {
    /// Attempt to parse the entry as DeepSeek's `userToken` blob and return
    /// the inner bearer token value.
    pub fn as_deepseek_token(&self) -> Option<String> {
        if self.key != "userToken" {
            return None;
        }
        let parsed: serde_json::Value = serde_json::from_str(&self.value).ok()?;
        parsed
            .get("value")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }
}

/// Which browser the auth state came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserType {
    Firefox,
    Chrome,
    Edge,
}

impl std::fmt::Display for BrowserType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrowserType::Firefox => write!(f, "firefox"),
            BrowserType::Chrome => write!(f, "chrome"),
            BrowserType::Edge => write!(f, "edge"),
        }
    }
}

/// The source browser profile that was used for extraction.
#[derive(Debug, Clone)]
pub struct BrowserSource {
    pub browser_type: BrowserType,
    pub profile_path: PathBuf,
}

/// Imported DeepSeek authentication state.
#[derive(Debug, Clone)]
pub struct AuthState {
    pub cookies: Vec<CookieInfo>,
    pub local_storage: Vec<LocalStorageEntry>,
    pub source: BrowserSource,
}

/// How to select the source browser.
#[derive(Debug, Clone, Default)]
pub enum SourceSelection {
    /// Detect the default browser, falling back to any browser with DeepSeek auth.
    #[default]
    Auto,
    /// Use a specific browser type.
    Browser(BrowserType),
    /// Use an explicit profile directory.
    Profile(PathBuf),
}

/// Cookie import options.
#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// How to choose the browser/profile.
    pub source: SourceSelection,
    /// Optional JSON cookie file fallback.
    pub cookies_json_path: Option<PathBuf>,
}

/// Shared auth data imported from any browser.
#[derive(Debug, Clone)]
pub struct ImportedAuth {
    pub cookies: Vec<CookieInfo>,
    pub local_storage: Vec<LocalStorageEntry>,
}

/// Import DeepSeek authentication state from the user's browser.
///
/// 1. If `cookies_json_path` is set and exists, load JSON cookies.
/// 2. Otherwise select a browser/profile per `source`.
/// 3. Snapshot the profile files read-only.
/// 4. Extract DeepSeek cookies and localStorage.
pub fn import_auth(options: &ImportOptions) -> Result<AuthState, GatewayError> {
    if let Some(ref json_path) = options.cookies_json_path {
        if json_path.exists() {
            warn!(path = %json_path.display(), "Using JSON cookie fallback; browser import skipped");
            let cookies = read_cookies_json(json_path)?;
            return Ok(AuthState {
                cookies,
                local_storage: Vec::new(),
                source: BrowserSource {
                    browser_type: BrowserType::Firefox,
                    profile_path: json_path.into(),
                },
            });
        }
    }

    let source = select_source(&options.source)?;

    let auth = match source.browser_type {
        BrowserType::Firefox => firefox::import(&source.profile_path).map(|a| ImportedAuth {
            cookies: a.cookies,
            local_storage: a.local_storage,
        }),
        BrowserType::Chrome | BrowserType::Edge => chromium::import(&source),
    }?;

    Ok(AuthState {
        cookies: auth.cookies,
        local_storage: auth.local_storage,
        source,
    })
}

fn select_source(selection: &SourceSelection) -> Result<BrowserSource, GatewayError> {
    match selection {
        SourceSelection::Profile(path) => {
            // An explicit profile path is treated as Firefox for now. Once
            // Chromium profile structures are supported we can detect by layout.
            Ok(BrowserSource {
                browser_type: BrowserType::Firefox,
                profile_path: path.clone(),
            })
        }
        SourceSelection::Browser(browser_type) => {
            let profile = detect::find_default_profile(*browser_type)
                .ok_or_else(|| GatewayError::Internal(format!(
                    "{} profile directory not found",
                    browser_type
                )))?;
            Ok(BrowserSource {
                browser_type: *browser_type,
                profile_path: profile,
            })
        }
        SourceSelection::Auto => detect::select_source(),
    }
}

/// Cookie names that may carry the DeepSeek bearer token.
const DEEPSEEK_TOKEN_COOKIE_NAMES: &[&str] = &[
    "userToken",
    "user_token",
    "__ds_token",
    "ds_token",
    "Authorization",
    "authorization",
];

/// Returns true if the host belongs to DeepSeek.
fn is_deepseek_domain(host: &str) -> bool {
    let host = host.trim_start_matches('.').to_lowercase();
    host == "deepseek.com" || host.ends_with(".deepseek.com")
}

/// Returns true if the host belongs to Google (used for Gemini auth cookies).
fn is_google_domain(host: &str) -> bool {
    let host = host.trim_start_matches('.').to_lowercase();
    host == "google.com" || host.ends_with(".google.com")
}

/// Returns true for Moonshot/Kimi web-session hosts.
fn is_kimi_domain(host: &str) -> bool {
    let host = host.trim_start_matches('.').to_lowercase();
    host == "kimi.com" || host.ends_with(".kimi.com") || host == "moonshot.cn" || host.ends_with(".moonshot.cn")
}

/// Returns true for Z.AI's public GLM chat hosts.
fn is_zai_domain(host: &str) -> bool {
    let host = host.trim_start_matches('.').to_lowercase();
    host == "z.ai" || host.ends_with(".z.ai")
}

/// Returns true for Zhipu AI / ChatGLM web-app hosts (chatglm.cn).
fn is_chatglm_domain(host: &str) -> bool {
    let host = host.trim_start_matches('.').to_lowercase();
    host == "chatglm.cn" || host.ends_with(".chatglm.cn")
}

/// Returns true for Claude's consumer web-session hosts.
fn is_claude_domain(host: &str) -> bool {
    let host = host.trim_start_matches('.').to_lowercase();
    host == "claude.ai" || host.ends_with(".claude.ai") || host == "anthropic.com" || host.ends_with(".anthropic.com")
}

/// Returns true for Grok / xAI web-session hosts.
fn is_grok_domain(host: &str) -> bool {
    let host = host.trim_start_matches('.').to_lowercase();
    host == "grok.com" || host.ends_with(".grok.com") || host == "x.ai" || host.ends_with(".x.ai")
}

/// Returns true for Qwen / Alibaba web-session hosts.
fn is_qwen_domain(host: &str) -> bool {
    let host = host.trim_start_matches('.').to_lowercase();
    host == "qwen.ai" || host.ends_with(".qwen.ai") || host == "aliyun.com" || host.ends_with(".aliyun.com")
}

/// Returns true for ChatGPT / OpenAI web-session hosts.
fn is_chatgpt_domain(host: &str) -> bool {
    let host = host.trim_start_matches('.').to_lowercase();
    host == "chatgpt.com" || host.ends_with(".chatgpt.com")
        || host == "openai.com" || host.ends_with(".openai.com")
}

/// Returns true for Mistral Le Chat web-session hosts.
fn is_mistral_domain(host: &str) -> bool {
    let host = host.trim_start_matches('.').to_lowercase();
    host == "mistral.ai" || host.ends_with(".mistral.ai")
}

/// Returns true for Xiaomi MiMo web-session hosts (aistudio.xiaomimimo.com).
fn is_mimo_domain(host: &str) -> bool {
    let host = host.trim_start_matches('.').to_lowercase();
    host == "xiaomimimo.com"
        || host.ends_with(".xiaomimimo.com")
        || host == "xiaomi.com"
        || host.ends_with(".xiaomi.com")
}

/// Returns true for Meta AI web-session hosts (meta.ai).
fn is_metaai_domain(host: &str) -> bool {
    let host = host.trim_start_matches('.').to_lowercase();
    host == "meta.ai" || host.ends_with(".meta.ai")
}

/// Returns true if the host is relevant to any supported provider.
fn is_relevant_domain(host: &str) -> bool {
    is_deepseek_domain(host)
        || is_google_domain(host)
        || is_kimi_domain(host)
        || is_zai_domain(host)
        || is_chatglm_domain(host)
        || is_claude_domain(host)
        || is_chatgpt_domain(host)
        || is_grok_domain(host)
        || is_qwen_domain(host)
        || is_mistral_domain(host)
        || is_mimo_domain(host)
        || is_metaai_domain(host)
}

/// Returns true for AWS WAF / fingerprint-bound anti-bot cookies that should
/// not be imported across browser engines.
fn is_waf_cookie(name: &str) -> bool {
    name == "aws-waf-token" || name.starts_with(".thumbcache_")
}

/// Fallback reader for a JSON cookie file exported from a browser extension.
fn read_cookies_json(path: &Path) -> Result<Vec<CookieInfo>, GatewayError> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        GatewayError::Internal(format!("failed to read JSON cookie file: {e}"))
    })?;
    let cookies: Vec<CookieInfo> = serde_json::from_str(&content).map_err(|e| {
        GatewayError::Internal(format!("failed to parse JSON cookie file: {e}"))
    })?;
    Ok(cookies
        .into_iter()
        .filter(|c| is_relevant_domain(&c.domain))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_deepseek_domains() {
        assert!(is_deepseek_domain("deepseek.com"));
        assert!(is_deepseek_domain(".deepseek.com"));
        assert!(is_deepseek_domain("chat.deepseek.com"));
        assert!(!is_deepseek_domain("google.com"));
    }

    #[test]
    fn detects_browser_ui_provider_domains() {
        assert!(is_kimi_domain("www.kimi.com"));
        assert!(is_zai_domain("chat.z.ai"));
        assert!(is_chatglm_domain("chatglm.cn"));
        assert!(is_chatglm_domain(".chatglm.cn"));
        assert!(is_claude_domain("claude.ai"));
        assert!(!is_relevant_domain("example.com"));
    }

    #[test]
    fn detects_chatgpt_domains() {
        assert!(is_chatgpt_domain("chatgpt.com"));
        assert!(is_chatgpt_domain(".chatgpt.com"));
        assert!(is_chatgpt_domain("auth.chatgpt.com"));
        assert!(is_chatgpt_domain("openai.com"));
        assert!(is_chatgpt_domain("auth.openai.com"));
        assert!(is_chatgpt_domain("api.openai.com"));
        assert!(!is_chatgpt_domain("example.com"));
        assert!(!is_chatgpt_domain("claude.ai"));
        assert!(is_relevant_domain("chatgpt.com"));
        assert!(is_relevant_domain("openai.com"));
    }

    #[test]
    fn local_storage_extracts_deepseek_token() {
        let entry = LocalStorageEntry {
            origin: "https://chat.deepseek.com".to_string(),
            key: "userToken".to_string(),
            value: r#"{"value":"abc123","__version":"0"}"#.to_string(),
        };
        assert_eq!(entry.as_deepseek_token(), Some("abc123".to_string()));
    }

    #[test]
    fn local_storage_ignores_non_token_keys() {
        let entry = LocalStorageEntry {
            origin: "https://chat.deepseek.com".to_string(),
            key: "searchEnabled".to_string(),
            value: r#"{"value":true,"__version":"0"}"#.to_string(),
        };
        assert_eq!(entry.as_deepseek_token(), None);
    }
}
