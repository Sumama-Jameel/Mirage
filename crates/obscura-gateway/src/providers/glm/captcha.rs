#![allow(dead_code)]
//! GLM/Z.AI captcha reverse-engineering reference — NOT used at runtime.
//!
//! Z.AI serves an Aliyun CaptchaV3 (NVC — No Verification Code) challenge
//! when `features.enable_captcha` is set.  This module documents how the
//! Aliyun CaptchaV3 INIT API works, but the `CertifyId` it returns is NOT
//! sufficient to satisfy Z.AI's captcha check — the Aliyun captcha widget
//! must also run in a browser to "complete" the verification session.
//!
//! Aliyun TRACELESS captcha uses a two-phase flow:
//!   1. `InitCaptchaV3` (server-side, what this module does) → CertifyId.
//!   2. Captcha widget JS (loaded from `CaptchaJsPath`) runs in a browser
//!      and sends device-integrity proof to Aliyun to mark the session as
//!      verified.
//!
//! Without step 2, Aliyun's `VerifyCaptchaV3` always returns `F002`/`F019`,
//! and Z.AI rejects the token.
//!
//! The direct API path in `direct.rs` therefore skips captcha entirely and
//! lets Z.AI's captcha error trigger a fallback to the UI-automation path
//! (which runs the captcha widget inside the headless browser).
//!
//! The Aliyun Captcha SDK ships with a built-in Aliyun RAM AccessKey pair
//! (`LTAI5tSEBwYMwVKAQGpxmvTd` / `YSKfst7GaVkXwZYvVihJsKF9r89koz`) that
//! is used by all websites that embed the captcha.  We extract these keys
//! from the SDK JS bundle and use them to sign the INIT request.
//!
//! Flow (reference only):
//! 1. Build signed request params (HMAC-SHA1, Aliyun REST API V3 signing).
//! 2. POST to `https://{prefix}.captcha-open-aliyuncs.com/`.
//! 3. Parse `CertifyId` from the JSON response.
//! 4. Construct `captcha_verify_param = base64({certifyId, sceneId, isSign})`.
//!    (Step 4 produces a syntactically valid token, but the CertifyId is
//!    unverified and gets rejected with `verify_code: F019`.)

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use tracing::{debug, warn};

use crate::error::GatewayError;

/// Aliyun AccessKey credentials embedded in the CaptchaV3 SDK.
const ACCESS_KEY_ID: &str = "LTAI5tSEBwYMwVKAQGpxmvTd";
const ACCESS_KEY_SECRET: &str = "YSKfst7GaVkXwZYvVihJsKF9r89koz";

/// Z.AI's captcha scene identifier and identity prefix.
const SCENE_ID: &str = "didk33e0";
const PREFIX: &str = "no8xfe";

/// Aliyun Captcha API version (from the SDK's `pt` constant).
const API_VERSION: &str = "2023-03-05";

/// Base URL for the Aliyun captcha INIT endpoint.
///
/// The Aliyun Captcha SDK sends INIT requests to
/// `https://{prefix}.captcha-open.aliyuncs.com/` for all regions.  The
/// region-specific `captcha-open-{suffix}.aliyuncs.com` variants serve a
/// different (newer) API that does not accept the query-param `Action`
/// signing scheme, so we always use the non-suffixed domain.
fn captcha_endpoint() -> String {
    format!("https://{}.captcha-open.aliyuncs.com/", PREFIX)
}

/// Aliyun-specific percent-encoding for API signing.
///
/// Unlike standard `urlencode`, this keeps `A-Z`, `a-z`, `0-9`, `-`, `_`,
/// `.`, and `~` unencoded, encodes spaces as `%20` (not `+`), and
/// percent-encodes everything else.
fn aliyun_percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 2);
    for &byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

/// Compute the Aliyun REST API V3 signature.
///
/// `StringToSign = HTTPMethod + "&" + percent-encode("/") + "&" + percent-encode(canonicalized-query)`
/// `Signature = Base64(HMAC-SHA1(StringToSign, Secret + "&"))`
fn aliyun_signature(params: &BTreeMap<String, String>, secret: &str) -> String {
    let canonical = params
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                aliyun_percent_encode(k),
                aliyun_percent_encode(v)
            )
        })
        .collect::<Vec<_>>()
        .join("&");

    let string_to_sign = format!(
        "POST&{}&{}",
        aliyun_percent_encode("/"),
        aliyun_percent_encode(&canonical),
    );

    let key = format!("{}&", secret);
    type HmacSha1 = Hmac<Sha1>;
    let mut mac =
        HmacSha1::new_from_slice(key.as_bytes()).expect("HMAC key is valid UTF-8");
    mac.update(string_to_sign.as_bytes());
    let result = mac.finalize().into_bytes();

    base64::engine::general_purpose::STANDARD.encode(result)
}

/// ISO 8601 timestamp for the Aliyun API (e.g. `2025-01-15T10:30:00Z`).
fn iso_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Manual calculation to avoid pulling in chrono just for this.
    // Days since epoch.
    let days = secs / 86_400;
    let time_secs = secs % 86_400;

    // Gregorian calendar date from days since 1970-01-01.
    let mut y = 1970i64;
    let mut remaining = days as i64;

    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }

    let months_days = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut m = 0usize;
    for (i, &md) in months_days.iter().enumerate() {
        if remaining < md as i64 {
            m = i;
            break;
        }
        remaining -= md as i64;
    }
    let d = remaining + 1;

    let h = time_secs / 3600;
    let mi = (time_secs % 3600) / 60;
    let s = time_secs % 60;

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m + 1,
        d,
        h,
        mi,
        s
    )
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Generate a random UUID for `SignatureNonce`.
fn signature_nonce() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Try to obtain an Aliyun CaptchaV3 token via direct API call.
///
/// Returns `Ok(Some(token))` on success, `Ok(None)` when the captcha could
/// not be satisfied, or `Err(...)` on network/parse failures.
pub async fn try_solve_captcha() -> Result<Option<String>, GatewayError> {
    let url = captcha_endpoint();
    let timestamp = iso_timestamp();
    let nonce = signature_nonce();

    // Build request params WITHOUT the Signature.
    let mut params = BTreeMap::new();
    params.insert("AccessKeyId".to_string(), ACCESS_KEY_ID.to_string());
    params.insert("Action".to_string(), "InitCaptchaV3".to_string());
    params.insert("Format".to_string(), "JSON".to_string());
    params.insert("SceneId".to_string(), SCENE_ID.to_string());
    params.insert("SignatureMethod".to_string(), "HMAC-SHA1".to_string());
    params.insert("SignatureNonce".to_string(), nonce);
    params.insert("SignatureVersion".to_string(), "1.0".to_string());
    params.insert("Timestamp".to_string(), timestamp);
    params.insert("Version".to_string(), API_VERSION.to_string());

    // Compute signature.
    let signature = aliyun_signature(&params, ACCESS_KEY_SECRET);
    params.insert("Signature".to_string(), signature);

    // POST form-urlencoded.
    let body = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencoding(v)))
        .collect::<Vec<_>>()
        .join("&");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| GatewayError::Internal(format!("captcha HTTP client: {e}")))?;

    let resp = client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded; charset=UTF-8")
        .body(body)
        .send()
        .await
        .map_err(|e| {
            warn!(error = %e, "Aliyun captcha INIT request failed");
            GatewayError::Provider(format!("captcha INIT request failed: {e}"))
        })?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        warn!(status = %status, body = %text.chars().take(200).collect::<String>(), "Aliyun captcha INIT returned error");
        return Ok(None);
    }

    let json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, body = %text.chars().take(200).collect::<String>(), "captcha INIT response is not valid JSON");
            return Ok(None);
        }
    };

    let certify_id = match json.get("CertifyId").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => {
            warn!(
                body = %text.chars().take(300).collect::<String>(),
                "captcha INIT response missing CertifyId"
            );
            return Ok(None);
        }
    };

    // Build captcha_verify_param token.
    let token_payload = serde_json::json!({
        "certifyId": certify_id,
        "sceneId": SCENE_ID,
        "isSign": true,
    });
    let token = base64::engine::general_purpose::STANDARD
        .encode(token_payload.to_string().as_bytes());

    debug!(token_len = token.len(), certify_id = %certify_id, "Aliyun captcha token obtained via direct API");
    Ok(Some(token))
}

/// Standard form-urlencoding for the POST body (not the Aliyun signing encoding).
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}
