//! Self-update against GitHub Releases. On startup the app asks for the
//! latest release; if it's newer than the running version a banner appears in
//! the top bar. Windows gets one-click "update & restart": download the Inno
//! installer and run it silently (`/SILENT /CLOSEAPPLICATIONS`) — the
//! installer's `[Run]` entry relaunches dbTool when it finishes. Other
//! platforms open the release page in the browser.

use anyhow::{Context, Result};
use std::path::PathBuf;

const REPO: &str = "Rinoceronte/dbTool";
const USER_AGENT: &str = concat!("dbTool/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    /// New version without the leading `v`, e.g. "0.2.0".
    pub version: String,
    /// Release page for manual download (macOS/Linux path).
    pub release_url: String,
    /// Direct download of the Windows installer asset, when present.
    pub installer_url: Option<String>,
}

/// Latest release if it's newer than the running build. `Ok(None)` covers
/// "up to date" and "no releases yet" (404).
pub async fn check() -> Result<Option<UpdateInfo>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = reqwest::Client::new()
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("update check request failed")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let resp = resp.error_for_status().context("update check failed")?;
    let release: serde_json::Value = resp.json().await.context("bad release JSON")?;

    let tag = release["tag_name"].as_str().unwrap_or_default();
    let latest = tag.trim_start_matches('v');
    if !is_newer(latest, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }

    let installer_url = release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| {
            a["name"]
                .as_str()
                .is_some_and(|n| n.ends_with("-setup.exe"))
        })
        .and_then(|a| a["browser_download_url"].as_str())
        .map(str::to_owned);
    let release_url = release["html_url"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("https://github.com/{REPO}/releases/latest"));

    Ok(Some(UpdateInfo {
        version: latest.to_owned(),
        release_url,
        installer_url,
    }))
}

/// Download the installer asset to the OS temp dir and return its path.
pub async fn download_installer(url: &str, version: &str) -> Result<PathBuf> {
    let resp = reqwest::Client::new()
        .get(url)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .context("installer download failed")?
        .error_for_status()
        .context("installer download failed")?;
    let bytes = resp.bytes().await.context("installer download interrupted")?;
    let path = std::env::temp_dir().join(format!("dbTool-{version}-setup.exe"));
    tokio::fs::write(&path, &bytes)
        .await
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

/// Run the installer silently and detach. `/CLOSEAPPLICATIONS` lets it wait
/// out our own exit if the file copy starts before we're gone; the
/// installer's `[Run]` entry relaunches the app afterwards.
pub fn launch_installer(path: &std::path::Path) -> Result<()> {
    std::process::Command::new(path)
        .args(["/SILENT", "/CLOSEAPPLICATIONS", "/NORESTART"])
        .spawn()
        .with_context(|| format!("could not launch {}", path.display()))?;
    Ok(())
}

/// Strictly-newer semver-triple compare; unparsable versions are never newer.
fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_triple(candidate), parse_triple(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn parse_triple(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    // Tolerate suffixes like "3-rc1" in the patch slot.
    let patch = it
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    it.next().is_none().then_some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(!is_newer("1.2", "0.1.0"));
    }

    #[test]
    fn triple_parses_suffixes() {
        assert_eq!(parse_triple("0.2.3-rc1"), Some((0, 2, 3)));
        assert_eq!(parse_triple("v1.2.3"), None); // tag prefix stripped by caller
    }
}
