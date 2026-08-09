use std::path::{Path, PathBuf};

use crate::error::GatewayError;

/// Locate the default Firefox profile directory on Linux and Windows.
///
/// Linux:  `~/.mozilla/firefox/profiles.ini` / `~/.mozilla/firefox-esr/profiles.ini`
/// Windows: `%APPDATA%\Mozilla\Firefox\profiles.ini`
///
/// Returns the profile marked as `Default=1`, falling back to any profile whose
/// name ends with `.default-release` or `.default-esr*`.
pub fn find_default_profile() -> Result<PathBuf, GatewayError> {
    let profiles_ini = find_profiles_ini()?;

    let content = std::fs::read_to_string(&profiles_ini).map_err(|e| {
        GatewayError::Internal(format!("failed to read {}: {e}", profiles_ini.display()))
    })?;

    let base_dir = profiles_ini
        .parent()
        .expect("profiles.ini must have a parent directory")
        .to_path_buf();

    parse_profiles_ini(&content, &base_dir)
}

fn find_profiles_ini() -> Result<PathBuf, GatewayError> {
    #[cfg(target_os = "linux")]
    {
        let home = dirs::home_dir()
            .ok_or_else(|| GatewayError::Internal("could not determine home directory".to_string()))?;

        let candidates = [
            home.join(".mozilla/firefox/profiles.ini"),
            home.join(".mozilla/firefox-esr/profiles.ini"),
        ];

        candidates
            .iter()
            .find(|p| p.exists())
            .cloned()
            .ok_or_else(|| {
                GatewayError::Internal(format!(
                    "Firefox profiles.ini not found at any of: {}",
                    candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
                ))
            })
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = dirs::data_dir()
            .ok_or_else(|| GatewayError::Internal("could not determine AppData directory".to_string()))?;
        let path = appdata.join("Mozilla").join("Firefox").join("profiles.ini");
        if path.exists() {
            Ok(path)
        } else {
            Err(GatewayError::Internal(format!(
                "Firefox profiles.ini not found at {}",
                path.display()
            )))
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Err(GatewayError::Internal(
            "Firefox profile discovery is not supported on this platform".to_string(),
        ))
    }
}

/// Parse a `profiles.ini` file and resolve the active profile path.
///
/// Selection order:
/// 1. Profile with `Default=1`.
/// 2. Profile whose `Name` looks like a default profile.
/// 3. Any profile with a valid `Path`.
fn parse_profiles_ini(content: &str, base_dir: &Path) -> Result<PathBuf, GatewayError> {
    #[derive(Default)]
    struct Profile {
        path: Option<PathBuf>,
        name: Option<String>,
        is_default: bool,
    }

    let mut profiles: Vec<Profile> = Vec::new();
    let mut current = Profile::default();

    for line in content.lines() {
        let line = line.trim();

        if line.starts_with('[') && line.ends_with(']') {
            profiles.push(std::mem::take(&mut current));
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();

            match key {
                "Path" => current.path = Some(base_dir.join(value)),
                "Name" => current.name = Some(value.to_string()),
                "Default" if value == "1" => current.is_default = true,
                _ => {}
            }
        }
    }
    profiles.push(current);

    let chosen = profiles
        .iter()
        .find(|p| p.is_default && p.path.is_some())
        .or_else(|| {
            profiles
                .iter()
                .find(|p| p.name.as_ref().map_or(false, |n| is_default_profile_name(n)) && p.path.is_some())
        })
        .or_else(|| profiles.iter().find(|p| p.path.is_some()));

    chosen
        .and_then(|p| p.path.clone())
        .ok_or_else(|| GatewayError::Internal("no usable Firefox profile found".to_string()))
}

/// Recognize default-profile names for standard Firefox, Firefox ESR, and
/// developer edition variants.
fn is_default_profile_name(name: &str) -> bool {
    name.ends_with(".default-release")
        || name.split('.').last().map_or(false, |suffix| suffix.starts_with("default-esr"))
        || name.ends_with(".default")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parse_profiles_prefers_default() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let ini = base.join("profiles.ini");
        let mut file = std::fs::File::create(&ini).unwrap();
        writeln!(
            file,
            "[Profile0]\nName=default\nPath=default\nDefault=1\n[Profile1]\nName=default-release\nPath=release"
        )
        .unwrap();

        let content = std::fs::read_to_string(&ini).unwrap();
        let profile = parse_profiles_ini(&content, base).unwrap();
        assert_eq!(profile, base.join("default"));
    }

    #[test]
    fn parse_profiles_falls_back_to_release() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let ini = base.join("profiles.ini");
        let mut file = std::fs::File::create(&ini).unwrap();
        writeln!(
            file,
            "[Profile0]\nName=dev-edition-default\nPath=dev\n[Profile1]\nName=xxx.default-release\nPath=release"
        )
        .unwrap();

        let content = std::fs::read_to_string(&ini).unwrap();
        let profile = parse_profiles_ini(&content, base).unwrap();
        assert_eq!(profile, base.join("release"));
    }

    #[test]
    fn parse_profiles_finds_esr() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let ini = base.join("profiles.ini");
        let mut file = std::fs::File::create(&ini).unwrap();
        writeln!(
            file,
            "[Profile0]\nName=default\nPath=d6qafzml.default-esr140\n[Profile1]\nName=dev\nPath=dev"
        )
        .unwrap();

        let content = std::fs::read_to_string(&ini).unwrap();
        let profile = parse_profiles_ini(&content, base).unwrap();
        assert_eq!(profile, base.join("d6qafzml.default-esr140"));
    }
}
