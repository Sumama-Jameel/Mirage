use std::path::PathBuf;

use config::{Config as ConfigBuilder, File};
use serde::Deserialize;

use crate::error::GatewayError;

/// Gateway configuration.
///
/// Loaded from `obscura-gateway.toml` in the current working directory,
/// with environment overrides via `OBSCURA_GATEWAY_*` prefixes.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub browser: BrowserConfig,
    #[serde(default)]
    pub glm: GlmConfig,
    /// Directory for persistent session storage.
    /// Each provider stores its sessions in a separate JSON file inside this directory.
    /// When absent, sessions are kept only in memory (lost on restart).
    #[serde(default)]
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    /// Bearer token clients must send. Local-only default; change in production.
    pub api_key: String,
}

/// GLM provider configuration.
///
/// These values are exposed as configuration so the gateway can adapt quickly
/// when Z.AI rotates endpoints or signing secrets, without requiring a code
/// change and re-release.
#[derive(Debug, Clone, Deserialize)]
pub struct GlmConfig {
    /// Signing secret used for the dual-layer HMAC-SHA256 `X-Signature`
    /// header. Z.AI has historically used `"junjie"`; this can be rotated
    /// at runtime if that changes.
    #[serde(default = "default_glm_sign_secret")]
    pub sign_secret: String,

    /// Upstream Z.AI internal chat-completions endpoint. Defaults to the v2
    /// endpoint; v1 can be forced here if an account still uses it.
    #[serde(default = "default_glm_upstream_url")]
    pub upstream_url: String,

    /// Force the UI-automation fallback path for every GLM request.
    /// Useful for debugging or when the direct API is temporarily unstable.
    #[serde(default)]
    pub force_ui: bool,
}

fn default_glm_sign_secret() -> String {
    "junjie".to_string()
}

fn default_glm_upstream_url() -> String {
    "https://chat.z.ai/api/v2/chat/completions".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct BrowserConfig {
    /// How to select the source browser. One of: "auto", "firefox", "chrome",
    /// "edge", or an explicit profile path.
    #[serde(default = "default_browser_source")]
    pub source: String,

    /// Explicit profile path. If set, takes precedence over `source`.
    pub profile_path: Option<String>,

    /// Optional fallback JSON cookie file (e.g. exported from a browser
    /// extension). Used if automatic extraction fails.
    pub cookies_json_path: Option<String>,

    /// Browser identity the headless engine should impersonate. "firefox"
    /// matches a Firefox-on-Linux session (the default). "chrome" and "edge"
    /// select the corresponding platform fingerprint.
    pub identity: Option<String>,

    /// Optional manual UA override. When set, it takes precedence over the
    /// auto-detected browser identity.
    pub user_agent_override: Option<String>,
}

fn default_browser_source() -> String {
    "auto".to_string()
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            source: "auto".to_string(),
            profile_path: None,
            cookies_json_path: None,
            identity: None,
            user_agent_override: None,
        }
    }
}

impl Config {
    /// Load configuration from file and environment.
    ///
    /// Defaults bind to `127.0.0.1:3000` and require the local API key
    /// `obscura-local`. Firefox profile discovery is attempted automatically.
    pub fn load() -> Result<Self, GatewayError> {
        let builder = ConfigBuilder::builder()
            .set_default("server.host", "127.0.0.1")?
            .set_default("server.port", 3000i64)?
            .set_default("auth.api_key", "obscura-local")?
            .set_default("browser.source", "auto")?
            .set_default("browser.profile_path", None::<String>)?
            .set_default("browser.cookies_json_path", None::<String>)?
            .set_default("browser.identity", "firefox")?
            .set_default("browser.user_agent_override", None::<String>)?
            .set_default("glm.sign_secret", "junjie")?
            .set_default("glm.upstream_url", "https://chat.z.ai/api/v2/chat/completions")?
            .set_default("glm.force_ui", false)?
            .set_default("data_dir", None::<String>)?
            .add_source(File::with_name("obscura-gateway").required(false))
            .add_source(config::Environment::with_prefix("OBSCURA_GATEWAY").separator("__"));

        let config: Config = builder.build()?.try_deserialize()?;
        config.validate()?;
        Ok(config)
    }

    /// Validate security-sensitive configuration.
    fn validate(&self) -> Result<(), GatewayError> {
        if self.server.port == 0 {
            return Err(GatewayError::Config("server.port cannot be 0".to_string()));
        }

        // Refuse to bind to all interfaces by default. The gateway handles
        // session cookies and should not be exposed to untrusted networks
        // without explicit operator intent.
        if self.server.host == "0.0.0.0" {
            return Err(GatewayError::Config(
                "binding to 0.0.0.0 is disabled for security; use 127.0.0.1".to_string(),
            ));
        }

        if self.auth.api_key.is_empty() {
            return Err(GatewayError::Config(
                "auth.api_key cannot be empty".to_string(),
            ));
        }

        let identity = self.browser.identity.as_deref().unwrap_or("firefox");
        if identity != "firefox" && identity != "chrome" && identity != "edge" {
            return Err(GatewayError::Config(
                "browser.identity must be 'firefox', 'chrome', or 'edge'".to_string(),
            ));
        }

        let source = self.browser.source.as_str();
        if !matches!(source, "auto" | "firefox" | "chrome" | "edge") {
            return Err(GatewayError::Config(
                "browser.source must be 'auto', 'firefox', 'chrome', or 'edge'".to_string(),
            ));
        }

        if self.glm.sign_secret.is_empty() {
            return Err(GatewayError::Config(
                "glm.sign_secret cannot be empty".to_string(),
            ));
        }

        if self.glm.upstream_url.is_empty() {
            return Err(GatewayError::Config(
                "glm.upstream_url cannot be empty".to_string(),
            ));
        }

        Ok(())
    }
}

impl BrowserConfig {
    /// Resolved browser identity, defaulting to Firefox.
    #[allow(dead_code)]
    pub fn resolved_identity(&self) -> &str {
        self.identity.as_deref().unwrap_or("firefox")
    }

    /// Build import options from the config section.
    pub fn import_options(&self) -> super::browser::ImportOptions {
        let source = if let Some(ref path) = self.profile_path {
            super::browser::SourceSelection::Profile(PathBuf::from(path))
        } else {
            match self.source.as_str() {
                "firefox" => super::browser::SourceSelection::Browser(
                    super::browser::BrowserType::Firefox,
                ),
                "chrome" => super::browser::SourceSelection::Browser(
                    super::browser::BrowserType::Chrome,
                ),
                "edge" => super::browser::SourceSelection::Browser(
                    super::browser::BrowserType::Edge,
                ),
                _ => super::browser::SourceSelection::Auto,
            }
        };
        super::browser::ImportOptions {
            source,
            cookies_json_path: self.cookies_json_path.as_ref().map(PathBuf::from),
        }
    }
}

impl Default for GlmConfig {
    fn default() -> Self {
        Self {
            sign_secret: default_glm_sign_secret(),
            upstream_url: default_glm_upstream_url(),
            force_ui: false,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 3000,
            },
            auth: AuthConfig {
                api_key: "obscura-local".to_string(),
            },
            browser: BrowserConfig::default(),
            glm: GlmConfig::default(),
            data_dir: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let cfg = Config::default();
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.server.host, "127.0.0.1");
        assert_eq!(cfg.server.port, 3000);
        assert_eq!(cfg.browser.source, "auto");
        assert_eq!(cfg.browser.resolved_identity(), "firefox");
        assert_eq!(cfg.glm.sign_secret, "junjie");
        assert_eq!(cfg.glm.upstream_url, "https://chat.z.ai/api/v2/chat/completions");
        assert!(!cfg.glm.force_ui);
    }

    #[test]
    fn rejects_zero_port() {
        let mut cfg = Config::default();
        cfg.server.port = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_open_binding() {
        let mut cfg = Config::default();
        cfg.server.host = "0.0.0.0".to_string();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_invalid_browser_source() {
        let mut cfg = Config::default();
        cfg.browser.source = "safari".to_string();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn accepts_valid_browser_source_values() {
        for source in &["auto", "firefox", "chrome", "edge"] {
            let mut cfg = Config::default();
            cfg.browser.source = source.to_string();
            assert!(cfg.validate().is_ok(), "source={} should be valid", source);
        }
    }

    #[test]
    fn rejects_invalid_browser_identity() {
        let mut cfg = Config::default();
        cfg.browser.identity = Some("safari".to_string());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn import_options_with_profile_path() {
        let mut cfg = Config::default();
        cfg.browser.profile_path = Some("/custom/profile".to_string());
        let opts = cfg.browser.import_options();
        match opts.source {
            crate::browser::SourceSelection::Profile(p) => {
                assert_eq!(p.to_str(), Some("/custom/profile"));
            }
            _ => panic!("expected Profile source"),
        }
    }

    #[test]
    fn import_options_with_browser_type() {
        let mut cfg = Config::default();
        cfg.browser.source = "chrome".to_string();
        let opts = cfg.browser.import_options();
        match opts.source {
            crate::browser::SourceSelection::Browser(bt) => {
                assert_eq!(bt, crate::browser::BrowserType::Chrome);
            }
            _ => panic!("expected Browser source"),
        }
    }

    #[test]
    fn rejects_empty_glm_sign_secret() {
        let mut cfg = Config::default();
        cfg.glm.sign_secret = String::new();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_empty_glm_upstream_url() {
        let mut cfg = Config::default();
        cfg.glm.upstream_url = String::new();
        assert!(cfg.validate().is_err());
    }
}
