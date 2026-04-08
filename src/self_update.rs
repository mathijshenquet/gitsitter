//! Self-update: check for new releases and replace the running binary.
//!
//! Uses GitHub Releases API to find the latest version, downloads the
//! platform-appropriate tarball, and replaces the binary via `self-replace`.

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const REPO: &str = "mathijshenquet/gitsitter";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60); // 24h

// ---------------------------------------------------------------------------
// Version check (used by daemon)
// ---------------------------------------------------------------------------

/// Path to the file that caches the latest known version + last check time.
pub fn update_state_path() -> std::path::PathBuf {
    let state_dir = std::env::var_os("GITSITTER_STATE_DIR")
        .map(std::path::PathBuf::from)
        .or_else(dirs::state_dir)
        .or_else(dirs::data_local_dir)
        .expect("cannot determine state directory")
        .join("gitsitter");
    state_dir.join("update_check.json")
}

#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct UpdateState {
    pub latest_version: String,
    pub checked_at: u64, // unix timestamp
}

impl UpdateState {
    fn load(path: &Path) -> Option<Self> {
        let data = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&data).ok()
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string(self)?;
        std::fs::write(path, data)?;
        Ok(())
    }

    fn is_stale(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now.saturating_sub(self.checked_at) > CHECK_INTERVAL.as_secs()
    }
}

/// Returns the latest version string if it is newer than the current version,
/// or None if we're up to date or can't determine.
pub fn cached_update_available() -> Option<String> {
    let state = UpdateState::load(&update_state_path())?;
    if is_newer(&state.latest_version, CURRENT_VERSION) {
        Some(state.latest_version)
    } else {
        None
    }
}

/// Check GitHub for the latest release. Called by the daemon periodically.
/// Returns Ok(Some(version)) if a newer version exists.
pub async fn check_for_update() -> Result<Option<String>> {
    let state_path = update_state_path();

    // Don't check if we recently checked
    if let Some(state) = UpdateState::load(&state_path)
        && !state.is_stale()
    {
        return Ok(if is_newer(&state.latest_version, CURRENT_VERSION) {
            Some(state.latest_version)
        } else {
            None
        });
    }

    let latest = fetch_latest_version().await?;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let state = UpdateState {
        latest_version: latest.clone(),
        checked_at: now,
    };
    // Best-effort save
    let _ = state.save(&state_path);

    Ok(if is_newer(&latest, CURRENT_VERSION) {
        Some(latest)
    } else {
        None
    })
}

// ---------------------------------------------------------------------------
// Self-update (used by CLI)
// ---------------------------------------------------------------------------

/// Perform the self-update: download latest release and replace the binary.
pub async fn self_update() -> Result<()> {
    // Skip if installed via nix
    if is_nix_managed() {
        bail!("gitsitter is managed by Nix — update via your flake instead");
    }

    println!("Current version: v{CURRENT_VERSION}");
    println!("Checking for updates...");

    let latest = fetch_latest_version().await?;
    if !is_newer(&latest, CURRENT_VERSION) {
        println!("Already up to date (v{CURRENT_VERSION})");
        return Ok(());
    }

    println!("Updating to {latest}...");

    let target = detect_target()?;
    let archive_name = format!("gitsitter-{target}.tar.gz");
    let url = format!("https://github.com/{REPO}/releases/download/{latest}/{archive_name}");

    // Download to temp dir
    let tmpdir = tempfile::tempdir().context("failed to create temp dir")?;
    let archive_path = tmpdir.path().join(&archive_name);

    download_file(&url, &archive_path).await?;

    // Extract
    let binary_path = tmpdir.path().join("gitsitter");
    extract_tar_gz(&archive_path, tmpdir.path())?;

    if !binary_path.exists() {
        bail!("extracted archive does not contain 'gitsitter' binary");
    }

    // Replace the running binary
    self_replace::self_replace(&binary_path).context("failed to replace binary")?;

    // Update state cache
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let _ = UpdateState {
        latest_version: latest.clone(),
        checked_at: now,
    }
    .save(&update_state_path());

    println!("Updated to {latest}");
    println!("Run 'gitsitter install' to update the daemon service if needed.");

    Ok(())
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
}

async fn fetch_latest_version() -> Result<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let client = reqwest::Client::builder()
        .user_agent("gitsitter-updater")
        .timeout(Duration::from_secs(10))
        .build()?;

    let resp = client
        .get(&url)
        .send()
        .await
        .context("failed to reach GitHub API")?;

    if !resp.status().is_success() {
        bail!("GitHub API returned {}", resp.status());
    }

    let release: GithubRelease = resp.json().await.context("failed to parse release")?;
    Ok(release.tag_name)
}

async fn download_file(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent("gitsitter-updater")
        .timeout(Duration::from_secs(120))
        .build()?;

    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to download {url}"))?;

    if !resp.status().is_success() {
        bail!("download failed: HTTP {}", resp.status());
    }

    let bytes = resp.bytes().await?;
    std::fs::write(dest, &bytes)?;
    Ok(())
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<()> {
    let status = std::process::Command::new("tar")
        .args([
            "xzf",
            &archive.display().to_string(),
            "-C",
            &dest.display().to_string(),
        ])
        .status()
        .context("failed to run tar")?;

    if !status.success() {
        bail!("tar extraction failed");
    }
    Ok(())
}

fn detect_target() -> Result<String> {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;

    let arch = match arch {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => bail!("unsupported architecture: {arch}"),
    };

    let os = match os {
        "linux" => "unknown-linux-gnu",
        "macos" => "apple-darwin",
        _ => bail!("unsupported OS: {os}"),
    };

    Ok(format!("{arch}-{os}"))
}

/// Compare semver-ish version strings. Returns true if `latest` is newer than `current`.
fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<(u64, u64, u64)> {
        let v = v.strip_prefix('v').unwrap_or(v);
        let mut parts = v.splitn(3, '.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        Some((major, minor, patch))
    };
    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Detect if we're running from the nix store.
fn is_nix_managed() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.starts_with("/nix/store/")))
        .unwrap_or(false)
}

pub fn current_version() -> &'static str {
    CURRENT_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
    }

    #[test]
    fn test_is_nix_managed() {
        // current_exe in tests won't be in /nix/store
        assert!(!is_nix_managed());
    }
}
