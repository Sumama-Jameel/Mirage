use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD as BASE64;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds between Unix epoch and May 1, 2023 00:00:00 UTC.
const EPOCH: u64 = 1_682_924_400;

const HEADER_LEN: usize = 49;
const COUNTER_BYTES: usize = 4;
const HASH_BYTES: usize = 16;
const TRAILER_BYTES: usize = 1;
const TOKEN_LEN: usize = HEADER_LEN + COUNTER_BYTES + HASH_BYTES + TRAILER_BYTES;

#[derive(Clone)]
pub struct ChallengeConfig {
    header: [u8; HEADER_LEN],
    suffix: String,
    trailer: u8,
}

impl ChallengeConfig {
    pub fn new(header: [u8; HEADER_LEN], suffix: String, trailer: u8) -> Self {
        Self {
            header,
            suffix,
            trailer,
        }
    }

    pub fn header(&self) -> &[u8; HEADER_LEN] {
        &self.header
    }

    pub fn suffix(&self) -> &str {
        &self.suffix
    }

    pub fn trailer(&self) -> u8 {
        self.trailer
    }

    /// Generate an x-statsig-id token identical to what the browser's
    /// Statsig SDK produces.
    ///
    /// Token layout (70 bytes, base64-encoded):
    ///   header[49] + counter_le32[4] + sha256(...)[0..16] + trailer[1]
    ///
    /// After assembly the whole 70-byte buffer is XOR'd with a single
    /// random byte so the token changes on every request while the
    /// server can still verify it (brute-force 256 possible keys).
    pub fn generate_token(&self, method: &str, path: &str) -> String {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(EPOCH);
        let counter = now_secs.saturating_sub(EPOCH);
        let counter_str = counter.to_string();

        let hash_input = format!("{}!{}!{}{}", method, path, counter_str, self.suffix);
        let hash = Sha256::digest(hash_input.as_bytes());

        const HASH_START: usize = HEADER_LEN + COUNTER_BYTES;
        let mut raw = [0u8; TOKEN_LEN];
        raw[..HEADER_LEN].copy_from_slice(&self.header);
        raw[HEADER_LEN..HASH_START].copy_from_slice(&(counter as u32).to_le_bytes());
        raw[HASH_START..HASH_START + HASH_BYTES].copy_from_slice(&hash[..HASH_BYTES]);
        raw[TOKEN_LEN - 1] = self.trailer;

        let xor_key: u8 = rand::random();
        for byte in &mut raw {
            *byte ^= xor_key;
        }

        BASE64.encode(raw)
    }
}

pub fn generate_statsig_id(config: &ChallengeConfig, method: &str, path: &str) -> String {
    config.generate_token(method, path)
}
