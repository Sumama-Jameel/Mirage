//! System browser detection.
//!
//! Detects the user's default browser on Linux/Windows and provides fallback
//! scanning of installed browsers.

use std::path::PathBuf;

use tracing::{debug, warn};

use crate::error::GatewayError;

use super::firefox_profile;
use super::{BrowserSource, BrowserType};

/// Detect the default browser using OS-specific mechanisms.
///
/// Linux:  `xdg-settings get default-web-browser`
/// Windows: UserChoice registry under
///          `HKEY_CURRENT_USER\Software\Microsoft\Windows\Shell\Associations\UrlAssociations\http\UserChoice`
pub fn detect_default_browser() -> Option<BrowserType> {
    #[cfg(target_os = "linux")]
    {
        detect_default_browser_linux()
    }
    #[cfg(target_os = "windows")]
    {
        detect_default_browser_windows()
    }
    #[cfg(target_os = "macos")]
    {
        detect_default_browser_macos()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        None
    }
}

/// Detect the first installed browser that has a known profile path.
pub fn detect_any_installed_browser() -> Option<BrowserSource> {
    for bt in [BrowserType::Firefox, BrowserType::Chrome, BrowserType::Edge] {
        if let Some(profile) = find_default_profile(bt) {
            return Some(BrowserSource {
                browser_type: bt,
                profile_path: profile,
            });
        }
    }
    None
}

/// Pick the best browser source for DeepSeek auth:
///
/// 1. Default browser, if installed.
/// 2. First installed browser with a profile path.
pub fn select_source() -> Result<BrowserSource, GatewayError> {
    if let Some(default) = detect_default_browser() {
        if let Some(profile) = find_default_profile(default) {
            return Ok(BrowserSource {
                browser_type: default,
                profile_path: profile,
            });
        }
        warn!(
            browser = %default,
            "Default browser detected but no profile path found"
        );
    }

    detect_any_installed_browser().ok_or_else(|| {
        GatewayError::Internal(
            "no browser profile found (looked for Firefox, Chrome, Edge on Linux/Windows/macOS)"
                .to_string(),
        )
    })
}

/// Locate the default profile path for a specific browser type.
pub fn find_default_profile(browser_type: BrowserType) -> Option<PathBuf> {
    match browser_type {
        BrowserType::Firefox => firefox_profile::find_default_profile().ok(),
        BrowserType::Chrome => find_chrome_default_profile(),
        BrowserType::Edge => find_edge_default_profile(),
    }
}

// =============================================================================
// Linux detection
// =============================================================================

#[cfg(target_os = "linux")]
fn detect_default_browser_linux() -> Option<BrowserType> {
    let output = std::process::Command::new("xdg-settings")
        .arg("get")
        .arg("default-web-browser")
        .output()
        .ok()?;
    if !output.status.success() {
        debug!("xdg-settings returned non-zero; default browser unknown");
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let desktop = stdout.trim();
    let browser = map_desktop_to_browser(desktop);
    if browser.is_none() {
        debug!(desktop, "unrecognized default-web-browser desktop file");
    }
    browser
}

/// Map a Linux `.desktop` file name to a browser type.
pub fn map_desktop_to_browser(desktop: &str) -> Option<BrowserType> {
    let lower = desktop.to_lowercase();
    if lower.contains("firefox") {
        Some(BrowserType::Firefox)
    } else if lower.contains("google-chrome") || lower.contains("chromium") {
        Some(BrowserType::Chrome)
    } else if lower.contains("microsoft-edge") || lower.contains("msedge") {
        Some(BrowserType::Edge)
    } else {
        None
    }
}

// =============================================================================
// Windows detection
// =============================================================================

#[cfg(target_os = "windows")]
fn detect_default_browser_windows() -> Option<BrowserType> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey(
            "Software\\Microsoft\\Windows\\Shell\\Associations\\UrlAssociations\\http\\UserChoice",
        )
        .ok()?;
    let prog_id: String = key.get_value("ProgId").ok()?;
    map_progid_to_browser(&prog_id)
}

/// Map a Windows ProgId string to a browser type.
#[allow(dead_code)]
pub fn map_progid_to_browser(prog_id: &str) -> Option<BrowserType> {
    let lower = prog_id.to_lowercase();
    if lower.contains("firefox") {
        Some(BrowserType::Firefox)
    } else if lower == "chromehtml" || lower.starts_with("chromehtml") {
        Some(BrowserType::Chrome)
    } else if lower.contains("msedgehtm") {
        Some(BrowserType::Edge)
    } else {
        None
    }
}

// =============================================================================
// macOS detection
// =============================================================================

#[cfg(target_os = "macos")]
fn detect_default_browser_macos() -> Option<BrowserType> {
    // Use `defaults read com.apple.LaunchServices/com.apple.launchservices.secure` to find
    // the default HTTP handler, or fall back to checking LSHandlers.
    let output = std::process::Command::new("defaults")
        .args([
            "read",
            "com.apple.LaunchServices/com.apple.launchservices.secure",
            "LSHandlers",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        debug!("defaults read LSHandlers failed; default browser unknown");
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Look for the http scheme handler
    let mut in_http_section = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.contains("LSHandlerURLScheme = http") || trimmed.contains("LSHandlerURLScheme = https") {
            in_http_section = true;
        }
        if in_http_section && trimmed.contains("LSHandlerRoleAll =") {
            let bundle_id = trimmed.split('=').nth(1)?.trim();
            return map_bundle_id_to_browser(bundle_id);
        }
    }

    // Fallback: check if common browsers are in /Applications
    let home = dirs::home_dir()?;
    for (browser, app_name) in [
        (BrowserType::Firefox, "Firefox.app"),
        (BrowserType::Chrome, "Google Chrome.app"),
        (BrowserType::Edge, "Microsoft Edge.app"),
    ] {
        let path = home.join("Applications").join(app_name);
        if path.exists() {
            return Some(browser);
        }
        let path = PathBuf::from("/Applications").join(app_name);
        if path.exists() {
            return Some(browser);
        }
    }

    None
}

/// Map a macOS bundle ID to a browser type.
#[cfg(target_os = "macos")]
fn map_bundle_id_to_browser(bundle_id: &str) -> Option<BrowserType> {
    let lower = bundle_id.to_lowercase();
    if lower.contains("firefox") {
        Some(BrowserType::Firefox)
    } else if lower.contains("chrome") || lower.contains("chromium") {
        Some(BrowserType::Chrome)
    } else if lower.contains("edge") {
        Some(BrowserType::Edge)
    } else {
        None
    }
}

// =============================================================================
// Chrome / Edge profile discovery (placeholders for Phase 4)
// =============================================================================

fn find_chrome_default_profile() -> Option<PathBuf> {
    chrome_default_profile_path().and_then(|base| {
        let default = base.join("Default");
        if default.exists() {
            Some(default)
        } else {
            None
        }
    })
}

fn find_edge_default_profile() -> Option<PathBuf> {
    edge_default_profile_path().and_then(|base| {
        let default = base.join("Default");
        if default.exists() {
            Some(default)
        } else {
            None
        }
    })
}

/// Return the user-data directory for Chrome on this OS.
fn chrome_default_profile_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let home = dirs::home_dir()?;
        for candidate in [
            home.join(".config/google-chrome"),
            home.join(".config/chromium"),
        ] {
            if candidate.exists() {
                return Some(candidate);
            }
        }
        None
    }
    #[cfg(target_os = "windows")]
    {
        let local = dirs::data_local_dir()?;
        for candidate in [
            local.join("Google").join("Chrome").join("User Data"),
            local.join("Chromium").join("User Data"),
        ] {
            if candidate.exists() {
                return Some(candidate);
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir()?;
        for candidate in [
            home.join("Library/Application Support/Google/Chrome"),
            home.join("Library/Application Support/Chromium"),
        ] {
            if candidate.exists() {
                return Some(candidate);
            }
        }
        None
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        None
    }
}

/// Return the user-data directory for Edge on this OS.
fn edge_default_profile_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let home = dirs::home_dir()?;
        let candidate = home.join(".config/microsoft-edge");
        if candidate.exists() {
            Some(candidate)
        } else {
            None
        }
    }
    #[cfg(target_os = "windows")]
    {
        let local = dirs::data_local_dir()?;
        let candidate = local.join("Microsoft").join("Edge").join("User Data");
        if candidate.exists() {
            Some(candidate)
        } else {
            None
        }
    }
    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir()?;
        let candidate = home.join("Library/Application Support/Microsoft Edge");
        if candidate.exists() {
            Some(candidate)
        } else {
            None
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_desktop_maps_firefox_variants() {
        assert_eq!(
            map_desktop_to_browser("firefox.desktop"),
            Some(BrowserType::Firefox)
        );
        assert_eq!(
            map_desktop_to_browser("firefox-esr.desktop"),
            Some(BrowserType::Firefox)
        );
        assert_eq!(
            map_desktop_to_browser("org.mozilla.firefox.desktop"),
            Some(BrowserType::Firefox)
        );
    }

    #[test]
    fn linux_desktop_maps_chrome_variants() {
        assert_eq!(
            map_desktop_to_browser("google-chrome.desktop"),
            Some(BrowserType::Chrome)
        );
        assert_eq!(
            map_desktop_to_browser("chromium.desktop"),
            Some(BrowserType::Chrome)
        );
        assert_eq!(
            map_desktop_to_browser("chromium-browser.desktop"),
            Some(BrowserType::Chrome)
        );
    }

    #[test]
    fn linux_desktop_maps_edge() {
        assert_eq!(
            map_desktop_to_browser("microsoft-edge.desktop"),
            Some(BrowserType::Edge)
        );
    }

    #[test]
    fn linux_desktop_unknown_returns_none() {
        assert_eq!(map_desktop_to_browser("konqueror.desktop"), None);
        assert_eq!(map_desktop_to_browser(""), None);
    }

    #[test]
    fn windows_progid_maps_browsers() {
        assert_eq!(
            map_progid_to_browser("FirefoxURL-1234567890ABCDEF"),
            Some(BrowserType::Firefox)
        );
        assert_eq!(map_progid_to_browser("ChromeHTML"), Some(BrowserType::Chrome));
        assert_eq!(map_progid_to_browser("MSEdgeHTM"), Some(BrowserType::Edge));
    }

    #[test]
    fn windows_progid_unknown_returns_none() {
        assert_eq!(map_progid_to_browser("IE.HTTP"), None);
    }
}
