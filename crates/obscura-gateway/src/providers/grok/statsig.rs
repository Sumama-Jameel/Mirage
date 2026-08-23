use base64::Engine;

/// Current grok.com web requests require a signed `x-statsig-id` minted by
/// grok.com's own JavaScript (capture-verified: captures/WhileCapturingGrok
/// shows ~172-char base64 tokens; synthetic error markers are rejected with
/// code 7). We harvest one from the live logged-in page by hooking `fetch`
/// and triggering a lightweight same-origin API call.
///
/// Harvested ids are cached with a short TTL; when no harvest is possible
/// the retired synthetic marker remains as a last-resort fallback.
pub const HARVEST_TTL_SECS: u64 = 300;

/// JS installed into the live grok.com page: wraps `window.fetch`, records
/// the `x-statsig-id` header of every same-origin request into
/// `window.__obscuraStatsig`, and exposes a getter.
pub const HARVEST_INSTALL_JS: &str = r#"
(function() {
    if (window.__obscuraStatsigHooked) return 'already';
    window.__obscuraStatsig = null;
    const orig = window.fetch.bind(window);
    window.fetch = async function(input, init) {
        try {
            const h = new Headers((init && init.headers) || (input && input.headers) || undefined);
            const v = h.get('x-statsig-id');
            if (v) window.__obscuraStatsig = v;
        } catch (_) {}
        return orig(input, init);
    };
    window.__obscuraStatsigHooked = true;
    return 'installed';
})()
"#;

/// JS that performs a lightweight authenticated call so the hooked fetch
/// captures a freshly minted statsig id.
pub const HARVEST_TRIGGER_JS: &str = r#"
(async function() {
    try {
        await fetch('/rest/app-chat/conversations?pageSize=1', {
            credentials: 'include',
            headers: { 'Accept': 'application/json' }
        });
        return { statsig: window.__obscuraStatsig };
    } catch (e) {
        return { statsig: null, error: String(e) };
    }
})()
"#;

/// Harvest a fresh signed `x-statsig-id` from the live logged-in grok.com
/// page. Returns None when the page did not yield one.
pub async fn harvest_from_page(
    sessions: &crate::session::SessionManager,
    session_id: &str,
) -> Option<String> {
    sessions.navigate(session_id, "https://grok.com/").await.ok()?;
    // Give the SPA time to boot its telemetry/statsig machinery.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    for _ in 0..3 {
        sessions.pump_event_loop(session_id, 200).await.ok();
    }

    let install = sessions.execute_js(session_id, HARVEST_INSTALL_JS).await.ok()?;
    if !install.to_string().contains("installed") && !install.to_string().contains("already") {
        return None;
    }
    let result = sessions.execute_js(session_id, HARVEST_TRIGGER_JS).await.ok()?;
    let value = result.get("statsig").and_then(|v| v.as_str())?;
    (!value.is_empty()).then(|| value.to_string())
}

/// Generate a fallback synthetic marker (retired upstream, kept only as a
/// last-resort attempt before failing).
pub fn browser_statsig_id() -> String {
    let msg = if rand::random::<bool>() {
        // "Cannot read properties of null (reading 'children["xxxxx"]')"
        format!(
            "e:TypeError: Cannot read properties of null (reading 'children[\"{}\"]')",
            random_alphanumeric(5)
        )
    } else {
        // "Cannot read properties of undefined (reading 'xxxxxxxxxx')"
        format!(
            "e:TypeError: Cannot read properties of undefined (reading '{}')",
            random_lowercase(10)
        )
    };
    base64::engine::general_purpose::STANDARD.encode(msg)
}

fn random_alphanumeric(len: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..len)
        .map(|_| CHARS[rand::random::<usize>() % CHARS.len()] as char)
        .collect()
}

fn random_lowercase(len: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    (0..len)
        .map(|_| CHARS[rand::random::<usize>() % CHARS.len()] as char)
        .collect()
}
