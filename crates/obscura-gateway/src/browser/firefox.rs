//! Firefox-specific DeepSeek authentication import.

use std::path::Path;

use obscura_net::CookieInfo;
use tracing::{info, warn};

use crate::error::GatewayError;

/// Decompress Firefox LSNG snappy-compressed value.
///
/// Firefox's LSNG format uses Snappy compression for values where
/// `compression_type = 1`. After decompression, the buffer contains
/// a `uint32_t` LE byte-length prefix followed by the payload.
/// The payload is usually plain UTF-8, occasionally UTF-16LE (legacy).
fn decode_lsng_value(value_bytes: &[u8], compression_type: i32) -> Option<String> {
    if compression_type == 1 {
        if let Ok(buf) = snap::raw::Decoder::new().decompress_vec(value_bytes) {
            // Case A: Decompressed data starts directly with UTF-8 text
            // (no length prefix — some Firefox versions omit it).
            if let Ok(s) = String::from_utf8(buf.clone()) {
                if let Some(c) = s.chars().next() {
                    if c.is_ascii_graphic() || c == ' ' || c == '\n' {
                        return Some(s);
                    }
                }
            }

            // Case B: 4-byte LE length prefix followed by UTF-8 data
            // (modern LSNG format). The prefix is the byte-length of the
            // payload; strip it and decode the rest as UTF-8.
            if buf.len() >= 4 {
                let payload = &buf[4..];
                if let Ok(s) = String::from_utf8(payload.to_vec()) {
                    if let Some(c) = s.chars().next() {
                        if c.is_ascii_graphic() || c == ' ' || c == '\n' {
                            return Some(s);
                        }
                    }
                }
            }

            // Case C: 4-byte LE length prefix followed by UTF-16LE data
            // (legacy Firefox format). Strip the prefix, decode as UTF-16LE.
            if buf.len() >= 6 {
                let (_, data) = buf.split_at(4);
                let utf16: Vec<u16> = data
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&c| c != 0)
                    .collect();
                if let Ok(s) = String::from_utf16(&utf16) {
                    return Some(s);
                }
            }
        }
    }

    // compression_type=0 (or snappy-fallback): the SQLite value column IS
    // the raw string bytes (UTF-8 or binary).
    // Try raw UTF-8 first.
    if let Ok(s) = String::from_utf8(value_bytes.to_vec()) {
        if let Some(c) = s.chars().next() {
            if c.is_ascii_graphic() || c == ' ' || c == '\n' {
                return Some(s);
            }
        }
    }

    // Last resort for compression_type=0: binary blob that might have a
    // 4-byte LE prefix followed by UTF-8 (some Firefox versions wrap
    // even uncompressed values with the length prefix).
    if value_bytes.len() >= 4 {
        let payload = &value_bytes[4..];
        if let Ok(s) = String::from_utf8(payload.to_vec()) {
            if let Some(c) = s.chars().next() {
                if c.is_ascii_graphic() || c == ' ' || c == '\n' {
                    return Some(s);
                }
            }
        }
    }

    None
}

use super::{
    is_relevant_domain, is_waf_cookie, LocalStorageEntry,
};
use super::firefox_nss::NssContext;
use super::firefox_profile;
use super::firefox_sqlite::FirefoxCookie;
use super::snapshot;

/// Auth data imported from Firefox.
#[derive(Debug, Clone)]
pub struct FirefoxAuth {
    pub cookies: Vec<CookieInfo>,
    pub local_storage: Vec<LocalStorageEntry>,
}

/// Import DeepSeek auth state from a Firefox profile directory.
pub fn import(profile_path: &Path) -> Result<FirefoxAuth, GatewayError> {
    let profile = if profile_path.is_dir() && profile_path.join("cookies.sqlite").exists() {
        profile_path.to_path_buf()
    } else {
        firefox_profile::find_default_profile()?
    };

    let snapshot = snapshot::snapshot_firefox_profile(&profile)?;
    let snapshot_path = snapshot.path().to_path_buf();

    let cookies = import_cookies(&snapshot_path)?;
    let local_storage = import_local_storage(&snapshot_path)?;

    Ok(FirefoxAuth {
        cookies,
        local_storage,
    })
}

/// Import DeepSeek cookies from a Firefox profile snapshot.
fn import_cookies(snapshot_path: &Path) -> Result<Vec<CookieInfo>, GatewayError> {
    let db_path = snapshot_path.join("cookies.sqlite");
    let cookies = super::firefox_sqlite::read_cookies(&db_path)?;

    let mut nss = NssContext::new()?;
    nss.init(snapshot_path)?;

    let mut imported = Vec::new();
    let mut decrypted_count = 0;
    let mut plaintext_count = 0;
    let mut skipped_count = 0;

    for cookie in cookies {
        if !is_relevant_domain(&cookie.host) {
            continue;
        }

        // Skip expired/stale anti-bot cookies. AWS WAF tokens are short-lived
        // and fingerprint-bound; importing an old token from a different
        // browser engine triggers the visible Turnstile challenge.
        if is_waf_cookie(&cookie.name) {
            skipped_count += 1;
            continue;
        }

        let value = match resolve_cookie_value(&cookie, &nss) {
            Some(v) => v,
            None => {
                skipped_count += 1;
                continue;
            }
        };

        if cookie.encrypted_value.is_some() {
            decrypted_count += 1;
        } else {
            plaintext_count += 1;
        }

        imported.push(to_cookie_info(&cookie, value));
    }

    let names: Vec<_> = imported.iter().map(|c| c.name.as_str()).collect();
    let has_token = imported.iter().any(|c| {
        super::DEEPSEEK_TOKEN_COOKIE_NAMES.contains(&c.name.as_str())
    });
    info!(
        count = imported.len(),
        decrypted = decrypted_count,
        plaintext = plaintext_count,
        skipped = skipped_count,
        names = ?names,
        has_token = has_token,
        "Imported cookies from Firefox"
    );

    Ok(imported)
}

/// Import provider localStorage from a Firefox profile snapshot.
///
/// Checks multiple known provider storage paths:
/// - `https+++chat.deepseek.com` (DeepSeek auth tokens)
/// - `https+++gemini.google.com` (Gemini user preferences, potential future auth)
fn import_local_storage(snapshot_path: &Path) -> Result<Vec<LocalStorageEntry>, GatewayError> {
    let mut all_entries = Vec::new();

    // DeepSeek localStorage
    let ds_path = snapshot_path.join("storage/default/https+++chat.deepseek.com/ls/data.sqlite");
    if ds_path.exists() {
        match import_from_ls_db(&ds_path, "DeepSeek", "https://chat.deepseek.com") {
            Ok(entries) => {
                let token_found = entries.iter().any(|e| e.key == "userToken" && e.as_deepseek_token().is_some());
                info!(
                    count = entries.len(),
                    has_token = token_found,
                    "Imported DeepSeek localStorage"
                );
                all_entries.extend(entries);
            }
            Err(e) => warn!(error = %e, path = %ds_path.display(), "Failed to import DeepSeek localStorage"),
        }
    } else {
        info!(path = %ds_path.display(), "No DeepSeek localStorage database found");
    }

    // Gemini localStorage
    let gm_path = snapshot_path.join("storage/default/https+++gemini.google.com/ls/data.sqlite");
    if gm_path.exists() {
        match import_from_ls_db(&gm_path, "Gemini", "https://gemini.google.com") {
            Ok(entries) => {
                info!(
                    count = entries.len(),
                    "Imported Gemini localStorage"
                );
                all_entries.extend(entries);
            }
            Err(e) => warn!(error = %e, path = %gm_path.display(), "Failed to import Gemini localStorage"),
        }
    }

    for (label, origin, storage_dir) in [
        ("Kimi", "https://www.kimi.com", "https+++www.kimi.com"),
        ("Kimi", "https://kimi.com", "https+++kimi.com"),
        ("Kimi", "https://kimi.moonshot.cn", "https+++kimi.moonshot.cn"),
        ("GLM", "https://chatglm.cn", "https+++chatglm.cn"),
        ("GLM", "https://chat.z.ai", "https+++chat.z.ai"),
        ("Claude", "https://claude.ai", "https+++claude.ai"),
        ("Qwen", "https://chat.qwen.ai", "https+++chat.qwen.ai"),
        ("Minimax", "https://agent.minimax.io", "https+++agent.minimax.io"),
    ] {
        let path = snapshot_path.join(format!("storage/default/{storage_dir}/ls/data.sqlite"));
        if !path.exists() {
            if label == "Minimax" {
                warn!(snapshot = %snapshot_path.display(), storage_dir = %storage_dir, "Minimax localStorage not found in snapshot");
            }
            continue;
        }
        match import_from_ls_db(&path, label, origin) {
            Ok(entries) => {
                info!(provider = label, count = entries.len(), "Imported provider localStorage");
                all_entries.extend(entries);
            }
            Err(e) => warn!(error = %e, path = %path.display(), provider = label, "Failed to import provider localStorage"),
        }
    }

    Ok(all_entries)
}

/// Read all entries from a single localStorage SQLite database.
///
/// Firefox's localStorage `data.sqlite` stores the `value` column as either:
/// - BLOB: Compressed data (compression_type=1) or raw binary
/// - TEXT: Plain string values (some providers like Kimi, GLM use this)
///
/// This function handles both cases gracefully by trying to read the value
/// in the format it was stored, without assuming a specific type.
fn import_from_ls_db(
    db_path: &std::path::Path,
    label: &str,
    origin: &str,
) -> Result<Vec<LocalStorageEntry>, GatewayError> {
    let conn = rusqlite::Connection::open(db_path).map_err(|e| {
        GatewayError::Internal(format!(
            "failed to open {label} localStorage database: {e}"
        ))
    })?;

    let mut stmt = conn
        .prepare("SELECT key, value, compression_type FROM data ORDER BY key")
        .map_err(|e| GatewayError::Internal(format!("failed to query localStorage: {e}")))?;

    let rows = stmt.query_map([], |row| {
        let key: String = row.get(0)?;
        let compression_type: i32 = row.get(2)?;

        // The value column can be stored as either BLOB or TEXT.
        // Try reading as BLOB first (most common — compressed or binary data).
        // If that fails, try reading as TEXT (used by some providers like Kimi, GLM).
        let value = match row.get::<_, Vec<u8>>(1) {
            Ok(value_bytes) => {
                // Value was stored as BLOB — decode it
                decode_lsng_value(&value_bytes, compression_type).unwrap_or_default()
            }
            Err(_) => {
                // Value was stored as TEXT — use it directly
                match row.get::<_, String>(1) {
                    Ok(text_value) => text_value,
                    Err(e) => {
                        // Neither BLOB nor TEXT worked — skip this entry
                        tracing::debug!(
                            key = %key,
                            error = %e,
                            "Failed to read localStorage value (neither BLOB nor TEXT)"
                        );
                        String::new()
                    }
                }
            }
        };

        Ok(LocalStorageEntry {
            origin: origin.to_string(),
            key,
            value,
        })
    });

    let mut entries = Vec::new();
    for row in rows.map_err(|e| GatewayError::Internal(format!("localStorage query failed: {e}")))? {
        let entry = row.map_err(|e| GatewayError::Internal(format!("localStorage row error: {e}")))?;
        entries.push(entry);
    }

    Ok(entries)
}

fn resolve_cookie_value(cookie: &FirefoxCookie, nss: &NssContext) -> Option<String> {
    // Firefox may store the value either in `value` (plaintext) or
    // `encryptedValue` (NSS-encrypted). Prefer the encrypted payload when
    // present, because the plaintext `value` column is often empty for
    // modern encrypted cookies.
    if let Some(ref encrypted) = cookie.encrypted_value {
        if !encrypted.is_empty() {
            return nss
                .decrypt(encrypted)
                .ok()
                .and_then(|v| String::from_utf8(v).ok());
        }
    }

    if !cookie.value.is_empty() {
        return Some(cookie.value.clone());
    }

    None
}

fn to_cookie_info(cookie: &FirefoxCookie, value: String) -> CookieInfo {
    let domain = cookie.host.trim_start_matches('.').to_lowercase();
    let same_site = match cookie.same_site {
        0 => "None",
        2 => "Strict",
        _ => "Lax",
    }
    .to_string();

    CookieInfo {
        name: cookie.name.clone(),
        value,
        domain,
        path: cookie.path.clone(),
        secure: cookie.is_secure,
        http_only: cookie.is_http_only,
        same_site,
        expires: cookie.expiry,
    }
}
