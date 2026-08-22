use base64::Engine;

/// Current grok.com web requests use a per-request browser error marker for
/// `x-statsig-id`, not the retired 70-byte challenge blob (which now triggers
/// anti-bot 403s). Format verified against OmniRoute's GrokWebExecutor
/// (live grok.com capture, 2026-08-16): a base64-encoded synthetic TypeError
/// that mirrors what the frontend's error handler produces. No constants are
/// needed and none are extracted at runtime.
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
