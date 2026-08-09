pub struct BrowserProfile {
    pub user_agent: &'static str,
    pub platform: &'static str,
    pub ua_platform: &'static str,
    pub ua_platform_version: &'static str,
}

pub static PROFILES: &[BrowserProfile] = &[
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36",
        platform: "Win32",
        ua_platform: "Windows",
        ua_platform_version: "10.0.0",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/144.0.0.0 Safari/537.36",
        platform: "Win32",
        ua_platform: "Windows",
        ua_platform_version: "10.0.0",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36",
        platform: "Win32",
        ua_platform: "Windows",
        ua_platform_version: "15.0.0",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36",
        platform: "Win32",
        ua_platform: "Windows",
        ua_platform_version: "15.0.0",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36",
        platform: "MacIntel",
        ua_platform: "macOS",
        ua_platform_version: "13.6.7",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/144.0.0.0 Safari/537.36",
        platform: "MacIntel",
        ua_platform: "macOS",
        ua_platform_version: "14.4.1",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36",
        platform: "MacIntel",
        ua_platform: "macOS",
        ua_platform_version: "14.5.0",
    },
    BrowserProfile {
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36",
        platform: "MacIntel",
        ua_platform: "macOS",
        ua_platform_version: "14.6.0",
    },
];

/// Owned browser fingerprint values derived from a User-Agent string.
#[derive(Debug, Clone)]
pub struct BrowserIdentity {
    pub user_agent: String,
    pub platform: String,
    pub ua_platform: String,
    pub ua_platform_version: String,
}

/// Derive a browser identity from a User-Agent string.
///
/// This lets callers supply a custom UA (e.g., matching the user's real
/// Firefox installation) and still get consistent platform values for the
/// navigator shim.
pub fn identity_from_ua(ua: &str) -> BrowserIdentity {
    let ua_lower = ua.to_ascii_lowercase();
    if ua_lower.contains("firefox") {
        if ua.contains("Linux") {
            BrowserIdentity {
                user_agent: ua.to_string(),
                platform: "Linux x86_64".to_string(),
                ua_platform: "Linux".to_string(),
                ua_platform_version: "".to_string(),
            }
        } else if ua.contains("Macintosh") || ua.contains("Mac OS X") {
            BrowserIdentity {
                user_agent: ua.to_string(),
                platform: "MacIntel".to_string(),
                ua_platform: "macOS".to_string(),
                ua_platform_version: "14.5.0".to_string(),
            }
        } else {
            BrowserIdentity {
                user_agent: ua.to_string(),
                platform: "Win32".to_string(),
                ua_platform: "Windows".to_string(),
                ua_platform_version: "15.0.0".to_string(),
            }
        }
    } else {
        if ua.contains("Macintosh") || ua.contains("Mac OS X") {
            BrowserIdentity {
                user_agent: ua.to_string(),
                platform: "MacIntel".to_string(),
                ua_platform: "macOS".to_string(),
                ua_platform_version: "14.5.0".to_string(),
            }
        } else {
            BrowserIdentity {
                user_agent: ua.to_string(),
                platform: "Win32".to_string(),
                ua_platform: "Windows".to_string(),
                ua_platform_version: "15.0.0".to_string(),
            }
        }
    }
}

/// Return the browser-family identity implied by a User-Agent string.
pub fn browser_family_from_ua(ua: &str) -> &'static str {
    let ua_lower = ua.to_ascii_lowercase();
    if ua_lower.contains("firefox") {
        "firefox"
    } else {
        "chrome"
    }
}

pub fn random_profile() -> &'static BrowserProfile {
    let idx = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as usize)
        % PROFILES.len();
    &PROFILES[idx]
}

/// Pick the profile for a new browser context.
///
/// The default is a single stable profile. Cycling through different browser
/// identities from one address is itself a bot signal (a real address maps to a
/// stable device), and the rotated profile does not yet carry a matching TLS or
/// timezone fingerprint, so rotation is opt-in:
///   OBSCURA_PROFILE=<index>   pin a specific profile from PROFILES
///   OBSCURA_ROTATE_PROFILE=1  pick a random profile per context
pub fn select_profile() -> &'static BrowserProfile {
    if let Some(idx) = std::env::var("OBSCURA_PROFILE")
        .ok()
        .as_deref()
        .map(str::trim)
        .and_then(|s| s.parse::<usize>().ok())
    {
        if idx < PROFILES.len() {
            return &PROFILES[idx];
        }
    }
    if env_enabled("OBSCURA_ROTATE_PROFILE") {
        return random_profile();
    }
    &PROFILES[0]
}

fn env_enabled(key: &str) -> bool {
    matches!(
        std::env::var(key)
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}
