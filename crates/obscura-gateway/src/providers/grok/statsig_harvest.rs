//! In-process minting of grok.com's signed `x-statsig-id`.
//!
//! Upstream validates statsig tokens against the live deploy; synthetic
//! markers are rejected with code 7 ("page out of date"). The tokens are
//! minted by a self-contained obfuscated signer chunk (`botoxSign`, module
//! 1645000) whose file name changes every deploy — but the chunk itself only
//! needs `btoa`/`atob`/`Math`, so we execute it inside an isolated V8
//! runtime.
//!
//! Fetch strategy: plain reqwest with Chrome UA — same as `current_release()`
//! which reliably fetches grok.com. No stealth/TLS emulation needed for
//! static asset discovery.

use crate::error::GatewayError;

use super::state::GrokSessionStore;

const GROK_HOME: &str = "https://grok.com/";

#[derive(Debug, Clone)]
pub struct HarvestedAuth {
    pub statsig_id: String,
}

fn http_client() -> Result<reqwest::Client, GatewayError> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36")
        .build()
        .map_err(|e| GatewayError::Internal(format!("grok mint http client: {e}")))
}

async fn get_text(client: &reqwest::Client, url: &str) -> Result<String, GatewayError> {
    let resp = client
        .get(url)
        .header("Accept", "*/*")
        .send()
        .await
        .map_err(|e| GatewayError::Provider(format!("grok mint GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(GatewayError::Provider(format!(
            "grok mint GET {url} returned {}",
            resp.status()
        )));
    }
    resp.text()
        .await
        .map_err(|e| GatewayError::Provider(format!("grok mint GET {url} utf8: {e}")))
}

/// Fetch the current deploy's signer chunk source (Send-safe async half).
async fn fetch_signer_source(client: &reqwest::Client) -> Result<String, GatewayError> {
    let home = get_text(client, GROK_HOME).await?;
    tracing::info!(
        home_len = home.len(),
        "grok mint: homepage fetched"
    );

    let mut urls: Vec<String> = Vec::new();
    for part in home.split('"') {
        if part.contains("_next/static/chunks/") && part.ends_with(".js") {
            let url = if part.starts_with("http") {
                part.to_string()
            } else {
                format!("https://grok.com{part}")
            };
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    if urls.is_empty() {
        let preview: String = home.chars().take(200).collect();
        return Err(GatewayError::Provider(format!(
            "grok mint: no chunks found on homepage (len={}). Preview: {preview}",
            home.len()
        )));
    }

    // Locate the module-map chunk naming the signer file for module 4629918.
    for url in &urls {
        let text = get_text(client, url).await?;
        if text.contains("4629918") && text.contains("static/chunks/") {
            let signer_file = text
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
                    GatewayError::Provider(
                        "grok mint: signer file not named in map".to_string(),
                    )
                })?;
            tracing::info!(signer = %signer_file, "grok mint: signer chunk discovered");
            let signer_url = format!("https://grok.com/_next/{signer_file}");
            return get_text(client, &signer_url).await;
        }
    }

    Err(GatewayError::Provider(
        "grok mint: no chunk referenced module 4629918".to_string(),
    ))
}

/// Execute the signer inside an isolated V8 runtime (blocking).
fn mint_blocking(
    signer_src: String,
    store: GrokSessionStore,
) -> Result<HarvestedAuth, GatewayError> {
    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| GatewayError::Internal(format!("grok mint runtime: {e}")))?;

    tokio_rt.block_on(async move {
        let mut js = obscura_browser::ObscuraJsRuntime::new();

        // ATOMIC: shim + eval signer + instantiate + start signing, all in one
        // evaluate call so globalThis mutations persist within the same script.
        let signer_json =
            serde_json::to_string(&signer_src).unwrap_or_else(|_| "\"\"".to_string());
        let combined = format!(
            r#"(function() {{
                const entries = [];
                globalThis.TURBOPACK = {{ push: function(e) {{ entries.push(e); }} }};
                try {{ (0, eval)({signer_json}); }} catch(e) {{
                    return {{ error: 'eval failed: ' + String(e) }};
                }}
                const entry = entries.find(function(e) {{ return Number(e[1]) === 1645000; }});
                if (!entry) return {{ error: 'module 1645000 not registered', registered: entries.length }};
                let def;
                const ctx = function() {{ return []; }};
                ctx.s = function(names, meta, getter) {{
                    if (names && names.indexOf('default') >= 0) def = getter();
                }};
                entry[2](ctx);
                if (typeof def !== 'function') return {{ error: 'no default export' }};
                def('/rest/app-chat/conversations', 'POST').then(function(token) {{
                    globalThis.__obscuraToken = token;
                }}, function(e) {{
                    globalThis.__obscuraToken = {{ error: 'sign rejected: ' + String(e) }};
                }});
                return {{ ok: true, registered: entries.length }};
            }})()"#
        );
        let started = js.evaluate(&combined).map_err(|e| GatewayError::Internal(e))?;
        if let Some(err) = started.get("error").and_then(|e| e.as_str()) {
            return Err(GatewayError::Provider(format!("grok mint setup: {err}")));
        }

        // Pump the event loop until the signing promise resolves.
        for _ in 0..40 {
            js.run_event_loop().await.map_err(|e| GatewayError::Internal(e))?;
            let state = js.evaluate(
                r#"(function(){
                    const t = globalThis.__obscuraToken;
                    if (typeof t === 'string' && t.length > 40) return { token: t };
                    if (t && typeof t === 'object' && t.error) return { error: t.error };
                    return null;
                })()"#,
            ).map_err(|e| GatewayError::Internal(e))?;

            if let Some(tok) = state.get("token").and_then(|t| t.as_str()) {
                if tok.len() >= 40 {
                    store.store_statsig(tok.to_string());
                    return Ok(HarvestedAuth {
                        statsig_id: tok.to_string(),
                    });
                }
                return Err(GatewayError::Provider(format!(
                    "grok mint produced short token ({} chars)",
                    tok.len()
                )));
            }
            if let Some(err) = state.get("error").and_then(|e| e.as_str()) {
                return Err(GatewayError::Provider(format!(
                    "grok mint sign rejected: {err}"
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        Err(GatewayError::Provider(
            "grok statsig mint timed out".to_string(),
        ))

    })
}

/// Mint a fresh signed statsig id and persist it into `store`.
pub async fn harvest_auth(store: &GrokSessionStore) -> Option<HarvestedAuth> {
    let client = match http_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "grok statsig http client failed");
            return None;
        }
    };
    let signer_src = match fetch_signer_source(&client).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "grok statsig fetch failed");
            return None;
        }
    };
    // Strip browser-only references that crash in a bare V8 sandbox.
    let signer_src = signer_src.replace("document.currentScript", "void 0");
    let store = store.clone();
    match tokio::task::spawn_blocking(move || mint_blocking(signer_src, store)).await {
        Ok(Ok(auth)) => {
            tracing::info!(
                statsig_len = auth.statsig_id.len(),
                "grok statsig minted successfully"
            );
            Some(auth)
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "grok statsig mint failed");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "grok statsig mint task panicked");
            None
        }
    }
}
