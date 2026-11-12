//! Verified updates from this repository's stable GitHub releases.

use std::fs;
use std::path::{Path, PathBuf};
use std::ptr::null;
use std::thread::{sleep, spawn};
use std::time::Duration;

use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex};

use crate::{config, install, log, ui, watch};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const REPO: &str = "pepsi-enjoyer/auto-display";
const LATEST: &str = "https://api.github.com/repos/pepsi-enjoyer/auto-display/releases/latest";
const FIRST_CHECK: Duration = Duration::from_secs(60);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_DOWNLOAD: u64 = 64 * 1024 * 1024;

#[derive(Debug, PartialEq)]
pub enum Outcome {
    UpToDate,
    Installed(String),
    Deferred,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

/// Only the installed release watcher updates itself; development builds stay local.
pub fn start() {
    if cfg!(debug_assertions) || !is_installed() {
        return;
    }
    spawn(|| {
        sleep(FIRST_CHECK);
        loop {
            cleanup();
            let mut delay = CHECK_EVERY;
            if config::load().auto_update {
                match check_and_install() {
                    Ok(Outcome::Installed(_)) => return,
                    Ok(Outcome::Deferred) => delay = FIRST_CHECK,
                    Ok(Outcome::UpToDate) => log::line("auto-update: already up to date."),
                    Err(e) => log::line(&format!("auto-update: {e}; will retry later.")),
                }
            }
            sleep(delay);
        }
    });
}

pub fn run() -> Result<(), String> {
    match check_and_install()? {
        Outcome::UpToDate => log::line(&format!("auto-display {CURRENT} is up to date.")),
        Outcome::Installed(version) => log::line(&format!("Updated to {version}.")),
        Outcome::Deferred => {
            log::line("Update deferred while a layout card or another updater is open. Try again shortly.")
        }
    }
    Ok(())
}

fn is_installed() -> bool {
    let installed = fs::canonicalize(log::data_dir().join("auto-display.exe"));
    let current = std::env::current_exe().and_then(fs::canonicalize);
    matches!((installed, current), (Ok(a), Ok(b)) if a == b)
}

pub fn check_and_install() -> Result<Outcome, String> {
    if !is_installed() {
        return Err("Run the installed copy of auto-display to update it (in %LOCALAPPDATA%\\auto-display).".into());
    }
    let Some(_lock) = UpdateLock::acquire()? else {
        return Ok(Outcome::Deferred);
    };
    if ui::is_open() {
        return Ok(Outcome::Deferred);
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(60)))
        .user_agent(format!("auto-display/{CURRENT}"))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into();
    let mut response = agent
        .get(LATEST)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("could not check GitHub: {e}"))?;
    if response.status().as_u16() != 200 {
        return Err(format!("GitHub returned HTTP {}", response.status().as_u16()));
    }
    let release: Release = response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_json()
        .map_err(|e| format!("invalid release response: {e}"))?;
    if !release.is_newer_than(CURRENT)? {
        return Ok(Outcome::UpToDate);
    }
    let Some(asset) = release.asset("auto-display.exe") else {
        return Ok(Outcome::Deferred);
    };
    if asset.size == 0 || asset.size > MAX_DOWNLOAD {
        return Err("release executable has an unexpected size".into());
    }
    let expected = match &asset.digest {
        Some(digest) => checksum(digest.strip_prefix("sha256:").unwrap_or(""))?,
        None => {
            let Some(sums) = release.asset("SHA256SUMS") else {
                return Ok(Outcome::Deferred);
            };
            let bytes = download(&agent, &release, sums, 4096)?;
            let text = std::str::from_utf8(&bytes).map_err(|_| "invalid checksum file")?;
            checksum_file(text)?
        }
    };
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let original = fs::read(&exe).map_err(|e| e.to_string())?;
    let bytes = download(&agent, &release, asset, MAX_DOWNLOAD)?;
    verify(&bytes, asset.size, &expected)?;
    // Another installer may have replaced this version during the download.
    if fs::read(&exe).map_err(|e| e.to_string())? != original {
        return Err("the installed executable changed during the download".into());
    }
    if ui::is_open() {
        return Ok(Outcome::Deferred);
    }
    let parent = watch::running_pid().unwrap_or(0);
    replace_and_restart(&exe, &bytes, |path| {
        install::start_detached(&format!("\"{}\" watch --updated {parent}", path.display()))
    })?;
    log::line(&format!(
        "Updated auto-display {CURRENT} to {}. Restarting the watcher.",
        release.tag_name
    ));
    watch::request_stop(parent);
    Ok(Outcome::Installed(release.tag_name))
}

impl Release {
    fn is_newer_than(&self, current: &str) -> Result<bool, String> {
        let latest = Version::parse(self.tag_name.strip_prefix('v').unwrap_or(&self.tag_name))
            .map_err(|_| "release has an invalid version")?;
        let current = Version::parse(current).map_err(|_| "current version is invalid")?;
        Ok(!self.draft && !self.prerelease && latest.pre.is_empty() && latest.cmp_precedence(&current).is_gt())
    }

    fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

fn download(agent: &ureq::Agent, release: &Release, asset: &Asset, limit: u64) -> Result<Vec<u8>, String> {
    let expected_url = format!(
        "https://github.com/{REPO}/releases/download/{}/{}",
        release.tag_name, asset.name
    );
    if asset.browser_download_url != expected_url || asset.size == 0 || asset.size > limit {
        return Err("release has an invalid download asset".into());
    }
    let mut response = agent
        .get(&asset.browser_download_url)
        .config()
        .timeout_global(Some(Duration::from_secs(300)))
        .build()
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    if response.status().as_u16() != 200 {
        return Err(format!("download returned HTTP {}", response.status().as_u16()));
    }
    let bytes = response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(|e| format!("download failed: {e}"))?;
    if bytes.len() as u64 != asset.size {
        return Err("download was incomplete".into());
    }
    Ok(bytes)
}

fn checksum(text: &str) -> Result<String, String> {
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("release has an invalid SHA-256 checksum".into());
    }
    Ok(text.to_ascii_lowercase())
}

fn checksum_file(text: &str) -> Result<String, String> {
    for line in text.lines() {
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() == 2 && parts[1].trim_start_matches('*') == "auto-display.exe" {
            return checksum(parts[0]);
        }
    }
    Err("release checksum file has no auto-display.exe entry".into())
}

fn verify(bytes: &[u8], size: u64, expected: &str) -> Result<(), String> {
    if bytes.len() as u64 != size || !bytes.starts_with(b"MZ") {
        return Err("download is not a complete Windows executable".into());
    }
    let actual = sha256(bytes);
    if actual != checksum(expected)? {
        return Err("download did not match its SHA-256 checksum".into());
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Windows permits a running executable to be renamed. Keep it until the new version starts.
fn replace_and_restart(
    exe: &Path,
    bytes: &[u8],
    restart: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let new = sibling(exe, "new");
    let old = sibling(exe, "old");
    if old.exists() {
        fs::remove_file(&old).map_err(|e| format!("previous version is still in use: {e}"))?;
    }
    fs::write(&new, bytes).map_err(|e| format!("could not stage update: {e}"))?;
    if let Err(e) = fs::rename(exe, &old) {
        let _ = fs::remove_file(&new);
        return Err(format!("could not move the old executable: {e}"));
    }
    let result = fs::rename(&new, exe)
        .map_err(|e| e.to_string())
        .and_then(|()| restart(exe));
    if let Err(e) = result {
        if exe.exists() {
            fs::rename(exe, &new).map_err(|rollback| format!("update failed ({e}); rollback failed: {rollback}"))?;
        }
        fs::rename(&old, exe).map_err(|rollback| format!("update failed ({e}); rollback failed: {rollback}"))?;
        let _ = fs::remove_file(&new);
        return Err(format!("update failed; restored the previous version: {e}"));
    }
    Ok(())
}

pub fn cleanup() {
    if is_installed()
        && let Ok(exe) = std::env::current_exe()
    {
        let _ = fs::remove_file(sibling(&exe, "old"));
    }
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut path = exe.as_os_str().to_owned();
    path.push(format!(".{suffix}"));
    PathBuf::from(path)
}

struct UpdateLock(HANDLE);

impl UpdateLock {
    fn acquire() -> Result<Option<Self>, String> {
        let name = watch::wide("Local\\auto-display-update");
        let handle = unsafe { CreateMutexW(null(), 1, name.as_ptr()) };
        if handle.is_null() {
            return Err("could not lock the updater".into());
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(handle) };
            return Ok(None);
        }
        Ok(Some(Self(handle)))
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn release(tag: &str) -> Release {
        serde_json::from_value(json!({"tag_name": tag, "assets": []})).unwrap()
    }

    #[test]
    fn only_newer_stable_releases_are_installed() {
        for tag in ["v0.1.1", "v0.2.0", "v0.2.0+build", "v0.3.0-beta.1"] {
            assert!(!release(tag).is_newer_than("0.2.0").unwrap());
        }
        assert!(release("v0.10.0").is_newer_than("0.9.0").unwrap());
        assert!(release("bad").is_newer_than("0.2.0").is_err());
        let mut r = release("v0.3.0");
        r.draft = true;
        assert!(!r.is_newer_than("0.2.0").unwrap());
        r.draft = false;
        r.prerelease = true;
        assert!(!r.is_newer_than("0.2.0").unwrap());
    }

    #[test]
    fn incomplete_or_corrupt_downloads_are_rejected() {
        let bytes = b"MZ test executable";
        let hash = sha256(bytes);
        assert!(verify(bytes, bytes.len() as u64, &hash).is_ok());
        assert!(verify(bytes, bytes.len() as u64 + 1, &hash).is_err());
        assert!(verify(bytes, bytes.len() as u64, &"0".repeat(64)).is_err());
        assert!(verify(b"HTML", 4, &hash).is_err());
        assert!(verify(bytes, bytes.len() as u64, "invalid").is_err());
        assert_eq!(checksum_file(&format!("{hash}  auto-display.exe\n")).unwrap(), hash);
        assert_eq!(checksum_file(&format!("{hash} *auto-display.exe\n")).unwrap(), hash);
        assert!(checksum_file(&format!("{hash} other.exe")).is_err());
    }

    #[test]
    fn failed_restart_restores_the_previous_executable() {
        let dir = std::env::temp_dir().join(format!("auto-display-rollback-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("auto-display.exe");
        fs::write(&exe, b"original").unwrap();
        let result = replace_and_restart(&exe, b"replacement", |_| Err("simulated launch failure".into()));
        assert!(result.unwrap_err().contains("restored the previous version"));
        assert_eq!(fs::read(&exe).unwrap(), b"original");
        assert!(!sibling(&exe, "new").exists());
        assert!(!sibling(&exe, "old").exists());
        replace_and_restart(&exe, b"replacement", |p| {
            assert_eq!(fs::read(p).unwrap(), b"replacement");
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"replacement");
        assert_eq!(fs::read(sibling(&exe, "old")).unwrap(), b"original");
        fs::remove_file(&exe).unwrap();
        fs::remove_file(sibling(&exe, "old")).unwrap();
        fs::remove_dir(&dir).unwrap();
    }
}
