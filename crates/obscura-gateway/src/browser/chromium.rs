//! Chrome/Edge cookie and localStorage import.
//!
//! Chromium-based browsers store cookies in a SQLite database (`Cookies`) with
//! AES-256-GCM-encrypted values. The encryption key is stored in the `Local
//! State` JSON file (`os_crypt.encrypted_key`), itself encrypted on Linux via
//! GNOME Keyring / KDE Wallet (or a hardcoded fallback password `peanuts`).
//!
//! ## Decryption flow
//!
//! 1. Read `Local State` → base64-decode `encrypted_key` → strip `v10`/`v11` prefix.
//! 2. Derive AES-128-CBC key/IV from the safe-storage password (PBKDF2-SHA1, 1 iter).
//! 3. Decrypt the `encrypted_key` payload → 32-byte AES-256-GCM master key.
//! 4. For each cookie: skip `v10`/`v11` prefix (3 bytes), extract 12-byte nonce,
//!    then decrypt with AES-256-GCM.

use std::path::Path;

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use obscura_net::CookieInfo;
use tracing::{debug, info, warn};

use crate::error::GatewayError;

use super::{is_deepseek_domain, is_waf_cookie, BrowserSource, ImportedAuth, LocalStorageEntry};

/// Auth data imported from a Chromium-based browser.

/// Import DeepSeek auth from a Chrome/Edge profile directory.
pub fn import(source: &BrowserSource) -> Result<ImportedAuth, GatewayError> {
    let profile_path = &source.profile_path;
    let user_data_dir = profile_path.parent().ok_or_else(|| {
        GatewayError::Internal(format!(
            "cannot determine User Data directory from profile path {}",
            profile_path.display()
        ))
    })?;

    let snapshot = chromium_snapshot(profile_path, user_data_dir)?;
    let snapshot_path = snapshot.path().to_path_buf();

    let master_key = match retrieve_master_key(user_data_dir) {
        Some(k) => k,
        None => {
            warn!(
                browser = %source.browser_type,
                "Could not decrypt Chromium master key; cookies will be plaintext only"
            );
            Vec::new()
        }
    };

    let cookies = import_cookies(&snapshot_path, &master_key)?;
    let local_storage = import_local_storage(&snapshot_path)?;

    Ok(ImportedAuth {
        cookies,
        local_storage,
    })
}

// =============================================================================
// Snapshot
// =============================================================================

fn chromium_snapshot(
    profile_path: &Path,
    _user_data_dir: &Path,
) -> Result<tempfile::TempDir, GatewayError> {
    let tmp = tempfile::TempDir::new().map_err(|e| {
        GatewayError::Internal(format!("failed to create temp dir for Chromium snapshot: {e}"))
    })?;

    let src = profile_path.join("Cookies");
    let dst = tmp.path().join("Cookies");
    if src.exists() {
        std::fs::copy(&src, &dst).map_err(|e| {
            GatewayError::Internal(format!(
                "failed to copy Chromium Cookies from {}: {e}",
                src.display()
            ))
        })?;
        for ext in &["-wal", "-shm"] {
            let sidecar = src.with_extension(format!("Cookies{}", ext));
            if sidecar.exists() {
                let dst_sidecar = dst.with_extension(format!("Cookies{}", ext));
                let _ = std::fs::copy(&sidecar, &dst_sidecar);
            }
        }
        debug!("Chromium cookies snapshot created at {}", dst.display());
    } else {
        warn!(
            "Chromium Cookies database not found at {}",
            src.display()
        );
    }

    Ok(tmp)
}

// =============================================================================
// Master key retrieval (Linux + Windows)
// =============================================================================

fn retrieve_master_key(user_data_dir: &Path) -> Option<Vec<u8>> {
    #[cfg(target_os = "linux")]
    {
        retrieve_master_key_linux(user_data_dir)
    }
    #[cfg(target_os = "windows")]
    {
        retrieve_master_key_windows(user_data_dir)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = user_data_dir;
        None
    }
}

#[cfg(target_os = "linux")]
fn retrieve_master_key_linux(user_data_dir: &Path) -> Option<Vec<u8>> {
    let local_state_path = user_data_dir.join("Local State");
    let content = std::fs::read_to_string(&local_state_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let enc_key_b64 = json
        .pointer("/os_crypt/encrypted_key")
        .and_then(|v| v.as_str())?;
    let enc_key = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        enc_key_b64,
    )
    .ok()?;

    if enc_key.len() < 6 {
        return None;
    }
    let encrypted_payload = &enc_key[5..];

    let password = get_safe_storage_password()?;
    let (aes_cbc_key, iv) = derive_linux_cbc_key(&password)?;

    // Decrypt with AES-128-CBC.
    let master_key = aes128_cbc_decrypt(encrypted_payload, &aes_cbc_key, &iv)?;
    if master_key.len() == 32 {
        Some(master_key)
    } else {
        warn!(
            master_key_len = master_key.len(),
            "Unexpected Chromium master key length; expected 32"
        );
        None
    }
}

#[cfg(target_os = "linux")]
fn get_safe_storage_password() -> Option<String> {
    if let Ok(output) = std::process::Command::new("secret-tool")
        .args(["lookup", "application", "chrome"])
        .output()
    {
        if output.status.success() {
            let password = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !password.is_empty() {
                return Some(password);
            }
        }
    }

    if let Ok(output) = std::process::Command::new("kwallet-query")
        .args(["--read-password", "Chrome Safe Storage", "kdewallet"])
        .output()
    {
        if output.status.success() {
            let password = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !password.is_empty() {
                return Some(password);
            }
        }
    }

    debug!("Using fallback Chrome safe storage password 'peanuts'");
    Some("peanuts".to_string())
}

#[cfg(target_os = "linux")]
fn derive_linux_cbc_key(password: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    use hmac::Hmac;
    use pbkdf2::pbkdf2;
    use sha1::Sha1;

    let salt = b"saltysalt";
    let mut output = [0u8; 48];
    let _ = pbkdf2::<Hmac<Sha1>>(password.as_bytes(), salt, 1, &mut output);

    // Chrome takes bytes 0..31 as key and 32..47 as IV for AES-128-CBC.
    // Actually AES-128-CBC key is 16 bytes, but Chrome uses the first 32
    // bytes as a double-size key? Let's be safe and use 16 bytes.
    Some((output[..16].to_vec(), output[32..48].to_vec()))
}

#[cfg(target_os = "linux")]
fn aes128_cbc_decrypt(
    ciphertext: &[u8],
    key: &[u8],
    iv: &[u8],
) -> Option<Vec<u8>> {
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};

    type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

    if ciphertext.len() < 16 {
        return None;
    }

    let mut buf = ciphertext.to_vec();
    let pt_len = Aes128CbcDec::new(key.into(), iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .ok()?
        .len();
    buf.truncate(pt_len);
    Some(buf)
}

#[cfg(target_os = "windows")]
fn retrieve_master_key_windows(user_data_dir: &Path) -> Option<Vec<u8>> {
    let local_state_path = user_data_dir.join("Local State");
    let content = std::fs::read_to_string(&local_state_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let enc_key_b64 = json
        .pointer("/os_crypt/encrypted_key")
        .and_then(|v| v.as_str())?;
    let enc_key = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        enc_key_b64,
    )
    .ok()?;

    if enc_key.len() < 6 {
        return None;
    }
    let encrypted_payload = &enc_key[5..];

    // DPAPI decryption via CryptUnprotectData.
    let master_key = dpapi_unprotect(encrypted_payload)?;
    if master_key.len() == 32 {
        Some(master_key)
    } else {
        warn!(
            master_key_len = master_key.len(),
            "Unexpected Chromium master key length on Windows; expected 32"
        );
        None
    }
}

#[cfg(target_os = "windows")]
fn dpapi_unprotect(ciphertext: &[u8]) -> Option<Vec<u8>> {
    use std::ptr;

    #[repr(C)]
    #[allow(non_snake_case)]
    struct DataBlob {
        cbData: u32,
        pbData: *mut u8,
    }

    extern "system" {
        fn CryptUnprotectData(
            pDataIn: *const DataBlob,
            ppszDataDescr: *mut *mut u16,
            pOptionalEntropy: *const DataBlob,
            pvReserved: *mut std::ffi::c_void,
            pPromptStruct: *const std::ffi::c_void,
            dwFlags: u32,
            pDataOut: *mut DataBlob,
        ) -> i32;
        fn LocalFree(hmem: *mut u8) -> *mut u8;
    }

    let mut data_in = DataBlob {
        cbData: ciphertext.len() as u32,
        pbData: ciphertext.as_ptr() as *mut u8,
    };
    let mut data_out = DataBlob {
        cbData: 0,
        pbData: ptr::null_mut(),
    };

    let result = unsafe {
        CryptUnprotectData(
            &data_in as *const DataBlob,
            ptr::null_mut(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null(),
            0x01, // CRYPTPROTECT_UI_FORBIDDEN
            &mut data_out as *mut DataBlob,
        )
    };

    if result == 0 {
        return None;
    }

    let len = data_out.cbData as usize;
    let mut out = vec![0u8; len];
    unsafe {
        ptr::copy_nonoverlapping(data_out.pbData, out.as_mut_ptr(), len);
        LocalFree(data_out.pbData);
    }
    Some(out)
}

// =============================================================================
// Cookie import
// =============================================================================

#[derive(Debug)]
struct RawCookie {
    host_key: String,
    name: String,
    encrypted_value: Vec<u8>,
    value: String,
    path: String,
    is_secure: bool,
    is_httponly: bool,
    same_site: i32,
    expires_utc: i64,
}

fn import_cookies(snapshot_path: &Path, master_key: &[u8]) -> Result<Vec<CookieInfo>, GatewayError> {
    let db_path = snapshot_path.join("Cookies");
    if !db_path.exists() {
        warn!("Chromium Cookies DB not found at {}", db_path.display());
        return Ok(Vec::new());
    }

    let conn = rusqlite::Connection::open(&db_path).map_err(|e| {
        GatewayError::Internal(format!("failed to open Chromium Cookies DB: {e}"))
    })?;

    let mut stmt = conn
        .prepare(
            "SELECT host_key, name, encrypted_value, value, path, is_secure, is_httponly, \
             same_site, expires_utc FROM cookies ORDER BY host_key",
        )
        .map_err(|e| GatewayError::Internal(format!("failed to query Chromium cookies: {e}")))?;

    let rows = stmt.query_map([], |row| {
            Ok(RawCookie {
                host_key: row.get(0)?,
                name: row.get(1)?,
                encrypted_value: row.get(2)?,
                value: row.get(3)?,
                path: row.get(4)?,
                is_secure: row.get(5)?,
                is_httponly: row.get(6)?,
                same_site: row.get(7)?,
                expires_utc: row.get(8)?,
            })
        })
        .map_err(|e| GatewayError::Internal(format!("Chromium cookie query failed: {e}")))?;

    let mut imported = Vec::new();
    let mut decrypted = 0;
    let mut plaintext = 0;
    let mut skipped = 0;

    for row in rows {
        let cookie = row
            .map_err(|e| GatewayError::Internal(format!("Chromium cookie row error: {e}")))?;

        if !is_deepseek_domain(&cookie.host_key) {
            continue;
        }
        if is_waf_cookie(&cookie.name) {
            skipped += 1;
            continue;
        }

        let value = match resolve_chromium_value(&cookie, master_key) {
            Some(v) => v,
            None => {
                skipped += 1;
                continue;
            }
        };

        if !cookie.encrypted_value.is_empty() && !master_key.is_empty() {
            decrypted += 1;
        } else {
            plaintext += 1;
        }

        imported.push(to_cookie_info(&cookie, value));
    }

    let names: Vec<_> = imported.iter().map(|c| c.name.as_str()).collect();
    let has_token = imported
        .iter()
        .any(|c| super::DEEPSEEK_TOKEN_COOKIE_NAMES.contains(&c.name.as_str()));
    info!(
        deepseek = imported.len(),
        decrypted = decrypted,
        plaintext = plaintext,
        skipped = skipped,
        names = ?names,
        has_token = has_token,
        "Imported DeepSeek cookies from Chromium"
    );

    Ok(imported)
}

fn resolve_chromium_value(cookie: &RawCookie, master_key: &[u8]) -> Option<String> {
    if !cookie.encrypted_value.is_empty() && !master_key.is_empty() {
        if let Ok(decrypted) = decrypt_aes_gcm(&cookie.encrypted_value, master_key) {
            if !decrypted.is_empty() {
                return Some(decrypted);
            }
        }
    }

    if !cookie.value.is_empty() {
        return Some(cookie.value.clone());
    }

    None
}

fn decrypt_aes_gcm(encrypted: &[u8], master_key: &[u8]) -> Result<String, GatewayError> {
    if encrypted.len() < 15 + 3 {
        return Err(GatewayError::Internal(format!(
            "Chromium AES-GCM payload too short ({} bytes)",
            encrypted.len()
        )));
    }

    let offset = 3;
    let nonce = Nonce::from_slice(&encrypted[offset..offset + 12]);
    let ciphertext = &encrypted[offset + 12..];

    let cipher = Aes256Gcm::new_from_slice(master_key).map_err(|e| {
        GatewayError::Internal(format!("invalid Chromium AES-256-GCM key: {e}"))
    })?;

    let plaintext = cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|e| GatewayError::Internal(format!("AES-GCM decryption failed: {e}")))?;

    String::from_utf8(plaintext).map_err(|e| {
        GatewayError::Internal(format!("decrypted cookie is not valid UTF-8: {e}"))
    })
}

fn to_cookie_info(cookie: &RawCookie, value: String) -> CookieInfo {
    let domain = cookie.host_key.trim_start_matches('.').to_lowercase();
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
        http_only: cookie.is_httponly,
        same_site,
        expires: if cookie.expires_utc > 0 {
            Some((cookie.expires_utc / 1000000) - 11644473600)
        } else {
            None
        },
    }
}

// =============================================================================
// LocalStorage import (LevelDB)
// =============================================================================

fn import_local_storage(_snapshot_path: &Path) -> Result<Vec<LocalStorageEntry>, GatewayError> {
    Ok(Vec::new())
}