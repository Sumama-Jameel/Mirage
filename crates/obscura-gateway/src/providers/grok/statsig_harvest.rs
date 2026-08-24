//! Browser-level harvest of grok.com's signed `x-statsig-id` and current
//! deploy release.
//!
//! Upstream validates statsig tokens against the live deploy; synthetic
//! markers are rejected with code 7 ("page out of date"). The only durable
//! source is the logged-in web app itself: we load grok.com in the engine,
//! observe outgoing `/rest/*` requests at the wire level (`Page::on_request`,
//! same mechanism as `crate::capture`), and take the header values the app
//! mints. Values persist in `GrokSessionStore` and remain valid until the
//! next deploy invalidates them (code 7 → invalidate → re-harvest).

use std::sync::Arc;

use obscura_browser::{BrowserContext, Page};
use tokio::sync::Mutex;

use crate::browser;
use crate::config::Config;
use crate::error::GatewayError;
use crate::session::BrowserIdentity;

use super::state::GrokSessionStore;

/// A statsig id observed on the wire, plus the deploy release declared by
/// the same request (ground truth for the Baggage header).
#[derive(Debug, Clone)]
pub struct HarvestedAuth {
    pub statsig_id: String,
    pub release: Option<String>,
}

/// Synchronous wrapper: runs the harvest on a dedicated thread with its own
/// current-thread runtime, because the browser Page embeds the engine's
/// non-Send V8 state and cannot be awaited inside the gateway's Send-bounded
/// provider futures. Returns None on any failure.
pub fn harvest_via_browser_sync(
    config: &Config,
    store: &GrokSessionStore,
) -> Option<HarvestedAuth> {
    let config = config.clone();
    let store = store.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let result: Option<HarvestedAuth> = rt.block_on(harvest_via_browser(&config, &store)).ok();
        let _ = tx.send(result);
    });
    rx.recv().unwrap_or(None)
}

/// Load the logged-in grok.com page in a throwaway browser context and
/// capture the first signed request. Typically completes in 10–20 s.
///
/// Persisted side effects: statsig id and (when observed) deploy release are
/// written into `store`.
async fn harvest_via_browser(
    config: &Config,
    store: &GrokSessionStore,
) -> Result<HarvestedAuth, GatewayError> {
    let import_options = config.browser.import_options();
    let auth = browser::import_auth(&import_options)?;
    let identity = BrowserIdentity::from_auth(&auth.source, config);

    let temp_dir = tempfile::tempdir()
        .map_err(|e| GatewayError::Internal(format!("harvest temp profile failed: {e}")))?;
    let context = Arc::new(BrowserContext::with_storage_full(
        "grok-statsig-harvest".to_string(),
        None,
        identity.identity == "chrome",
        Some(identity.user_agent.clone()),
        Some(temp_dir.path().to_path_buf()),
    ));
    // Auth cookies (sso + sso-rw) must precede navigation.
    context.cookie_jar.set_cookies_from_cdp(auth.cookies);

    let mut page = Page::new("grok-statsig-harvest".to_string(), context.clone());

    // Wire-level capture: any /rest/ request carrying a signed statsig id.
    let captured: Arc<Mutex<Option<HarvestedAuth>>> = Arc::new(Mutex::new(None));
    let captured_req = captured.clone();
    page.on_request(Arc::new(move |req| {
        if captured_req.blocking_lock().is_some() || !req.url.as_str().contains("/rest/") {
            return;
        }
        let statsig = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("x-statsig-id"))
            .map(|(_, v)| v.clone());
        let Some(statsig_id) = statsig else { return };
        let release = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("baggage"))
            .and_then(|(_, v)| {
                v.split(',').find_map(|part| {
                    let part = part.trim();
                    part.strip_prefix("sentry-release=")
                })
            })
            .map(String::from);
        tracing::info!(
            url = %req.url,
            statsig_len = statsig_id.len(),
            release = release.as_deref().unwrap_or(""),
            "Grok statsig harvested from live request"
        );
        *captured_req.blocking_lock() = Some(HarvestedAuth {
            statsig_id,
            release,
        });
    }));

    page.navigate("https://grok.com/")
        .await
        .map_err(|e| GatewayError::Provider(format!("grok harvest navigation failed: {e}")))?;
    page.settle(4000).await;

    // The homepage fires several signed REST calls on its own; if none have
    // landed yet, nudge the app by typing into the composer (no submit).
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(25);
    loop {
        if captured.lock().await.is_some() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let _ = page.evaluate(
            r#"(function(){
                const el = document.querySelector('textarea') || document.querySelector('[contenteditable="true"]');
                if (!el) return 'no-input';
                el.focus();
                el.textContent = 'hi';
                el.dispatchEvent(new Event('input', {bubbles: true}));
                return 'typed';
            })()"#,
        );
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    }

    let harvested = captured.lock().await.clone();
    let Some(harvested) = harvested else {
        return Err(GatewayError::Provider(
            "Grok statsig harvest timed out: no signed request was observed on the page \
             (is the imported session still logged in to grok.com?)"
                .to_string(),
        ));
    };

    store.store_statsig(harvested.statsig_id.clone());
    if let Some(release) = &harvested.release {
        store.store_release(release.clone());
    }
    Ok(harvested)
}
