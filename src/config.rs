//! Configuration parsing for gitsitter.
//!
//! Configuration is split across three files in the config directory:
//! - `config.toml`: global settings (read-only, managed by nix or user)
//! - `repos.toml`: per-repo registration and overrides (mutable by CLI)
//! - `trusted_hosts`: one host per line (mutable by CLI)
//!
//! Plus an optional in-repo config: `.gitsitter.toml` in a repo's root.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::paths::Paths;

// ---------------------------------------------------------------------------
// Disabled enum (repo-level or per-remote)
// ---------------------------------------------------------------------------

/// A repo can be fully disabled (`true`), fully enabled (`false`),
/// or have specific remotes disabled (`["origin", "upstream"]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Disabled {
    All(bool),
    Remotes(Vec<String>),
}

impl Disabled {
    /// Is the entire repo disabled?
    pub fn is_repo_disabled(&self) -> bool {
        matches!(self, Disabled::All(true))
    }

    /// Is a specific remote disabled?
    pub fn is_remote_disabled(&self, remote: &str) -> bool {
        match self {
            Disabled::All(true) => true,
            Disabled::All(false) => false,
            Disabled::Remotes(list) => list.iter().any(|r| r == remote),
        }
    }

    /// List of explicitly disabled remote names (empty if whole-repo disabled or enabled).
    pub fn disabled_remotes(&self) -> &[String] {
        match self {
            Disabled::Remotes(list) => list,
            _ => &[],
        }
    }
}


// ---------------------------------------------------------------------------
// Duration helpers
// ---------------------------------------------------------------------------

fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty duration string");
    }
    let (num, unit) = if let Some(num) = s.strip_suffix("ms") {
        (num, "ms")
    } else {
        (&s[..s.len() - 1], &s[s.len() - 1..])
    };
    let n: u64 = num.parse().context("invalid duration number")?;
    match unit {
        "ms" => Ok(Duration::from_millis(n)),
        "s" => Ok(Duration::from_secs(n)),
        "m" => Ok(Duration::from_secs(n * 60)),
        "h" => Ok(Duration::from_secs(n * 3600)),
        _ => anyhow::bail!("unknown duration unit: {unit}"),
    }
}

fn format_duration(d: &Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        let ms = d.as_millis();
        return format!("{ms}ms");
    }
    if secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn _deserialize_duration<'de, D: Deserializer<'de>>(de: D) -> Result<Duration, D::Error> {
    let s = String::deserialize(de)?;
    parse_duration(&s).map_err(serde::de::Error::custom)
}

fn _serialize_duration<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format_duration(d))
}

fn deserialize_opt_duration<'de, D: Deserializer<'de>>(
    de: D,
) -> Result<Option<Duration>, D::Error> {
    let v: Option<String> = Option::deserialize(de)?;
    match v {
        None => Ok(None),
        Some(s) => parse_duration(&s).map(Some).map_err(serde::de::Error::custom),
    }
}

fn serialize_opt_duration<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
    match d {
        Some(d) => s.serialize_str(&format_duration(d)),
        None => s.serialize_none(),
    }
}

// ---------------------------------------------------------------------------
// Raw serde structs (what the TOML files look like on disk)
// ---------------------------------------------------------------------------

/// Raw representation of config.toml — flat top-level keys, no sections.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RawConfigToml {
    #[serde(
        default,
        deserialize_with = "deserialize_opt_duration",
        serialize_with = "serialize_opt_duration",
        skip_serializing_if = "Option::is_none"
    )]
    refresh_interval: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    colors: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    emoji: Option<bool>,
    #[serde(
        default,
        deserialize_with = "deserialize_opt_duration",
        serialize_with = "serialize_opt_duration",
        skip_serializing_if = "Option::is_none"
    )]
    notification_cooldown: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    git_path: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_opt_duration",
        serialize_with = "serialize_opt_duration",
        skip_serializing_if = "Option::is_none"
    )]
    watcher_debounce: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    on_conflict: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolve_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolve_agent_path: Option<String>,
}

/// Raw representation of repos.toml.
/// Keys are repo paths, values are per-repo config.
type RawReposToml = IndexMap<String, RawRepoConfig>;

#[derive(Debug, Serialize, Deserialize)]
struct RawRepoConfig {
    #[serde(
        default,
        deserialize_with = "deserialize_opt_duration",
        serialize_with = "serialize_opt_duration",
        skip_serializing_if = "Option::is_none"
    )]
    refresh_interval: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disabled: Option<Disabled>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RawInRepoConfig {
    #[serde(
        default,
        deserialize_with = "deserialize_opt_duration",
        serialize_with = "serialize_opt_duration",
        skip_serializing_if = "Option::is_none"
    )]
    refresh_interval: Option<Duration>,
}

// ---------------------------------------------------------------------------
// Public data structures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct UserConfig {
    pub global: GlobalSettings,
    pub trusted_hosts: HashSet<String>,
    pub repos: HashMap<String, RepoConfig>,
}

/// What to do when a rebase hits conflicts.
#[derive(Debug, Clone, PartialEq)]
pub enum OnConflict {
    /// Try resolve agent if configured, otherwise leave conflicts. Default.
    Auto,
    /// Abort rebase, restore clean state.
    Revert,
    /// Leave repo in conflict state for user to handle.
    Leave,
    /// Spawn resolve agent (falls back to leave if not configured).
    ResolveAgent,
}

impl OnConflict {
    fn from_str_opt(s: Option<&str>) -> Self {
        match s {
            Some("revert") => OnConflict::Revert,
            Some("leave") => OnConflict::Leave,
            Some("resolve-agent") => OnConflict::ResolveAgent,
            Some("auto") | None => OnConflict::Auto,
            Some(_) => OnConflict::Auto,
        }
    }

    pub fn to_str(&self) -> &'static str {
        match self {
            OnConflict::Auto => "auto",
            OnConflict::Revert => "revert",
            OnConflict::Leave => "leave",
            OnConflict::ResolveAgent => "resolve-agent",
        }
    }

    /// Resolve the effective policy given whether an agent is configured.
    pub fn effective(&self, has_agent: bool) -> EffectiveConflictAction {
        match self {
            OnConflict::Revert => EffectiveConflictAction::Revert,
            OnConflict::Leave => EffectiveConflictAction::Leave,
            OnConflict::ResolveAgent => {
                if has_agent {
                    EffectiveConflictAction::ResolveAgent
                } else {
                    EffectiveConflictAction::Leave
                }
            }
            OnConflict::Auto => {
                if has_agent {
                    EffectiveConflictAction::ResolveAgent
                } else {
                    EffectiveConflictAction::Leave
                }
            }
        }
    }
}

/// The resolved action after considering agent availability.
#[derive(Debug, Clone, PartialEq)]
pub enum EffectiveConflictAction {
    Revert,
    Leave,
    ResolveAgent,
}

#[derive(Debug, Clone)]
pub struct GlobalSettings {
    pub refresh_interval: Duration,
    pub colors: bool,
    pub emoji: bool,
    pub notification_cooldown: Duration,
    pub git_path: Option<String>,
    pub watcher_debounce: Option<Duration>,
    pub on_conflict: OnConflict,
    pub resolve_agent: Option<String>,
    pub resolve_agent_path: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RepoConfig {
    pub refresh_interval: Option<Duration>,
    pub disabled: Option<Disabled>,
}

#[derive(Debug, Clone)]
pub struct InRepoConfig {
    pub refresh_interval: Option<Duration>,
}

// ---------------------------------------------------------------------------
// Defaults (compiled-in)
// ---------------------------------------------------------------------------

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            refresh_interval: Duration::from_secs(60),
            colors: true,
            emoji: true,
            notification_cooldown: Duration::from_secs(300),
            git_path: None,
            watcher_debounce: None,
            on_conflict: OnConflict::Auto,
            resolve_agent: None,
            resolve_agent_path: None,
        }
    }
}


// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

impl UserConfig {
    /// Load merged config from all three files. Missing files use defaults.
    pub fn load(paths: &Paths) -> Result<Self> {
        let global = load_config_toml(&paths.config_file)?;
        let trusted_hosts = load_trusted_hosts(&paths.trusted_hosts_file)?;
        let repos = load_repos_toml(&paths.repos_file)?;

        Ok(Self {
            global,
            trusted_hosts,
            repos,
        })
    }

    // -----------------------------------------------------------------------
    // Trusted hosts
    // -----------------------------------------------------------------------

    /// Add a host to the trusted_hosts file.
    pub fn trust(paths: &Paths, host: &str) -> Result<()> {
        check_nix_managed(&paths.trusted_hosts_file)?;
        ensure_parent(&paths.trusted_hosts_file)?;

        let lock_path = paths.trusted_hosts_file.with_extension("lock");
        let lock_file = std::fs::File::create(&lock_path)
            .with_context(|| format!("failed to create lock file: {}", lock_path.display()))?;
        lock_exclusive(&lock_file)?;

        let mut hosts = load_trusted_hosts(&paths.trusted_hosts_file)?;
        hosts.insert(host.to_string());
        write_trusted_hosts(&paths.trusted_hosts_file, &hosts)?;
        Ok(())
    }

    /// Remove a host from the trusted_hosts file.
    pub fn untrust(paths: &Paths, host: &str) -> Result<()> {
        check_nix_managed(&paths.trusted_hosts_file)?;
        ensure_parent(&paths.trusted_hosts_file)?;

        let lock_path = paths.trusted_hosts_file.with_extension("lock");
        let lock_file = std::fs::File::create(&lock_path)
            .with_context(|| format!("failed to create lock file: {}", lock_path.display()))?;
        lock_exclusive(&lock_file)?;

        let mut hosts = load_trusted_hosts(&paths.trusted_hosts_file)?;
        hosts.remove(host);
        write_trusted_hosts(&paths.trusted_hosts_file, &hosts)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Repos
    // -----------------------------------------------------------------------

    /// Read-modify-write a repo entry in repos.toml under a file lock.
    pub fn update_repo<F>(paths: &Paths, repo_id: &str, f: F) -> Result<()>
    where
        F: FnOnce(&mut RepoConfig),
    {
        ensure_parent(&paths.repos_file)?;

        let lock_path = paths.repos_file.with_extension("toml.lock");
        let lock_file = std::fs::File::create(&lock_path)
            .with_context(|| format!("failed to create lock file: {}", lock_path.display()))?;
        lock_exclusive(&lock_file)?;

        let mut repos = load_repos_toml(&paths.repos_file)?;
        let entry = repos.entry(repo_id.to_string()).or_default();
        f(entry);
        save_repos_toml(&paths.repos_file, &repos)?;
        Ok(())
    }

    /// Remove a repo entry from repos.toml.
    pub fn remove_repo(paths: &Paths, repo_id: &str) -> Result<()> {
        ensure_parent(&paths.repos_file)?;

        let lock_path = paths.repos_file.with_extension("toml.lock");
        let lock_file = std::fs::File::create(&lock_path)
            .with_context(|| format!("failed to create lock file: {}", lock_path.display()))?;
        lock_exclusive(&lock_file)?;

        let mut repos = load_repos_toml(&paths.repos_file)?;
        repos.remove(repo_id);
        save_repos_toml(&paths.repos_file, &repos)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// File loaders
// ---------------------------------------------------------------------------

/// Load config.toml (global settings). Returns defaults if missing.
fn load_config_toml(path: &Path) -> Result<GlobalSettings> {
    if !path.exists() {
        return Ok(GlobalSettings::default());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    let raw: RawConfigToml = toml::from_str(&text).with_context(|| {
        format!("failed to parse config file: {}", path.display())
    })?;
    Ok(GlobalSettings {
        refresh_interval: raw.refresh_interval.unwrap_or(Duration::from_secs(60)),
        colors: raw.colors.unwrap_or(true),
        emoji: raw.emoji.unwrap_or(true),
        notification_cooldown: raw.notification_cooldown.unwrap_or(Duration::from_secs(300)),
        git_path: raw.git_path,
        watcher_debounce: raw.watcher_debounce,
        on_conflict: OnConflict::from_str_opt(raw.on_conflict.as_deref()),
        resolve_agent: raw.resolve_agent,
        resolve_agent_path: raw.resolve_agent_path,
    })
}

/// Load trusted_hosts file (one host per line). Returns empty set if missing.
fn load_trusted_hosts(path: &Path) -> Result<HashSet<String>> {
    if !path.exists() {
        return Ok(HashSet::new());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read trusted hosts: {}", path.display()))?;
    Ok(text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect())
}

/// Write trusted_hosts file atomically.
fn write_trusted_hosts(path: &Path, hosts: &HashSet<String>) -> Result<()> {
    let mut sorted: Vec<&str> = hosts.iter().map(|s| s.as_str()).collect();
    sorted.sort();
    let text = sorted.join("\n") + if sorted.is_empty() { "" } else { "\n" };
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &text)
        .with_context(|| format!("failed to write temp trusted hosts: {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename trusted hosts into place: {}", path.display()))?;
    Ok(())
}

/// Load repos.toml. Returns empty map if missing.
fn load_repos_toml(path: &Path) -> Result<HashMap<String, RepoConfig>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read repos file: {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let raw: RawReposToml = toml::from_str(&text).with_context(|| {
        format!("failed to parse repos file: {}", path.display())
    })?;
    Ok(raw
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                RepoConfig {
                    refresh_interval: v.refresh_interval,
                    disabled: v.disabled,
                },
            )
        })
        .collect())
}

/// Save repos.toml atomically.
fn save_repos_toml(path: &Path, repos: &HashMap<String, RepoConfig>) -> Result<()> {
    let raw: RawReposToml = repos
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                RawRepoConfig {
                    refresh_interval: v.refresh_interval,
                    disabled: v.disabled.clone(),
                },
            )
        })
        .collect();
    let text = toml::to_string_pretty(&raw).context("failed to serialize repos")?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &text)
        .with_context(|| format!("failed to write temp repos: {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename repos into place: {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Nix symlink detection
// ---------------------------------------------------------------------------

/// Check if a file is a nix-store symlink and bail if so.
fn check_nix_managed(path: &Path) -> Result<()> {
    if path.is_symlink()
        && let Ok(target) = std::fs::read_link(path)
        && target.to_string_lossy().starts_with("/nix/store/") {
            anyhow::bail!(
                "{} is managed by nix (symlink to {}). \
                 Edit your nix configuration instead.",
                path.display(),
                target.display()
            );
        }
    Ok(())
}

/// Ensure parent directory exists.
fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory: {}", parent.display()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// File locking
// ---------------------------------------------------------------------------

/// Acquire an exclusive (write) lock on a file.
#[cfg(unix)]
fn lock_exclusive(file: &std::fs::File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        anyhow::bail!("flock failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// Acquire an exclusive (write) lock on a file.
#[cfg(windows)]
fn lock_exclusive(file: &std::fs::File) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle() as _,
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if ok == 0 {
        anyhow::bail!("LockFileEx failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// In-repo config
// ---------------------------------------------------------------------------

impl InRepoConfig {
    /// Load `.gitsitter.toml` from a repo root. Returns `None` if the file doesn't exist.
    pub fn load(repo_root: &Path) -> Result<Option<Self>> {
        let path = repo_root.join(".gitsitter.toml");
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read in-repo config: {}", path.display()))?;
        let raw: RawInRepoConfig = toml::from_str(&text).with_context(|| {
            format!(
                "failed to parse in-repo config: {}. For now, remove it and rerun",
                path.display()
            )
        })?;
        Ok(Some(Self {
            refresh_interval: raw.refresh_interval,
        }))
    }
}

// ---------------------------------------------------------------------------
// Host trust
// ---------------------------------------------------------------------------

impl UserConfig {
    /// Check whether a host is trusted.
    pub fn is_host_trusted(&self, host: &str) -> bool {
        self.trusted_hosts.contains(host)
    }

    /// Check whether a remote URL is trusted.
    ///
    /// Local `file://` URLs are always trusted. Empty URLs (no remote) are trusted.
    /// For everything else, the host must appear in `trusted_hosts`.
    pub fn is_remote_trusted(&self, remote_url: &str) -> bool {
        if remote_url.is_empty() || remote_url.starts_with("file://") {
            return true;
        }
        match extract_host(remote_url) {
            Some(host) => self.is_host_trusted(&host),
            None => false, // can't determine host — not trusted
        }
    }

    /// Check whether a specific remote is disabled in per-repo config.
    pub fn is_remote_disabled(&self, repo_path: &str, remote_name: &str) -> bool {
        self.repos
            .get(repo_path)
            .and_then(|r| r.disabled.as_ref())
            .is_some_and(|d| d.is_remote_disabled(remote_name))
    }

    /// Check if a repo is explicitly disabled in user config.
    pub fn is_repo_disabled(&self, repo_path: &str) -> bool {
        self.repos
            .get(repo_path)
            .and_then(|r| r.disabled.as_ref())
            .is_some_and(|d| d.is_repo_disabled())
    }

    /// Get the effective refresh interval for a repo.
    /// User per-repo > in-repo > global.
    pub fn effective_refresh_interval(
        &self,
        repo_path: &str,
        in_repo_config: Option<&InRepoConfig>,
    ) -> Duration {
        if let Some(repo_cfg) = self.repos.get(repo_path)
            && let Some(d) = repo_cfg.refresh_interval {
                return d;
            }
        if let Some(irc) = in_repo_config
            && let Some(d) = irc.refresh_interval {
                return d;
            }
        self.global.refresh_interval
    }
}

// ---------------------------------------------------------------------------
// URL helpers
// ---------------------------------------------------------------------------

/// Extract the hostname from a git remote URL.
///
/// Handles:
///  - `git@github.com:user/repo.git` (SSH)
///  - `ssh://git@github.com/user/repo.git`
///  - `https://github.com/user/repo.git`
///  - `http://github.com/user/repo.git`
///  - `git://github.com/user/repo.git`
pub fn extract_host(remote_url: &str) -> Option<String> {
    // Try scheme-based URLs first: https://host/... , ssh://user@host/... , git://host/...
    if let Some(after_scheme) = remote_url
        .strip_prefix("https://")
        .or_else(|| remote_url.strip_prefix("http://"))
        .or_else(|| remote_url.strip_prefix("ssh://"))
        .or_else(|| remote_url.strip_prefix("git://"))
    {
        // Strip optional user@ prefix
        let after_user = match after_scheme.find('@') {
            Some(at) => {
                let slash = after_scheme.find('/').unwrap_or(after_scheme.len());
                if at < slash {
                    &after_scheme[at + 1..]
                } else {
                    after_scheme
                }
            }
            None => after_scheme,
        };
        // Host is up to the first '/' or ':'
        let end = after_user
            .find('/')
            .or_else(|| after_user.find(':'))
            .unwrap_or(after_user.len());
        let host = &after_user[..end];
        // Strip port if present
        let host = host.split(':').next().unwrap_or(host);
        if host.is_empty() {
            return None;
        }
        return Some(host.to_string());
    }

    // SCP-style: user@host:path
    if let Some(at) = remote_url.find('@') {
        let rest = &remote_url[at + 1..];
        if let Some(colon) = rest.find(':') {
            let host = &rest[..colon];
            if !host.is_empty() && !host.contains('/') {
                return Some(host.to_string());
            }
        }
    }

    None
}
