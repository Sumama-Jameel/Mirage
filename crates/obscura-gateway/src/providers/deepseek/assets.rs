//! DeepSeek asset discovery.
//!
//! DeepSeek ships its PoW solver as a WebAssembly module on a CDN. The exact
//! URL changes with each deployment, so we discover it at runtime from the
//! warmed page and its worker chunk.

use std::sync::OnceLock;

use tracing::{info, warn};

use crate::error::GatewayError;
use crate::session::SessionManager;

/// Statically cached assets. Discovery happens once per process; after that
/// every request reuses the URLs.
static CACHED_ASSETS: OnceLock<DeepSeekAssets> = OnceLock::new();

/// Fallback WASM URL taken from the public reverse-engineering reference
/// implementation. Used only when dynamic discovery fails, so the gateway
/// stays useful even if DeepSeek reorganizes its markup.
const FALLBACK_WASM_URL: &str = "https://raw.githubusercontent.com/sums001/Deepseek-API/main/deepseek/sha3_wasm_bg.wasm";

/// Discovered DeepSeek static assets.
#[derive(Debug, Clone)]
pub struct DeepSeekAssets {
    /// Absolute URL of the PoW wasm module.
    pub wasm_url: String,
}

impl DeepSeekAssets {
    /// Return cached assets, discovering them first if necessary.
    ///
    /// Discovery uses the warmed browser page when possible and falls back to
    /// a well-known URL if the page cannot reveal the current wasm location.
    pub async fn discover(sessions: &SessionManager, session_id: &str) -> Result<Self, GatewayError> {
        if let Some(assets) = CACHED_ASSETS.get() {
            return Ok(assets.clone());
        }

        let discovered = Self::discover_in_page(sessions, session_id).await;
        let assets = match discovered {
            Ok(a) => a,
            Err(e) => {
                warn!(
                    error = %e,
                    fallback = %FALLBACK_WASM_URL,
                    "Dynamic asset discovery failed; using fallback wasm URL"
                );
                Self {
                    wasm_url: FALLBACK_WASM_URL.to_string(),
                }
            }
        };

        let _ = CACHED_ASSETS.set(assets.clone());
        info!(wasm_url = %assets.wasm_url, "DeepSeek assets discovered");
        Ok(assets)
    }

    /// Ask the warmed page for the wasm URL.
    ///
    /// We try several strategies in order:
    /// 1. Look for a preload/prefetch link whose href contains `sha3_wasm_bg`.
    /// 2. Locate the webpack worker chunk URL, fetch the worker script from
    ///    Rust, and parse the wasm filename out of it.
    /// 3. Scan all script/link tags for anything that looks like the wasm.
    async fn discover_in_page(
        sessions: &SessionManager,
        session_id: &str,
    ) -> Result<Self, GatewayError> {
        // Strategy 1: direct preload link / explicit tag.
        let js = r#"
        (function() {
            function hrefLike(pattern) {
                var selectors = [
                    'link[rel="preload"][as="fetch"][href*="' + pattern + '"]',
                    'link[rel="prefetch"][href*="' + pattern + '"]',
                    'link[rel="preload"][href*="' + pattern + '"]',
                    'script[src*="' + pattern + '"]'
                ];
                for (var i = 0; i < selectors.length; i++) {
                    var el = document.querySelector(selectors[i]);
                    if (el) return el.href || el.src;
                }
                return null;
            }

            function workerUrl() {
                var wpr;
                try { wpr = __webpack_require__; } catch (e) {}
                if (!wpr) try { wpr = self.__webpack_require__; } catch (e) {}
                if (!wpr) try { wpr = window.__webpack_require__; } catch (e) {}
                var CHUNK_ID = 37627;
                if (wpr && typeof wpr.u === 'function') {
                    var filename = wpr.u(CHUNK_ID);
                    var base = (typeof wpr.b === 'string') ? wpr.b : '';
                    if (filename && base) {
                        try { return new URL(filename, base).href; } catch (e) {}
                    }
                }
                var els = document.querySelectorAll('script[src*="37627"], link[href*="37627"]');
                for (var j = 0; j < els.length; j++) {
                    var src = els[j].src || els[j].href;
                    if (src) return src;
                }
                return null;
            }

            return {
                wasmUrl: hrefLike('sha3_wasm_bg'),
                workerUrl: workerUrl(),
            };
        })()
        "#;

        let result = sessions.execute_js(session_id, js).await?;

        if let Some(wasm_url) = result.get("wasmUrl").and_then(|v| v.as_str()) {
            if !wasm_url.is_empty() {
                return Ok(Self {
                    wasm_url: wasm_url.to_string(),
                });
            }
        }

        // Strategy 2: fetch the worker chunk and grep for the wasm filename.
        if let Some(worker_url) = result.get("workerUrl").and_then(|v| v.as_str()) {
            if !worker_url.is_empty() {
                if let Some(wasm_url) = Self::parse_wasm_url_from_worker(worker_url).await {
                    return Ok(Self {
                        wasm_url: wasm_url.to_string(),
                    });
                }
            }
        }

        Err(GatewayError::Provider(
            "could not discover DeepSeek wasm URL from page".to_string(),
        ))
    }

    /// Fetch a worker script and extract the wasm filename.
    async fn parse_wasm_url_from_worker(worker_url: &str) -> Option<String> {
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
        {
            Ok(c) => c,
            Err(_) => return None,
        };

        let resp = match client.get(worker_url).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "Failed to fetch DeepSeek worker chunk");
                return None;
            }
        };

        let text = match resp.text().await {
            Ok(t) => t,
            Err(_) => return None,
        };

        // Look for a string literal containing the wasm filename, e.g.
        // "sha3_wasm_bg.7b9ca65ddd.wasm".
        if let Some(start) = text.find("sha3_wasm_bg") {
            let tail = &text[start..];
            // Find the enclosing quote pair.
            let rest = &tail["sha3_wasm_bg".len()..];
            if let Some(end) = rest.find(".wasm") {
                let filename = &tail[.."sha3_wasm_bg".len() + end + ".wasm".len()];
                // If it's a full URL, return as-is; otherwise resolve against
                // the worker base URL.
                if filename.starts_with("http") {
                    return Some(filename.to_string());
                }
                if let Ok(base) = url::Url::parse(worker_url) {
                    if let Ok(resolved) = base.join(filename) {
                        return Some(resolved.to_string());
                    }
                }
            }
        }

        None
    }
}
