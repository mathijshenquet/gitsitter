//! Configuration parsing for gitsitter.
//!
//! Handles TOML config files at two levels:
//! - User config: `~/.config/gitsitter/config.toml`
//! - In-repo config: `.gitsitter.toml` in a repo's root

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

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
    let (num, unit) = if s.ends_with("ms") {
        (&s[..s.len() - 2], "ms")
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
    if secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else if secs % 60 == 0 {
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
// Raw serde structs (what the TOML file looks like on disk)
// ---------------------------------------------------------------------------

/// Raw representation of the user config file for serde.
#[derive(Debug, Serialize, Deserialize)]
struct RawUserConfig {
    #[serde(default)]
    global: Option<RawGlobalSettings>,
    #[serde(default)]
    trusted_hosts: Option<IndexMap<String, bool>>,
    #[serde(default)]
    repos: Option<IndexMap<String, RawRepoConfig>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RawGlobalSettings {
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

#[derive(Debug, Clone)]
pub struct UserConfig {
    pub global: GlobalSettings,
    pub trusted_hosts: HashMap<String, bool>,
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

const DEFAULT_CONFIG_TOML: &str = include_str!("../config/default-config.toml");

fn default_user_config() -> &'static UserConfig {
    static DEFAULT_CONFIG: OnceLock<UserConfig> = OnceLock::new();
    DEFAULT_CONFIG.get_or_init(|| {
        let raw: RawUserConfig = toml::from_str(DEFAULT_CONFIG_TOML)
            .expect("embedded default-config.toml must parse");
        raw.into()
    })
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

impl Default for GlobalSettings {
    fn default() -> Self {
        default_user_config().global.clone()
    }
}

impl Default for UserConfig {
    fn default() -> Self {
        default_user_config().clone()
    }
}

// ---------------------------------------------------------------------------
// Conversions raw <-> public
// ---------------------------------------------------------------------------

impl From<RawUserConfig> for UserConfig {
    fn from(raw: RawUserConfig) -> Self {
        let global = match raw.global {
            Some(g) => GlobalSettings {
                refresh_interval: g.refresh_interval.unwrap_or(Duration::from_secs(60)),
                colors: g.colors.unwrap_or(true),
                emoji: g.emoji.unwrap_or(true),
                notification_cooldown: g.notification_cooldown.unwrap_or(Duration::from_secs(300)),
                git_path: g.git_path,
                watcher_debounce: g.watcher_debounce,
                on_conflict: OnConflict::from_str_opt(g.on_conflict.as_deref()),
                resolve_agent: g.resolve_agent,
                resolve_agent_path: g.resolve_agent_path,
            },
            None => GlobalSettings::default(),
        };
        let trusted_hosts = raw.trusted_hosts.map(|m| m.into_iter().collect()).unwrap_or_default();
        let repos = match raw.repos {
            Some(m) => m
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
                .collect(),
            None => HashMap::new(),
        };
        Self {
            global,
            trusted_hosts,
            repos,
        }
    }
}

impl From<&UserConfig> for RawUserConfig {
    fn from(cfg: &UserConfig) -> Self {
        let global = Some(RawGlobalSettings {
            refresh_interval: Some(cfg.global.refresh_interval),
            colors: Some(cfg.global.colors),
            emoji: Some(cfg.global.emoji),
            notification_cooldown: Some(cfg.global.notification_cooldown),
            git_path: cfg.global.git_path.clone(),
            watcher_debounce: cfg.global.watcher_debounce,
            on_conflict: Some(cfg.global.on_conflict.to_str().to_string()),
            resolve_agent: cfg.global.resolve_agent.clone(),
            resolve_agent_path: cfg.global.resolve_agent_path.clone(),
        });
        let trusted_hosts = if cfg.trusted_hosts.is_empty() {
            None
        } else {
            Some(cfg.trusted_hosts.iter().map(|(k, v)| (k.clone(), *v)).collect())
        };
        let repos = if cfg.repos.is_empty() {
            None
        } else {
            Some(
                cfg.repos
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
                    .collect(),
            )
        };
        RawUserConfig {
            global,
            trusted_hosts,
            repos,
        }
    }
}

impl From<RawInRepoConfig> for InRepoConfig {
    fn from(raw: RawInRepoConfig) -> Self {
        Self {
            refresh_interval: raw.refresh_interval,
        }
    }
}

// ---------------------------------------------------------------------------
// Loading & saving
// ---------------------------------------------------------------------------

impl UserConfig {
    /// Load user config, creating the default if the file doesn't exist.
    pub fn load(config_file: &Path) -> Result<Self> {
        if !config_file.exists() {
            if let Some(parent) = config_file.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create config directory: {}", parent.display()))?;
            }
            std::fs::write(config_file, DEFAULT_CONFIG_TOML)
                .with_context(|| format!("failed to initialize config file: {}", config_file.display()))?;
            // Config file created silently — CLI commands handle user-facing output.
        }
        let text = std::fs::read_to_string(config_file)
            .with_context(|| format!("failed to read config file: {}", config_file.display()))?;
        let raw: RawUserConfig = toml::from_str(&text).with_context(|| {
            format!(
                "failed to parse config file: {}. Remove it and rerun gitsitter to regenerate the default config",
                config_file.display()
            )
        })?;
        Ok(raw.into())
    }

    /// Atomically read-modify-write the config file under a file lock.
    ///
    /// Safe to call concurrently from multiple processes (CLI + daemon).
    pub fn modify<F>(config_file: &Path, f: F) -> Result<()>
    where
        F: FnOnce(&mut UserConfig),
    {
        let lock_path = config_file.with_extension("toml.lock");

        if let Some(parent) = config_file.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create config directory: {}", parent.display()))?;
        }

        let lock_file = std::fs::File::create(&lock_path)
            .with_context(|| format!("failed to create lock file: {}", lock_path.display()))?;
        lock_exclusive(&lock_file)
            .context("failed to acquire config lock")?;

        let mut cfg = Self::load(config_file)?;
        f(&mut cfg);

        // Write to temp file and atomically rename
        let raw = RawUserConfig::from(&cfg);
        let text = toml::to_string_pretty(&raw).context("failed to serialize config")?;
        let tmp = config_file.with_extension("toml.tmp");
        std::fs::write(&tmp, &text)
            .with_context(|| format!("failed to write temp config: {}", tmp.display()))?;
        std::fs::rename(&tmp, config_file)
            .with_context(|| format!("failed to rename config into place: {}", config_file.display()))?;

        // Lock released on drop of lock_file
        Ok(())
    }
}

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
        Ok(Some(raw.into()))
    }
}

// ---------------------------------------------------------------------------
// Host trust
// ---------------------------------------------------------------------------

impl UserConfig {
    /// Check whether a host is trusted.
    pub fn is_host_trusted(&self, host: &str) -> bool {
        self.trusted_hosts.get(host).copied().unwrap_or(false)
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
            .map_or(false, |d| d.is_remote_disabled(remote_name))
    }

    /// Check if a repo is explicitly disabled in user config.
    pub fn is_repo_disabled(&self, repo_path: &str) -> bool {
        self.repos
            .get(repo_path)
            .and_then(|r| r.disabled.as_ref())
            .map_or(false, |d| d.is_repo_disabled())
    }

    /// Get the effective refresh interval for a repo.
    /// User per-repo > in-repo > global.
    pub fn effective_refresh_interval(
        &self,
        repo_path: &str,
        in_repo_config: Option<&InRepoConfig>,
    ) -> Duration {
        if let Some(repo_cfg) = self.repos.get(repo_path) {
            if let Some(d) = repo_cfg.refresh_interval {
                return d;
            }
        }
        if let Some(irc) = in_repo_config {
            if let Some(d) = irc.refresh_interval {
                return d;
            }
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
