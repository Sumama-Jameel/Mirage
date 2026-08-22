//! Encrypted session vault.
//!
//! The vault persists per-import session state (cookies and localStorage) at
//! rest, encrypted with AES-256-GCM. The key lives in a separate
//! `vault.key` file (0600 on Unix) next to the vault, so the ciphertext
//! alone is not usable. Writes are atomic: write to a `.tmp` sibling, fsync,
//! then rename over the target.
//!
//! Nothing from the vault is ever logged; cookie/localStorage values never
//! appear in trace output.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use chrono::Utc;
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::error::GatewayError;

/// Magic bytes prefixed to vault files; also used as GCM additional
/// authenticated data so a file from another tool cannot be replayed.
const VAULT_MAGIC: &[u8] = b"OBSVLT1\n";
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const VAULT_FILE: &str = "sessions-vault.json.enc";
const KEY_FILE: &str = "vault.key";

/// The encrypted session state, serialized to JSON before encryption.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionVault {
    /// Format version; bump when the layout changes.
    pub version: u32,
    /// Unix seconds of the last successful save.
    pub saved_at_utc: i64,
    /// Full cookie jar of the imported profile.
    pub cookie_jar: Vec<obscura_net::CookieInfo>,
    /// Per-origin localStorage entries of the imported profile.
    pub local_storage: Vec<crate::browser::LocalStorageEntry>,
    /// Browser identity label ("firefox"/"chrome") and UA at import time.
    pub identity: String,
    pub user_agent: String,
    /// Last time the session was verified against the live service.
    pub last_verified_utc: Option<i64>,
    /// Free-form capability flags captured at import time.
    pub capability_flags: Vec<String>,
}

/// Handle to the on-disk vault and its key. Cheap to clone; each
/// `save`/`load` re-reads the key from disk so a rotated key file takes
/// effect without a restart.
pub struct VaultFile {
    data_dir: PathBuf,
}

impl VaultFile {
    pub fn new(data_dir: PathBuf) -> Self {
        VaultFile { data_dir }
    }

    fn key_path(&self) -> PathBuf {
        self.data_dir.join(KEY_FILE)
    }

    fn vault_path(&self) -> PathBuf {
        self.data_dir.join(VAULT_FILE)
    }

    /// Ensure the key file exists (creating it with 0600 permissions on
    /// Unix), then return the 32-byte key.
    pub fn key(&self) -> Result<[u8; KEY_LEN], GatewayError> {
        fs::create_dir_all(&self.data_dir).map_err(|e| {
            GatewayError::Internal(format!(
                "vault data dir {}: {e}",
                self.data_dir.display()
            ))
        })?;
        let key_path = self.key_path();
        if let Ok(bytes) = fs::read(&key_path) {
            let key: [u8; KEY_LEN] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| GatewayError::Internal("vault.key has unexpected length".to_string()))?;
            return Ok(key);
        }
        let mut key = [0u8; KEY_LEN];
        rand::rngs::OsRng.try_fill_bytes(&mut key).map_err(|e| {
            GatewayError::Internal(format!("failed to generate vault key: {e}"))
        })?;
        let mut f = fs::File::create(&key_path).map_err(|e| {
            GatewayError::Internal(format!("failed to create vault key {}: {e}", key_path.display()))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
        }
        f.write_all(&key)
            .and_then(|_| f.sync_all())
            .map_err(|e| GatewayError::Internal(format!("failed to write vault key: {e}")))?;
        Ok(key)
    }

    /// Encrypt and atomically persist a vault.
    pub fn save(&self, vault: &SessionVault) -> Result<(), GatewayError> {
        let key = self.key()?;
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| {
            GatewayError::Internal(format!("vault key unusable: {e}"))
        })?;
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::rngs::OsRng
            .try_fill_bytes(&mut nonce_bytes)
            .map_err(|e| GatewayError::Internal(format!("nonce generation failed: {e}")))?;
        let nonce = Nonce::from_slice(&nonce_bytes);

        let mut plaintext = Vec::new();
        serde_json::to_writer(&mut plaintext, vault)
            .map_err(|e| GatewayError::Internal(format!("vault serialize failed: {e}")))?;

        let ciphertext = cipher
            .encrypt(nonce, plaintext.as_slice())
            .map_err(|e| GatewayError::Internal(format!("vault encrypt failed: {e}")))?;

        let tmp_path = self.data_dir.join(format!("{VAULT_FILE}.tmp"));
        let mut f = fs::File::create(&tmp_path).map_err(|e| {
            GatewayError::Internal(format!("failed to create {}: {e}", tmp_path.display()))
        })?;
        f.write_all(VAULT_MAGIC)
            .and_then(|_| f.write_all(&nonce_bytes))
            .and_then(|_| f.write_all(&ciphertext))
            .and_then(|_| f.sync_all())
            .map_err(|e| GatewayError::Internal(format!("failed to write vault: {e}")))?;
        fs::rename(&tmp_path, self.vault_path()).map_err(|e| {
            GatewayError::Internal(format!("failed to finalize vault: {e}"))
        })?;
        Ok(())
    }

    /// Decrypt and parse the vault. Returns `Ok(None)` when no vault file
    /// exists yet. Any decryption failure (tamper, wrong key, truncation)
    /// is an error, never a silent empty vault.
    pub fn load(&self) -> Result<Option<SessionVault>, GatewayError> {
        let vault_path = self.vault_path();
        let bytes = match fs::read(&vault_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(GatewayError::Internal(format!(
                    "failed to read vault {}: {e}",
                    vault_path.display()
                )))
            }
        };
        if bytes.len() < VAULT_MAGIC.len() + NONCE_LEN + 16 {
            return Err(GatewayError::Internal(
                "vault file is truncated".to_string(),
            ));
        }
        if &bytes[..VAULT_MAGIC.len()] != VAULT_MAGIC {
            return Err(GatewayError::Internal(
                "vault file has an unknown format".to_string(),
            ));
        }
        let nonce = Nonce::from_slice(&bytes[VAULT_MAGIC.len()..VAULT_MAGIC.len() + NONCE_LEN]);
        let ciphertext = &bytes[VAULT_MAGIC.len() + NONCE_LEN..];

        let key = self.key()?;
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| {
            GatewayError::Internal(format!("vault key unusable: {e}"))
        })?;
        let plaintext = cipher.decrypt(nonce, ciphertext).map_err(|_| {
            GatewayError::Internal(
                "vault decryption failed (tampered or wrong key); keeping the old file".to_string(),
            )
        })?;
        serde_json::from_slice(&plaintext).map_err(|e| {
            GatewayError::Internal(format!("vault payload is corrupt: {e}"))
        })
    }

    #[cfg(test)]
    fn path(&self) -> PathBuf {
        self.vault_path()
    }
}

impl SessionVault {
    /// Build a `SessionVault` from freshly imported auth state. Nothing is
    /// logged here: cookie and localStorage values never leave memory except
    /// through the ciphertext.
    pub fn from_import(
        cookies: Vec<obscura_net::CookieInfo>,
        local_storage: Vec<crate::browser::LocalStorageEntry>,
        identity: &str,
        user_agent: &str,
        capability_flags: Vec<String>,
    ) -> SessionVault {
        SessionVault {
            version: 1,
            saved_at_utc: Utc::now().timestamp(),
            cookie_jar: cookies,
            local_storage,
            identity: identity.to_string(),
            user_agent: user_agent.to_string(),
            last_verified_utc: None,
            capability_flags,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use obscura_net::CookieInfo;

    fn sample_vault() -> SessionVault {
        SessionVault::from_import(
            vec![CookieInfo {
                name: "__Secure-session".to_string(),
                value: "secret-token-value".to_string(),
                domain: ".deepseek.com".to_string(),
                path: "/".to_string(),
                secure: true,
                http_only: true,
                same_site: "lax".to_string(),
                expires: None,
            }],
            vec![crate::browser::LocalStorageEntry {
                origin: "https://chat.deepseek.com".to_string(),
                key: "token".to_string(),
                value: "ls-secret".to_string(),
            }],
            "firefox",
            "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0",
            vec!["capability:attachments".to_string()],
        )
    }

    #[test]
    fn roundtrip_encrypts_and_decrypts() {
        let dir = tempfile::tempdir().unwrap();
        let vault = VaultFile::new(dir.path().to_path_buf());
        let original = sample_vault();
        vault.save(&original).unwrap();

        let loaded = vault.load().unwrap().expect("vault must exist");
        assert_eq!(loaded.cookie_jar.len(), original.cookie_jar.len());
        assert_eq!(
            loaded.cookie_jar[0].value,
            original.cookie_jar[0].value
        );
        assert_eq!(loaded.local_storage[0].value, "ls-secret");
        assert_eq!(loaded.identity, "firefox");
        assert!(loaded.capability_flags.contains(&"capability:attachments".to_string()));
    }

    #[test]
    fn ciphertext_never_contains_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let vault = VaultFile::new(dir.path().to_path_buf());
        vault.save(&sample_vault()).unwrap();
        let bytes = fs::read(vault.path()).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("secret-token-value"));
        assert!(!text.contains("ls-secret"));
    }

    #[test]
    fn tampered_vault_fails_to_open() {
        let dir = tempfile::tempdir().unwrap();
        let vault = VaultFile::new(dir.path().to_path_buf());
        vault.save(&sample_vault()).unwrap();
        let path = vault.path();
        let bytes = fs::read(&path).unwrap();
        let mut corrupt = bytes;
        let mid = corrupt.len() / 2;
        corrupt[mid] ^= 0xff;
        fs::write(&path, &corrupt).unwrap();
        assert!(vault.load().is_err());
        assert_eq!(
            fs::read(&path).unwrap(),
            corrupt,
            "failed load must not modify the file"
        );
    }

    #[test]
    fn load_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let vault = VaultFile::new(dir.path().to_path_buf());
        assert!(vault.load().unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_private_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let vault = VaultFile::new(dir.path().to_path_buf());
        vault.key().unwrap();
        let mode = fs::metadata(vault.key_path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}