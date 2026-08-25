//! In-process minting of grok.com's signed `x-statsig-id`.
//!
//! Upstream validates statsig tokens against the live deploy; synthetic
//! markers are rejected with code 7 ("page out of date"). The tokens are
//! minted by a self-contained obfuscated signer chunk (`botoxSign`, module
//! 1645000) whose file name changes every deploy — but the chunk itself only
//! needs `btoa`/`atob`/`Math`, so we execute it inside the gateway's own V8
//! runtime:
//!
//! 1. Rust fetches the grok.com homepage and locates the module-map chunk.
//! 2. The map names the current signer chunk; Rust fetches its source.
//! 3. A minimal Turbopack shim + the signer source are evaluated in a fresh
//!    `MirageJsRuntime`; the default export mints the token for our path.
//!
//! Deploy-proof by construction: hashes are re-discovered every cycle.

/// A statsig id minted for our target path.
#[derive(Debug, Clone)]
pub struct HarvestedAuth {
    pub statsig_id: String,
}

use crate::error::GatewayError;

use super::state::GrokSessionStore;

const GROK_HOME: &str = "https://grok.com/";
/// Path we sign for. Grok validates the signature per-path; every gateway
/// request targets the conversations endpoint.
#[allow(dead_code)]
const SIGN_PATH: &str = "/rest/app-chat/conversations";

fn http_client() -> Result<reqwest::Client, GatewayError> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36")
        .build()
        .map_err(|e| GatewayError::Internal(format!("grok mint http client: {e}")))
}

/// Mint a fresh statsig id and persist it into `store`. Returns the token.
pub async fn harvest(store: &GrokSessionStore) -> Result<String, GatewayError> {
    let client = http_client()?;

    // 1. Homepage → all chunk URLs.
    let home = client
        .get(GROK_HOME)
        .send()
        .await
        .map_err(|e| GatewayError::Provider(format!("grok mint homepage failed: {e}")))?
        .text()
        .await
        .map_err(|e| GatewayError::Provider(format!("grok mint homepage body: {e}")))?;
    let mut urls: Vec<String> = Vec::new();
    for part in home.split('"') {
        if part.starts_with("/_next/static/chunks/")
            && part.ends_with(".js")
            && !urls.iter().any(|u| u.ends_with(part))
        {
            urls.push(part.to_string());
        }
    }
    if urls.is_empty() {
        return Err(GatewayError::Provider(
            "grok mint: no chunks found on homepage".to_string(),
        ));
    }

    // 2. Find the module-map chunk naming the signer file for module 4629918.
    let mut map_src: Option<String> = None;
    for url in &urls {
        if let Ok(resp) = client.get(format!("https://grok.com{url}")).send().await {
            if let Ok(text) = resp.text().await {
                if text.contains("4629918") && text.contains("static/chunks/") {
                    map_src = Some(text);
                    break;
                }
            }
        }
    }
    let map_src = map_src.ok_or_else(|| {
        GatewayError::Provider("grok mint: module-map chunk not found".to_string())
    })?;

    // Extract the signer filename from: 4629918,s=>{...["static/chunks/X.js"]...
    let signer_file = map_src
        .split("4629918")
        .nth(1)
        .and_then(|rest| {
            rest.find("static/chunks/").map(|pos| {
                rest[pos..]
                    .chars()
                    .take_while(|c| !matches!(c, '"' | ',' | ']' | ')'))
                    .collect::<String>()
            })
        })
        .ok_or_else(|| {
            GatewayError::Provider("grok mint: signer file not named in map".to_string())
        })?;
    let signer_url = format!("https://grok.com/_next/static/chunks/{signer_file}");

    // 3. Fetch signer source.
    let signer_src = client
        .get(&signer_url)
        .send()
        .await
        .map_err(|e| GatewayError::Provider(format!("grok mint signer fetch: {e}")))?
        .text()
        .await
        .map_err(|e| GatewayError::Provider(format!("grok mint signer body: {e}")))?;

    // 4. Evaluate in an isolated V8 runtime with a Turbopack shim. The
    // signer only needs btoa/atob/Math — no DOM, no network.
    let mut rt = obscura_browser::ObscuraJsRuntime::new();
    let signer_json = serde_json::to_string(&signer_src).unwrap_or_else(|_| "\"\"".to_string());
    let boot = format!(
        r#"(function(){{
            globalThis.__obscuraEntries = [];
            globalThis.TURBOPACK = {{ push: function(e) {{ __obscuraEntries.push(e); }} }};
        }})()"#
    );
    rt.evaluate(&boot).map_err(|e| GatewayError::Internal(e))?;
    rt.evaluate(
        &format!(
            "(function(){{ try {{ (0, eval)({signer_json}); }} catch(e) {{ globalThis.__obscuraSignerError = String(e); }} }})()"
        ),
    )
    .map_err(|e| GatewayError::Internal(e))?;
    rt.run_event_loop().await.map_err(|e| GatewayError::Internal(e))?;

    let instantiate = r#"(function(){
        const entry = (globalThis.__obscuraEntries || []).find(e => Number(e[1]) === 1645000);
        if (!entry) return { error: 'module 1645000 not registered', registered: (globalThis.__obscuraEntries || []).length };
        let def;
        const ctx = function(){ return []; };
        ctx.s = function(names, _meta, getter) {
            if (names && names.indexOf('default') >= 0) def = getter();
        };
        entry[2](ctx);
        if (typeof def !== 'function') return { error: 'no default export' };
        def('/rest/app-chat/conversations', 'POST').then(function(token){
            globalThis.__obscuraToken = token;
        }, function(e){
            globalThis.__obscuraToken = { error: 'sign rejected: ' + String(e) };
        });
        return 'pending';
    })()"#;
    rt.evaluate(instantiate).map_err(|e| GatewayError::Internal(e))?;
    rt.run_event_loop().await.map_err(|e| GatewayError::Internal(e))?;

    // Poll until the promise resolves (the signer is CPU-bound; resolves in
    // the first few pumps).
    for _ in 0..20 {
        let state = rt.evaluate(
            r#"(function(){
                const t = globalThis.__obscuraToken;
                if (typeof t === 'string' && t.length > 40) return { token: t };
                if (t && typeof t === 'object' && t.error) return { error: t.error };
                return null;
            })()"#,
        )
        .map_err(|e| GatewayError::Internal(e))?;
        if let Some(tok) = state.get("token").and_then(|t| t.as_str()) {
            if tok.len() >= 40 {
                store.store_statsig(tok.to_string());
                return Ok(tok.to_string());
            }
        }
        if let Some(err) = state.get("error").and_then(|e| e.as_str()) {
            return Err(GatewayError::Provider(format!(
                "grok in-page signer failed: {err}"
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        rt.run_event_loop().await.map_err(|e| GatewayError::Internal(e))?;
    }

    Err(GatewayError::Provider(
        "grok statsig mint timed out".to_string(),
    ))
}

/// Synchronous entry point for the provider: mints a fresh statsig id
/// (blocking, typically <2 s) and persists it into `store`. Returns None on
/// any failure — the caller falls back to the legacy marker.
pub fn harvest_via_browser_sync(store: &GrokSessionStore) -> Option<HarvestedAuth> {
    let store = store.clone();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    let result: Result<String, GatewayError> = rt.block_on(harvest(&store));
    match result {
        Ok(token) => {
            tracing::info!(statsig_len = token.len(), "grok statsig minted");
            Some(HarvestedAuth {
                statsig_id: token,
            })
        }
        Err(e) => {
            tracing::warn!(error = %e, "grok statsig mint failed");
            None
        }
    }
}
