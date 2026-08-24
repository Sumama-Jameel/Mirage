use base64::Engine;

/// Signed `x-statsig-id` tokens are minted by grok.com's own JavaScript and
/// validated against the live deploy (synthetic error markers are rejected
/// with code 7). They are harvested at the wire level from the logged-in
/// page — see [`super::statsig_harvest`] — and cached until the next deploy
/// invalidates them. The synthetic marker below remains only as a
/// last-resort attempt before failing.
pub const HARVEST_TTL_SECS: u64 = 12 * 60 * 60;

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
