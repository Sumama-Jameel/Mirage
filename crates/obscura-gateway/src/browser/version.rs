//! Firefox version detection and identity string construction.
//!
//! The gateway impersonates the user's real browser identity. When the source
//! browser is Firefox, we auto-detect the installed version from `firefox-esr`
//! or `firefox` and build a matching User-Agent string.

use tracing::{info, warn};

const DEFAULT_FIREFOX_VERSION: &str = "140.0";

/// Try to detect the installed Firefox/Firefox ESR version by running the
/// browser binary with `--version`. This does not launch a browser window;
/// it only prints the version string.
pub fn detect_firefox_version() -> Option<String> {
    for binary in ["firefox-esr", "firefox"] {
        match std::process::Command::new(binary).arg("--version").output() {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let text = format!("{} {}", stdout.trim(), stderr.trim());
                if let Some(version) = parse_firefox_version(&text) {
                    info!(binary, version, "Detected Firefox version");
                    return Some(version);
                }
            }
            Ok(output) => {
                warn!(
                    binary,
                    status = ?output.status,
                    "Firefox version command returned non-zero status"
                );
            }
            Err(e) => {
                warn!(binary, error = %e, "Failed to run Firefox version command");
            }
        }
    }
    None
}

/// Parse a Firefox version from a `--version` output string like
/// "Mozilla Firefox 140.0.1".
fn parse_firefox_version(text: &str) -> Option<String> {
    // Find "firefox" (case-insensitive) then scan forward for the version number.
    let lower = text.to_lowercase();
    let pos = lower.find("firefox")? + "firefox".len();
    let rest = &text[pos..];
    let start = rest.find(|c: char| c.is_ascii_digit())?;
    let rest = &rest[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(rest.len());
    let version = &rest[..end];
    // Ensure we captured at least "major.minor" and discard the patch segment.
    let mut parts = version.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    Some(format!("{}.{}", major, minor))
}

/// Build a Firefox-on-Linux User-Agent string from a version number.
pub fn build_firefox_ua(version: &str) -> String {
    format!(
        "Mozilla/5.0 (X11; Linux x86_64; rv:{version}) Gecko/20100101 Firefox/{version}"
    )
}

/// Return the auto-detected Firefox version, falling back to a safe default.
pub fn firefox_version_or_default() -> String {
    detect_firefox_version().unwrap_or_else(|| {
        info!(
            version = DEFAULT_FIREFOX_VERSION,
            "Using default Firefox version"
        );
        DEFAULT_FIREFOX_VERSION.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_firefox_version_output() {
        assert_eq!(
            parse_firefox_version("Mozilla Firefox 140.0.1").as_deref(),
            Some("140.0")
        );
        assert_eq!(
            parse_firefox_version("Mozilla Firefox 128.3.0esr").as_deref(),
            Some("128.3")
        );
        assert_eq!(
            parse_firefox_version("firefox-esr --version output: 115.0").as_deref(),
            Some("115.0")
        );
    }

    #[test]
    fn rejects_invalid_version_strings() {
        assert_eq!(parse_firefox_version(""), None);
        assert_eq!(parse_firefox_version("Chrome 120.0"), None);
    }

    #[test]
    fn builds_firefox_ua() {
        let ua = build_firefox_ua("140.0");
        assert!(ua.contains("Firefox/140.0"));
        assert!(ua.contains("Linux x86_64"));
    }
}
