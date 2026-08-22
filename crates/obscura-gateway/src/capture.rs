use std::path::PathBuf;
use std::sync::Arc;

use obscura_browser::{BrowserContext, Page};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::browser;
use crate::config::Config;
use crate::error::GatewayError;
use crate::session::BrowserIdentity;

/// Provider-specific capture configuration.
struct CaptureConfig {
    /// Display name.
    name: &'static str,
    /// URL to navigate to.
    navigate_url: &'static str,
    /// JS expression to type a message into the input field.
    type_message_js: &'static str,
    /// JS expression to submit/trigger the send action.
    submit_js: &'static str,
    /// URL pattern to filter for capture (substring match).
    url_filter: &'static str,
}

/// All supported capture providers.
fn capture_config(provider: &str) -> Result<CaptureConfig, GatewayError> {
    match provider {
        "glm" => Ok(CaptureConfig {
            name: "GLM",
            navigate_url: "https://chat.z.ai",
            // The GLM chat input is a contenteditable div; we type into it.
            type_message_js: r#"
                (function(msg) {
                    const el = document.querySelector('[contenteditable="true"]') || document.querySelector('textarea');
                    if (!el) throw new Error('no input element found');
                    el.focus();
                    el.textContent = msg;
                    el.dispatchEvent(new Event('input', {bubbles: true}));
                    return 'typed';
                })("capture probe message from obscura")
            "#,
            submit_js: r#"
                (function() {
                    const btn = document.querySelector('[data-testid="send-button"]')
                        || document.querySelector('button[type="submit"]')
                        || document.querySelector('.send-btn');
                    if (btn) { btn.click(); return 'clicked send'; }
                    // Fallback: press Enter on the input
                    const el = document.querySelector('[contenteditable="true"]') || document.querySelector('textarea');
                    if (el) {
                        el.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', code: 'Enter', keyCode: 13, bubbles: true}));
                        return 'pressed enter';
                    }
                    throw new Error('no submit mechanism found');
                })()
            "#,
            url_filter: "/api/v2/chat/completions",
        }),
        "gemini" => Ok(CaptureConfig {
            name: "Gemini",
            navigate_url: "https://gemini.google.com/app",
            type_message_js: r#"
                (function(msg) {
                    const el = document.querySelector('.ql-editor') || document.querySelector('[contenteditable="true"]') || document.querySelector('rich-textarea');
                    if (!el) throw new Error('no input element found');
                    el.focus();
                    el.textContent = msg;
                    el.dispatchEvent(new Event('input', {bubbles: true}));
                    return 'typed';
                })("capture probe message from obscura")
            "#,
            submit_js: r#"
                (function() {
                    const btn = document.querySelector('[aria-label="Send message"]')
                        || document.querySelector('button[mattooltip="Send"]')
                        || document.querySelector('.send-button');
                    if (btn) { btn.click(); return 'clicked send'; }
                    throw new Error('no submit button found');
                })()
            "#,
            url_filter: "/BardFrontendService/StreamGenerate",
        }),
        "chatgpt" => Ok(CaptureConfig {
            name: "ChatGPT",
            navigate_url: "https://chatgpt.com",
            type_message_js: r#"
                (function(msg) {
                    const el = document.querySelector('#prompt-textarea') || document.querySelector('[contenteditable="true"]');
                    if (!el) throw new Error('no input element found');
                    el.focus();
                    el.textContent = msg;
                    el.dispatchEvent(new Event('input', {bubbles: true}));
                    return 'typed';
                })("capture probe message from obscura")
            "#,
            submit_js: r#"
                (function() {
                    const btn = document.querySelector('[data-testid="send-button"]') || document.querySelector('button[data-testid="send-button"]');
                    if (btn) { btn.click(); return 'clicked send'; }
                    throw new Error('no submit button found');
                })()
            "#,
            url_filter: "/backend-api/conversation",
        }),
        "kimi" => Ok(CaptureConfig {
            name: "Kimi",
            navigate_url: "https://kimi.moonshot.cn",
            type_message_js: r#"
                (function(msg) {
                    const el = document.querySelector('[contenteditable="true"]') || document.querySelector('textarea');
                    if (!el) throw new Error('no input element found');
                    el.focus();
                    el.textContent = msg;
                    el.dispatchEvent(new Event('input', {bubbles: true}));
                    return 'typed';
                })("capture probe message from obscura")
            "#,
            submit_js: r#"
                (function() {
                    const btn = document.querySelector('[data-testid="send-button"]')
                        || document.querySelector('button.send-btn')
                        || document.querySelector('button[aria-label="Send"]');
                    if (btn) { btn.click(); return 'clicked send'; }
                    throw new Error('no submit button found');
                })()
            "#,
            url_filter: "/api/chat",
        }),
        "grok" => Ok(CaptureConfig {
            name: "Grok",
            navigate_url: "https://grok.com",
            type_message_js: r#"
                (function(msg) {
                    const el = document.querySelector('textarea') || document.querySelector('[contenteditable="true"]');
                    if (!el) throw new Error('no input element found');
                    el.focus();
                    el.textContent = msg;
                    el.dispatchEvent(new Event('input', {bubbles: true}));
                    return 'typed';
                })("capture probe message from obscura")
            "#,
            submit_js: r#"
                (function() {
                    const btn = document.querySelector('button[aria-label="Send"]') || document.querySelector('button.send-btn');
                    if (btn) { btn.click(); return 'clicked send'; }
                    throw new Error('no submit button found');
                })()
            "#,
            url_filter: "/rest/app-chat/conversational",
        }),
        _ => Err(GatewayError::Config(format!(
            "unknown capture provider '{}'; supported: glm, gemini, chatgpt, kimi, grok",
            provider
        ))),
    }
}

/// Run the capture session for a single provider.
///
/// Creates a browser context, navigates to the provider's web UI, types a
/// probe message, submits it, and captures the matching API request/response
/// to `docs/wire/<provider>-capture.json`.
pub async fn run_capture(provider: &str, config: &Config) -> Result<(), GatewayError> {
    let cfg = capture_config(provider)?;
    let output_path = PathBuf::from(format!("docs/wire/{}-capture.json", provider));

    info!(provider = cfg.name, url = cfg.navigate_url, "Starting capture session");

    // Import auth from the user's browser profile.
    let import_options = config.browser.import_options();
    let auth = browser::import_auth(&import_options)?;
    let cookies = auth.cookies;
    let local_storage = auth.local_storage;
    let identity = BrowserIdentity::from_auth(&auth.source, config);

    // Create a browser context (same as session.rs warm()).
    let temp_dir = tempfile::tempdir().map_err(|e| {
        GatewayError::Internal(format!("failed to create temp profile: {e}"))
    })?;
    let context = Arc::new(BrowserContext::with_storage_full(
        format!("capture-{}", provider),
        None,
        identity.identity == "chrome",
        Some(identity.user_agent.clone()),
        Some(temp_dir.path().to_path_buf()),
    ));
    context.cookie_jar.set_cookies_from_cdp(cookies);

    let mut page = Page::new(format!("capture-{}", provider), context.clone());

    // Inject localStorage as a preload script so the page's auth check sees
    // the session token before any page scripts run. Post-navigate injection
    // is too late: the page may already have redirected to sign-in.
    if !local_storage.is_empty() {
        let preload = local_storage_preload_script(&local_storage);
        page.add_preload_script(&preload);
        info!(count = local_storage.len(), "Injected localStorage preload script");
    }

    // Capture state: accumulated requests and responses.
    let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));

    // Register capture callbacks.
    let captured_req = captured.clone();
    let url_filter = cfg.url_filter.to_string();
    page.on_request(Arc::new(move |req| {
        if req.url.as_str().contains(&url_filter) {
            let entry = serde_json::json!({
                "type": "request",
                "url": req.url.as_str(),
                "method": req.method,
                "headers": req.headers,
                "resource_type": format!("{:?}", req.resource_type),
                "timestamp": chrono::Utc::now().to_rfc3339(),
            });
            let mut guard = captured_req.blocking_lock();
            guard.push(entry);
            info!(url = %req.url, method = %req.method, "CAPTURED request");
        }
    }));

    let captured_resp = captured.clone();
    let url_filter = cfg.url_filter.to_string();
    page.on_response(Arc::new(move |req, resp| {
        if req.url.as_str().contains(&url_filter) {
            let body_text = resp.text();
            let entry = serde_json::json!({
                "type": "response",
                "url": req.url.as_str(),
                "status": resp.status,
                "headers": resp.headers,
                "body": body_text,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            });
            let mut guard = captured_resp.blocking_lock();
            guard.push(entry);
            info!(url = %req.url, status = resp.status, body_len = body_text.len(), "CAPTURED response");
        }
    }));

    // Navigate to the provider.
    info!(url = cfg.navigate_url, "Navigating to provider");
    page.navigate(cfg.navigate_url)
        .await
        .map_err(|e| GatewayError::Provider(format!("navigation failed: {e}")))?;

    // Wait for page load.
    page.settle(3000).await;
    info!(url = %page.url_string(), "Page loaded");

    // Type the probe message.
    info!("Typing probe message");
    let typed = page.evaluate(cfg.type_message_js);
    info!(result = %typed, "Type result");

    // Small delay to let the page process the input.
    page.settle(1000).await;

    // Submit the message.
    info!("Submitting probe message");
    let submitted = page.evaluate(cfg.submit_js);
    info!(result = %submitted, "Submit result");

    // Wait for the response to arrive.
    info!("Waiting for response (15s)...");
    page.settle(15000).await;

    // Collect results.
    let results = {
        let guard = captured.lock().await;
        guard.clone()
    };

    if results.is_empty() {
        warn!("No matching requests/responses captured via automated interception.");
        info!("");
        info!("=== MANUAL CAPTURE INSTRUCTIONS ===");
        info!("The automated capture did not detect the target API request.");
        info!("This can happen when the page uses ES modules that don't fully load in Obscura's V8 runtime.");
        info!("");
        info!("To capture manually:");
        info!("  1. Open {} in your real browser (Firefox/Chrome)", cfg.navigate_url);
        info!("  2. Open DevTools (F12) → Network tab");
        info!("  3. Filter by: {}", cfg.url_filter);
        info!("  4. Type a message and submit it");
        info!("  5. Find the matching request in the Network tab");
        info!("  6. Right-click → Copy → Copy as cURL");
        info!("  7. Also copy the Response body");
        info!("  8. Save both to docs/wire/{}-manual-capture.txt", provider);
        info!("");
        info!("Waiting 30 more seconds for any automated captures...");
        page.settle(30000).await;

        let retry_results = {
            let guard = captured.lock().await;
            guard.clone()
        };

        if retry_results.is_empty() {
            error!("No automated captures after extended wait.");
        }
    }

    // Build the final capture file.
    let requests: Vec<_> = results.iter().filter(|r| r["type"] == "request").collect();
    let responses: Vec<_> = results.iter().filter(|r| r["type"] == "response").collect();

    let capture_file = serde_json::json!({
        "provider": provider,
        "captured_at": chrono::Utc::now().to_rfc3339(),
        "navigate_url": cfg.navigate_url,
        "url_filter": cfg.url_filter,
        "page_url": page.url_string(),
        "identity": identity.identity,
        "requests_count": requests.len(),
        "responses_count": responses.len(),
        "captures": results,
    });

    // Ensure output directory exists.
    if let Some(parent) = output_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| GatewayError::Internal(format!("failed to create output dir: {e}")))?;
    }

    let json_str = serde_json::to_string_pretty(&capture_file)
        .map_err(|e| GatewayError::Internal(format!("failed to serialize capture: {e}")))?;
    tokio::fs::write(&output_path, &json_str)
        .await
        .map_err(|e| GatewayError::Internal(format!("failed to write capture file: {e}")))?;

    info!(
        path = %output_path.display(),
        requests = requests.len(),
        responses = responses.len(),
        "Capture saved"
    );

    Ok(())
}

/// Build a preload script that seeds localStorage before page scripts run.
/// Copied from session.rs to avoid cross-module dependency.
fn local_storage_preload_script(entries: &[crate::browser::LocalStorageEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }

    let payload = entries
        .iter()
        .map(|e| serde_json::json!({
            "origin": e.origin,
            "key": e.key,
            "value": e.value,
        }))
        .collect::<Vec<_>>();

    match serde_json::to_string(&payload) {
        Ok(json) => format!(
            "(function(entries) {{ \
                try {{ \
                    entries.forEach(function(entry) {{ \
                        if (entry.origin === location.origin) window.localStorage.setItem(entry.key, entry.value); \
                    }}); \
                }} catch (e) {{ \
                    console.error('obscura: failed to seed localStorage', e); \
                }} \
            }})({});",
            json
        ),
        Err(e) => {
            warn!(error = %e, "Failed to serialize localStorage preload payload");
            String::new()
        }
    }
}
