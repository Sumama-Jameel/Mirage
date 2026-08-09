use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::error::GatewayError;
use crate::session::SessionManager;

use super::extract;
use super::statsig::ChallengeConfig;

/// Hardcoded defaults shipped with the binary.
/// These become stale when grok.com updates their frontend; `ChallengeStore::renew()`
/// replaces them with live-extracted values at runtime.
///
/// **Environment Override**: Set these variables to use custom constants without recompiling:
///   - `GROK_CHALLENGE_HEADER_HEX`: 98-character hex string (49 bytes) of the header
///   - `GROK_CHALLENGE_SUFFIX`: Base64-encoded suffix string
///   - `GROK_CHALLENGE_TRAILER`: Single byte trailer value (default: 3)
///
/// When extraction fails or env vars are not set, falls back to defaults.
/// If constants are stale (Grok returns 401/403), run EXTRACT_GROK_CONSTANTS.txt
/// from the browser console and update these values or the env vars.
///
/// See AGENTS.md for detailed extraction instructions.
const DEFAULT_HEADER_HEX: &str =
    "009a030e5b3a5ec69fe18b49ad9c77a2fe3d275536e0c8634bce31fc915f8a6dae07e1e58aeba7c358565c32e4805aab04";
const DEFAULT_SUFFIX: &str =
    "obfiowerehiringb94f340f0a3d70a3d70a0570a3d70a3d70c0570a3d70a3d70c0f0a3d70a3d70a00";
const DEFAULT_TRAILER: u8 = 3;

fn decode_header_hex(hex: &str) -> [u8; 49] {
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect();
    let mut header = [0u8; 49];
    let len = bytes.len().min(49);
    header[..len].copy_from_slice(&bytes[..len]);
    header
}

fn config_from_env_or_defaults() -> ChallengeConfig {
    if let Ok(hex) = std::env::var("GROK_CHALLENGE_HEADER_HEX") {
        let suffix = std::env::var("GROK_CHALLENGE_SUFFIX").unwrap_or_default();
        let trailer: u8 = std::env::var("GROK_CHALLENGE_TRAILER")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TRAILER);
        if hex.len() != 98 {
            tracing::warn!(
                "GROK_CHALLENGE_HEADER_HEX is {} chars (expected 98)",
                hex.len()
            );
        }
        return ChallengeConfig::new(decode_header_hex(&hex), suffix, trailer);
    }
    ChallengeConfig::new(
        decode_header_hex(DEFAULT_HEADER_HEX),
        DEFAULT_SUFFIX.to_string(),
        DEFAULT_TRAILER,
    )
}

#[derive(Serialize, Deserialize)]
struct PersistedChallenge {
    header_hex: String,
    suffix: String,
    trailer: u8,
    extracted_at: u64,
}

struct ChallengeStoreInner {
    config: ChallengeConfig,
    extracted_at: u64,
}

/// Runtime mutable store for Grok's x-statsig-id challenge constants.
///
/// Holds the current `ChallengeConfig` behind an `Arc<RwLock>>` so it can
/// be shared across concurrent requests. On detection of a stale config
/// (Grok API returning 403), callers invoke `renew()` to live-extract
/// fresh constants from a browser session.
///
/// Priority chain (highest to lowest):
///   1. Environment variables (`GROK_CHALLENGE_HEADER_HEX`, etc.)
///   2. Disk-persisted config from a previous successful extraction
///   3. Hardcoded defaults shipped with the binary
#[derive(Clone)]
pub struct ChallengeStore {
    inner: Arc<RwLock<ChallengeStoreInner>>,
    session_file: Option<PathBuf>,
}

impl ChallengeStore {
    /// Create a store with no disk persistence.
    /// State is loaded from env vars or hardcoded defaults.
    pub fn new() -> Self {
        let config = config_from_env_or_defaults();
        Self {
            inner: Arc::new(RwLock::new(ChallengeStoreInner {
                config,
                extracted_at: 0,
            })),
            session_file: None,
        }
    }

    /// Create a store with optional disk persistence under `data_dir`.
    /// On construction, tries disk → env vars → defaults.
    pub fn with_data_dir(data_dir: Option<PathBuf>) -> Self {
        let session_file = data_dir.as_ref().map(|d| d.join("grok_challenge.json"));

        if let Some(ref path) = session_file {
            if let Ok(content) = std::fs::read_to_string(path) {
                if let Ok(persisted) = serde_json::from_str::<PersistedChallenge>(&content) {
                    let header = decode_header_hex(&persisted.header_hex);
                    if header.iter().any(|&b| b != 0) {
                        let config =
                            ChallengeConfig::new(header, persisted.suffix, persisted.trailer);
                        tracing::info!(
                            "Loaded Grok challenge constants from disk (extracted_at={})",
                            persisted.extracted_at
                        );
                        return Self {
                            inner: Arc::new(RwLock::new(ChallengeStoreInner {
                                config,
                                extracted_at: persisted.extracted_at,
                            })),
                            session_file,
                        };
                    }
                }
            }
        }

        let config = config_from_env_or_defaults();
        Self {
            inner: Arc::new(RwLock::new(ChallengeStoreInner {
                config,
                extracted_at: 0,
            })),
            session_file,
        }
    }

    /// Returns the current challenge config.
    /// If env vars are set they take precedence at every call (debug override).
    pub fn get_config(&self) -> ChallengeConfig {
        if std::env::var("GROK_CHALLENGE_HEADER_HEX").is_ok() {
            return config_from_env_or_defaults();
        }
        let inner = self.inner.read().unwrap();
        inner.config.clone()
    }

    /// Returns true if the current config came from a live extraction
    /// (not defaults or env vars).
    pub fn was_extracted(&self) -> bool {
        let inner = self.inner.read().unwrap();
        inner.extracted_at > 0
    }

    /// Re-extract challenge constants from a live browser session.
    ///
    /// Navigates the session to grok.com, captures the x-statsig-id token
    /// the page's own JS generates, unscrambles it, and updates the store.
    /// The new config is also persisted to disk if a data_dir was configured.
    pub async fn renew(
        &self,
        sessions: &SessionManager,
        session_id: &str,
    ) -> Result<(), GatewayError> {
        tracing::info!("Renewing Grok challenge constants from browser session");

        let extracted = extract::extract_challenge(sessions, session_id).await?;
        let config = extracted.into_config()?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        {
            let mut inner = self.inner.write().unwrap();
            inner.config = config.clone();
            inner.extracted_at = now;
        }

        self.save_to_disk(&config, now);

        tracing::info!("Grok challenge constants refreshed successfully");
        Ok(())
    }

    fn save_to_disk(&self, config: &ChallengeConfig, extracted_at: u64) {
        let Some(ref path) = self.session_file else { return };
        let persisted = PersistedChallenge {
            header_hex: config.header().iter().map(|b| format!("{:02x}", b)).collect(),
            suffix: config.suffix().to_string(),
            trailer: config.trailer(),
            extracted_at,
        };
        if let Ok(json) = serde_json::to_string(&persisted) {
            let tmp = path.with_extension("tmp");
            let _ = std::fs::write(&tmp, &json);
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

impl Default for ChallengeStore {
    fn default() -> Self {
        Self::new()
    }
}
