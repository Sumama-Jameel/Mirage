use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD as BASE64;

use crate::error::GatewayError;
use crate::session::SessionManager;

use super::statsig::ChallengeConfig;

const EPOCH_SECS: u64 = 1_682_924_400;

/// Preload script injected before navigating to grok.com.
///
/// Wraps `crypto.subtle.digest` to capture the SHA-256 hash input (which
/// contains the suffix). Also wraps XHR to capture x-statsig-id from
/// headers set via setRequestHeader.
///
/// The fetch wrapper is injected *after* the page settles so it wraps
/// whatever the Statsig SDK has already installed.
///
/// The x-statsig-id token is XOR'd with a single random byte. Since the
/// counter (bytes 49-52) is embedded in the token, we can brute-force the
/// XOR key in Rust by picking the K whose decoded counter is closest to
/// the current timestamp.
const EXTRACT_PRELOAD_SCRIPT: &str = r#"
(function(){
  if (window.__grokCaptureDone) return;
  window.__grokCapture = { hashInput: null, statsigToken: null, done: false, fetchType: null };

  var _origDigest = crypto.subtle.digest.bind(crypto.subtle);
  crypto.subtle.digest = async function(alg, data) {
    var input = new TextDecoder().decode(data);
    if (input.indexOf('!') !== -1 && (input.indexOf('GET!') === 0 || input.indexOf('POST!') === 0)) {
      if (!window.__grokCapture.hashInput) {
        window.__grokCapture.hashInput = input;
      }
    }
    return _origDigest(alg, data);
  };

  var _origXHR = XMLHttpRequest.prototype.setRequestHeader;
  XMLHttpRequest.prototype.setRequestHeader = function(name, value) {
    if (name.toLowerCase() === 'x-statsig-id') {
      if (!window.__grokCapture.statsigToken) {
        window.__grokCapture.statsigToken = value;
        if (window.__grokCapture.hashInput) {
          window.__grokCapture.done = true;
        }
      }
    }
    return _origXHR.call(this, name, value);
  };

  var _origSend = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.send = function() {
    if (this.__statsigId && !window.__grokCapture.statsigToken) {
      window.__grokCapture.statsigToken = this.__statsigId;
      if (window.__grokCapture.hashInput) {
        window.__grokCapture.done = true;
      }
    }
    return _origSend.apply(this, arguments);
  };
})();
"#;

/// Result of a successful challenge extraction.
#[derive(Debug, Clone)]
pub struct ExtractedChallenge {
    pub header_hex: String,
    pub suffix: String,
    pub trailer: u8,
}

impl ExtractedChallenge {
    pub fn into_config(&self) -> Result<ChallengeConfig, GatewayError> {
        let decoded = hex_decode(&self.header_hex).map_err(|e| {
            GatewayError::Internal(format!("invalid extracted header hex: {e}"))
        })?;
        let header: [u8; 49] = decoded.clone().try_into().map_err(|_: Vec<u8>| {
            GatewayError::Internal(format!(
                "extracted header must be 49 bytes, got {}",
                decoded.len()
            ))
        })?;
        Ok(ChallengeConfig::new(header, self.suffix.clone(), self.trailer))
    }
}

/// Extract Grok's challenge constants by navigating a warmed browser
/// session to grok.com and intercepting the `x-statsig-id` the page's
/// own JS generates.
///
/// The session is temporarily navigated to `https://grok.com`. After
/// extraction it is navigated back to `https://chat.deepseek.com` so
/// other providers (DeepSeek, GLM) continue to work.
pub async fn extract_challenge(
    sessions: &SessionManager,
    session_id: &str,
) -> Result<ExtractedChallenge, GatewayError> {
    // Increase timeouts so grok.com's many JS bundles finish loading
    std::env::set_var("OBSCURA_NAV_TIMEOUT_MS", "120000");
    std::env::set_var("OBSCURA_SCRIPT_DEADLINE_MS", "110000");

    sessions
        .add_preload_script(session_id, EXTRACT_PRELOAD_SCRIPT)
        .await?;

    tracing::info!(session_id = %session_id, "Navigating to grok.com for challenge extraction");
    sessions.navigate(session_id, "https://grok.com").await?;

    // Give the page time to load its JS bundles
    for i in 0..60 {
        sessions.pump_event_loop(session_id, 1000).await?;

        // Check if a natural API call produced the token
        let capture = sessions
            .execute_js(
                session_id,
                "JSON.stringify(window.__grokCapture || {})",
            )
            .await?;

        let cap = capture.as_str().unwrap_or("{}");
        if cap.contains("\"done\":true") || cap.contains("\"statsigToken\":\"") {
            let parsed: serde_json::Value =
                serde_json::from_str(cap).map_err(|e| {
                    GatewayError::Internal(format!("failed to parse capture: {e}"))
                })?;

            let token = parsed["statsigToken"]
                .as_str()
                .ok_or_else(|| {
                    GatewayError::Provider(
                        "grok.com did not generate x-statsig-id. \
                         Make sure you are logged into grok.com in your browser."
                            .to_string(),
                    )
                })?;

            let hash_input = parsed["hashInput"].as_str().unwrap_or("");

            let extracted = parse_extracted_data(token, hash_input)?;

            // Remove extraction monkey-patches so they don't affect other providers
            sessions.clear_preload_scripts(session_id).await?;

            // Navigate back to DeepSeek so other providers work
            sessions
                .navigate(session_id, "https://chat.deepseek.com")
                .await?;

            tracing::info!(
                header_hex = %extracted.header_hex,
                suffix = %extracted.suffix,
                trailer = %extracted.trailer,
                "Grok challenge constants extracted"
            );

            return Ok(extracted);
        }

        if i == 10 || i == 30 {
            let info = sessions
                .execute_js(session_id, r#"JSON.stringify({
                  title: document.title,
                  bodyLen: (document.body ? document.body.innerText.length : 0),
                  bodyPreview: (document.body ? document.body.innerText.substring(0,100) : ''),
                  scriptCount: document.querySelectorAll('script').length,
                  url: location.href
                })"#)
                .await?;
            tracing::debug!(i, page = %info.as_str().unwrap_or("?"), "Page state during natural wait");
        }

        tracing::debug!(i, "Waiting for grok.com challenge capture...");
    }

    // No natural API call occurred. Dump page info to check if app loaded.
    let page_info_js = r#"
JSON.stringify((function(){
  return {
    title: document.title,
    bodyLen: (document.body ? document.body.innerText.length : 0),
    bodyPreview: (document.body ? document.body.innerText.substring(0, 200) : ''),
    scriptCount: document.querySelectorAll('script').length,
    metaDesc: (document.querySelector('meta[name="description"]') || {}).content || '',
    url: location.href
  };
})());
"#;
    let info = sessions.execute_js(session_id, page_info_js).await?;
    tracing::warn!(page_info = %info.as_str().unwrap_or("?"), "Page state before probe");

    // Try triggering via fetch and XHR inside the page context.
    // The page's Statsig middleware should intercept and add x-statsig-id
    // to whichever API the app uses (fetch or XHR).
    tracing::info!("No natural API call detected; triggering probes from page JS");

    let trigger_js = r#"
(function(){
  window.__grokFetchDiagnostics = {
    fetchToString: window.fetch ? window.fetch.toString().substring(0, 200) : 'no fetch',
  };

  // Save the CURRENT window.fetch (which should include Statsig middleware
  // if the SDK loaded). Then wrap it to capture x-statsig-id.
  var _innerFetch = window.fetch.bind(window);
  window.__grokFetchDiagnostics.isWrapped = _innerFetch.toString().indexOf('native code') === -1;

  // Monkey-patch Headers.prototype.set to capture x-statsig-id when Statsig
  // sets it via the Headers object (some versions use headers.set() instead of
  // direct property assignment).
  var _origHeadersSet = Headers.prototype.set;
  Headers.prototype.set = function(name, value) {
    if (name.toLowerCase() === 'x-statsig-id' && !window.__grokCapture.statsigToken) {
      window.__grokCapture.statsigToken = value;
      if (window.__grokCapture.hashInput) {
        window.__grokCapture.done = true;
      }
    }
    return _origHeadersSet.call(this, name, value);
  };

  window.fetch = async function(input, init) {
    var url = (typeof input === 'string' ? input : (input && input.url)) || '';
    var isApiCall = url.indexOf('/rest/app-chat/') !== -1;

    if (!init) init = {};
    if (!init.headers) init.headers = {};

    // Synchronously call the inner chain first.
    var result = await _innerFetch(input, init);

    if (isApiCall) {
      // Capture x-statsig-id from whatever the Statsig middleware set.
      var statsigId = '';
      if (init.headers) {
        if (typeof init.headers.get === 'function') {
          statsigId = init.headers.get('x-statsig-id') || init.headers.get('X-Statsig-Id') || '';
        }
        if (!statsigId) {
          statsigId = init.headers['x-statsig-id'] || init.headers['X-Statsig-Id'] || '';
        }
      }
      if (!statsigId && input && input.headers) {
        if (typeof input.headers.get === 'function') {
          statsigId = input.headers.get('x-statsig-id') || input.headers.get('X-Statsig-Id') || '';
        }
        if (!statsigId) {
          statsigId = input.headers['x-statsig-id'] || input.headers['X-Statsig-Id'] || '';
        }
      }
      if (statsigId && !window.__grokCapture.statsigToken) {
        window.__grokCapture.statsigToken = statsigId;
        if (window.__grokCapture.hashInput) {
          window.__grokCapture.done = true;
        }
      }
      window.__grokFetchDiagnostics.capturedStatsigId = !!statsigId;
      window.__grokFetchDiagnostics.statsigId = statsigId;
    }

    return result;
  };

  // Trigger probes: POST to the chat endpoint to force Statsig to generate a token.
  (async function(){
    var url = 'https://grok.com/rest/app-chat/conversations/new';
    var body = JSON.stringify({ temporary: true, modelName: 'auto', message: 'probe' });

    window.__grokProbeFetchStatus = null;
    try {
      var resp = await fetch(url, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: body
      });
      window.__grokProbeFetchStatus = resp.status;
      window.__grokFetchDiagnostics.responseHeaders = {};
      resp.headers.forEach(function(v,k){ window.__grokFetchDiagnostics.responseHeaders[k] = v; });
    } catch(e) {
      window.__grokProbeFetchStatus = -1;
    }

    window.__grokProbeXhrStatus = null;
    try {
      var xhr = new XMLHttpRequest();
      xhr.open('POST', url);
      xhr.setRequestHeader('Content-Type', 'application/json');
      xhr.onload = function() { window.__grokProbeXhrStatus = xhr.status; };
      xhr.onerror = function() { window.__grokProbeXhrStatus = -1; };
      xhr.send(body);
    } catch(e) {
      window.__grokProbeXhrStatus = -2;
    }
  })();

  // Fallback: if Statsig SDK fails to evaluate, grok.com falls back to
  // btoa("x1:" + error.toString()). We detect this pattern and log it.
  window.__grokFallbackCheck = setInterval(function(){
    if (window.__grokCapture.statsigToken) {
      var tok = window.__grokCapture.statsigToken;
      try {
        var decoded = atob(tok);
        if (decoded.startsWith('x1:') || decoded.startsWith('e:')) {
          window.__grokFetchDiagnostics.usedFallback = true;
          window.__grokFetchDiagnostics.fallbackError = decoded.substring(3);
        }
      } catch(e) {}
      clearInterval(window.__grokFallbackCheck);
    }
  }, 100);
})();
"#;

    sessions
        .execute_js(session_id, trigger_js)
        .await?;

    // Wait for the probes to complete and check capture
    for i in 0..60 {
        sessions.pump_event_loop(session_id, 500).await?;

        let capture = sessions
            .execute_js(
                session_id,
                "JSON.stringify(window.__grokCapture || {})",
            )
            .await?;

        let cap = capture.as_str().unwrap_or("{}");
        if cap.contains("\"done\":true") || cap.contains("\"statsigToken\":\"") {
            let parsed: serde_json::Value =
                serde_json::from_str(cap).map_err(|e| {
                    GatewayError::Internal(format!("failed to parse capture: {e}"))
                })?;

            let token = parsed["statsigToken"]
                .as_str()
                .ok_or_else(|| {
                    GatewayError::Provider(
                        "Probe fetch succeeded but x-statsig-id was not captured. \
                         The middleware may not wrap window.fetch globally."
                            .to_string(),
                    )
                })?;

            let hash_input = parsed["hashInput"].as_str().unwrap_or("");

            let extracted = parse_extracted_data(token, hash_input)?;

            sessions.clear_preload_scripts(session_id).await?;
            sessions
                .navigate(session_id, "https://chat.deepseek.com")
                .await?;

            tracing::info!(
                header_hex = %extracted.header_hex,
                suffix = %extracted.suffix,
                trailer = %extracted.trailer,
                "Grok challenge constants extracted via probe"
            );

            return Ok(extracted);
        }

        // Log probe status and diagnostics for debugging
        if i % 5 == 0 {
            let status = sessions
                .execute_js(session_id, r#"JSON.stringify({
                  fetchStatus: window.__grokProbeFetchStatus,
                  xhrStatus: window.__grokProbeXhrStatus,
                  diagnostics: window.__grokFetchDiagnostics,
                  capture: (function(c){ return { hashInput: !!c.hashInput, statsigToken: !!c.statsigToken, done: c.done }; })(window.__grokCapture || {})
                })"#)
                .await?;
            tracing::warn!(i, probe_status = %status.as_str().unwrap_or("?"), "Probe status");
        }
    }

    // Last resort: try UI-driven extraction by typing a message and clicking send.
    // This uses the page's own JS to trigger the request, which is more likely to
    // activate the Statsig SDK.
    tracing::info!("Probes failed; trying UI-driven extraction");
    if let Some(extracted) = try_ui_driven_extraction(sessions, session_id).await? {
        sessions.clear_preload_scripts(session_id).await?;
        sessions
            .navigate(session_id, "https://chat.deepseek.com")
            .await?;

        tracing::info!(
            header_hex = %extracted.header_hex,
            suffix = %extracted.suffix,
            trailer = %extracted.trailer,
            "Grok challenge constants extracted via UI interaction"
        );

        return Ok(extracted);
    }

    Err(GatewayError::Provider(
        "Timed out waiting for grok.com to generate x-statsig-id. \
         Neither fetch nor XHR probes nor UI interaction captured the header. \
         Check the diagnostics log above to see if Statsig middleware is wrapping fetch. \
         If 'isWrapped' is false, the Statsig SDK may not load on this page. \
         If 'isWrapped' is true but no token was captured, the middleware may modify a \
         copy of init.headers rather than mutating the caller's object. \
         Make sure you are logged into grok.com in your browser and re-run."
            .to_string(),
    ))
}

/// Parse a real (XOR'd) x-statsig-id token and the captured hash input.
///
/// The XOR key is a single random byte. We brute-force it by picking the
/// K whose decoded counter (bytes 49-52) is closest to the expected
/// current-counter value.
fn parse_extracted_data(token_base64: &str, hash_input: &str) -> Result<ExtractedChallenge, GatewayError> {
    let raw = BASE64
        .decode(token_base64)
        .map_err(|e| GatewayError::Internal(format!("invalid x-statsig-id base64: {e}")))?;

    if raw.len() != 70 {
        return Err(GatewayError::Internal(format!(
            "expected 70-byte x-statsig-id, got {}",
            raw.len()
        )));
    }

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(EPOCH_SECS);
    let expected_counter = now_secs.saturating_sub(EPOCH_SECS);

    // Brute-force the XOR key (0..255): the correct key gives a decoded
    // counter within 10 seconds of our estimate.
    let (xor_key, counter) = (0..=255u8)
        .filter_map(|k| {
            let mut dec = [0u8; 70];
            dec.copy_from_slice(&raw);
            for b in &mut dec {
                *b ^= k;
            }
            let c = u32::from_le_bytes([dec[49], dec[50], dec[51], dec[52]]) as u64;
            let diff = if c > expected_counter {
                c - expected_counter
            } else {
                expected_counter - c
            };
            if diff < 10 {
                Some((k, c))
            } else {
                None
            }
        })
        .next()
        .ok_or_else(|| {
            GatewayError::Provider(
                "Could not determine XOR key from x-statsig-id token. \
                 Ensure you are logged into grok.com in your browser and re-run."
                    .to_string(),
            )
        })?;

    // Now unscramble the whole token with the found XOR key
    let mut unscrambled = [0u8; 70];
    unscrambled.copy_from_slice(&raw);
    for b in &mut unscrambled {
        *b ^= xor_key;
    }

    let header_hex = unscrambled[..49]
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    let trailer = unscrambled[69];

    let suffix = if hash_input.is_empty() {
        tracing::warn!("hash input was not captured; suffix will be empty");
        String::new()
    } else {
        let parts: Vec<&str> = hash_input.split('!').collect();
        if parts.len() >= 3 {
            let rest = parts[2..].join("!");
            // Remove the leading counter (integer, possibly negative)
            let trimmed = rest.trim_start_matches('-');
            let digit_end = trimmed.find(|c: char| !c.is_ascii_digit()).unwrap_or(0);
            String::from(&trimmed[digit_end..])
        } else {
            tracing::warn!(input = %hash_input, "unexpected hash input format");
            String::new()
        }
    };

    tracing::info!(
        xor_key,
        decoded_counter = counter,
        header_len = %header_hex.len(),
        suffix_len = suffix.len(),
        trailer,
        "x-statsig-id unscrambled"
    );

    Ok(ExtractedChallenge {
        header_hex,
        suffix,
        trailer,
    })
}

/// Try to extract challenge constants by interacting with the grok.com UI.
///
/// This is a last-resort method that types a message in the chat input and
/// clicks the send button. The page's own JS triggers the API request with
/// the Statsig SDK active, which is more likely to generate a valid token.
async fn try_ui_driven_extraction(
    sessions: &SessionManager,
    session_id: &str,
) -> Result<Option<ExtractedChallenge>, GatewayError> {
    // Wait for chat input to be available (try multiple selectors).
    let wait_for_input_js = r#"
(function(){
  var selectors = [
    'textarea[placeholder]',
    'textarea',
    '[contenteditable="true"]',
    '[data-testid="chat-input"]',
    '.chat-input',
    'input[type="text"]'
  ];
  for (var i = 0; i < selectors.length; i++) {
    var el = document.querySelector(selectors[i]);
    if (el) return JSON.stringify({ found: true, selector: selectors[i], tagName: el.tagName });
  }
  return JSON.stringify({ found: false });
})();
"#;

    let mut input_found = false;
    for i in 0..30 {
        sessions.pump_event_loop(session_id, 1000).await?;

        let result = sessions.execute_js(session_id, wait_for_input_js).await?;
        let info: serde_json::Value = serde_json::from_str(result.as_str().unwrap_or("{}"))
            .map_err(|e| GatewayError::Internal(format!("failed to parse input check: {e}")))?;

        if info["found"].as_bool().unwrap_or(false) {
            input_found = true;
            tracing::info!(selector = %info["selector"].as_str().unwrap_or("?"), "Chat input found");
            break;
        }

        if i % 5 == 0 {
            let page = sessions.execute_js(session_id, r#"JSON.stringify({
              title: document.title,
              url: location.href,
              bodyLen: document.body ? document.body.innerText.length : 0
            })"#).await?;
            tracing::debug!(i, page = %page.as_str().unwrap_or("?"), "Waiting for chat input");
        }
    }

    if !input_found {
        tracing::warn!("Chat input not found; cannot do UI-driven extraction");
        return Ok(None);
    }

    // Type a message and click send.
    let type_and_send_js = r#"
(function(){
  var selectors = [
    'textarea[placeholder]',
    'textarea',
    '[contenteditable="true"]',
    '[data-testid="chat-input"]',
    '.chat-input',
    'input[type="text"]'
  ];
  var input = null;
  for (var i = 0; i < selectors.length; i++) {
    input = document.querySelector(selectors[i]);
    if (input) break;
  }
  if (!input) return 'no input';

  // Focus and set value.
  input.focus();
  if (input.tagName === 'TEXTAREA' || input.tagName === 'INPUT') {
    var nativeInputValueSetter = Object.getOwnPropertyDescriptor(
      window.HTMLInputElement.prototype, 'value'
    ) || Object.getOwnPropertyDescriptor(
      window.HTMLTextAreaElement.prototype, 'value'
    );
    if (nativeInputValueSetter && nativeInputValueSetter.set) {
      nativeInputValueSetter.set.call(input, 'hello');
    } else {
      input.value = 'hello';
    }
    input.dispatchEvent(new Event('input', { bubbles: true }));
    input.dispatchEvent(new Event('change', { bubbles: true }));
  } else {
    input.textContent = 'hello';
    input.dispatchEvent(new Event('input', { bubbles: true }));
  }

  // Find and click send button.
  var sendSelectors = [
    'button[type="submit"]',
    '[data-testid="send-button"]',
    'button[aria-label="Send"]',
    'button svg[data-testid="send"]',
    'button:has(> svg)',
    '.send-button'
  ];
  for (var i = 0; i < sendSelectors.length; i++) {
    var btn = document.querySelector(sendSelectors[i]);
    if (btn) {
      btn.click();
      return 'sent';
    }
  }

  // Try keyboard shortcut (Enter).
  input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', code: 'Enter', keyCode: 13, bubbles: true }));
  input.dispatchEvent(new KeyboardEvent('keypress', { key: 'Enter', code: 'Enter', keyCode: 13, bubbles: true }));
  input.dispatchEvent(new KeyboardEvent('keyup', { key: 'Enter', code: 'Enter', keyCode: 13, bubbles: true }));

  return 'enter-key';
})();
"#;

    let send_result = sessions.execute_js(session_id, type_and_send_js).await?;
    let result_str = send_result.as_str().unwrap_or("unknown");
    tracing::info!(result = %result_str, "UI interaction result");

    // Wait for the request to complete and check capture.
    for i in 0..30 {
        sessions.pump_event_loop(session_id, 1000).await?;

        let capture = sessions
            .execute_js(session_id, "JSON.stringify(window.__grokCapture || {})")
            .await?;

        let cap = capture.as_str().unwrap_or("{}");
        if cap.contains("\"done\":true") || cap.contains("\"statsigToken\":\"") {
            let parsed: serde_json::Value = serde_json::from_str(cap)
                .map_err(|e| GatewayError::Internal(format!("failed to parse capture: {e}")))?;

            let token = parsed["statsigToken"].as_str().unwrap_or("");
            let hash_input = parsed["hashInput"].as_str().unwrap_or("");

            if token.is_empty() {
                continue;
            }

            match parse_extracted_data(token, hash_input) {
                Ok(extracted) => return Ok(Some(extracted)),
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to parse UI-driven extraction");
                    continue;
                }
            }
        }

        if i % 5 == 0 {
            let status = sessions.execute_js(session_id, r#"JSON.stringify({
              capture: (function(c){ return { hashInput: !!c.hashInput, statsigToken: !!c.statsigToken, done: c.done }; })(window.__grokCapture || {})
            })"#).await?;
            tracing::debug!(i, status = %status.as_str().unwrap_or("?"), "UI-driven extraction status");
        }
    }

    tracing::warn!("UI-driven extraction did not capture x-statsig-id");
    Ok(None)
}

fn hex_decode(hex: &str) -> Result<Vec<u8>, String> {
    if hex.len() % 2 != 0 {
        return Err("odd-length hex string".into());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}
