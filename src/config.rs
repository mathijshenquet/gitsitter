//! Configuration parsing and hierarchical resolution for gitsitter.
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
// Sync modes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoSyncMode {
    None,
    Fetch,
    Pull,
    Push,
    #[serde(rename = "push+pull")]
    PushPull,
}

impl std::fmt::Display for RepoSyncMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoSyncMode::None => write!(f, "none"),
            RepoSyncMode::Fetch => write!(f, "fetch"),
            RepoSyncMode::Pull => write!(f, "pull"),
            RepoSyncMode::Push => write!(f, "push"),
            RepoSyncMode::PushPull => write!(f, "push+pull"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchSyncMode {
    Inherit,
    None,
    Fetch,
    Pull,
    Push,
    #[serde(rename = "push+pull")]
    PushPull,
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
    defaults: Option<RawDefaultsConfig>,
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
}

#[derive(Debug, Serialize, Deserialize)]
struct RawDefaultsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remotes: Option<Vec<RawRemoteRule>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branches: Option<Vec<RawBranchRule>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawRemoteRule {
    pattern: String,
    mode: RepoSyncMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawBranchRule {
    pattern: String,
    mode: BranchSyncMode,
}

#[derive(Debug, Serialize, Deserialize)]
struct RawRepoConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<RepoSyncMode>,
    #[serde(
        default,
        deserialize_with = "deserialize_opt_duration",
        serialize_with = "serialize_opt_duration",
        skip_serializing_if = "Option::is_none"
    )]
    refresh_interval: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branches: Option<Vec<RawBranchRule>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RawInRepoConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<RepoSyncMode>,
    #[serde(
        default,
        deserialize_with = "deserialize_opt_duration",
        serialize_with = "serialize_opt_duration",
        skip_serializing_if = "Option::is_none"
    )]
    refresh_interval: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branches: Option<Vec<RawBranchRule>>,
}

// ---------------------------------------------------------------------------
// Public data structures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct UserConfig {
    pub global: GlobalSettings,
    pub trusted_hosts: HashMap<String, bool>,
    pub defaults: DefaultsConfig,
    pub repos: HashMap<String, RepoConfig>,
}

#[derive(Debug, Clone)]
pub struct GlobalSettings {
    pub refresh_interval: Duration,
    pub colors: bool,
    pub emoji: bool,
    pub notification_cooldown: Duration,
    pub git_path: Option<String>,
    pub watcher_debounce: Option<Duration>,
}

#[derive(Debug, Clone, Default)]
pub struct DefaultsConfig {
    /// Ordered list of (glob_pattern, mode). First match wins.
    pub remotes: Vec<(String, RepoSyncMode)>,
    /// Ordered list of (pattern, mode). Evaluated by specificity.
    pub branches: Vec<(String, BranchSyncMode)>,
}

#[derive(Debug, Clone, Default)]
pub struct RepoConfig {
    pub mode: Option<RepoSyncMode>,
    pub refresh_interval: Option<Duration>,
    pub disabled: Option<bool>,
    /// Ordered list of (pattern, mode).
    pub branches: Vec<(String, BranchSyncMode)>,
}

#[derive(Debug, Clone)]
pub struct InRepoConfig {
    pub mode: Option<RepoSyncMode>,
    pub refresh_interval: Option<Duration>,
    pub branches: Vec<(String, BranchSyncMode)>,
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

fn remote_rules_to_vec(rules: Option<Vec<RawRemoteRule>>) -> Vec<(String, RepoSyncMode)> {
    rules
        .unwrap_or_default()
        .into_iter()
        .map(|rule| (rule.pattern, rule.mode))
        .collect()
}

fn branch_rules_to_vec(rules: Option<Vec<RawBranchRule>>) -> Vec<(String, BranchSyncMode)> {
    rules
        .unwrap_or_default()
        .into_iter()
        .map(|rule| (rule.pattern, rule.mode))
        .collect()
}

fn vec_to_remote_rules(v: &[(String, RepoSyncMode)]) -> Vec<RawRemoteRule> {
    v.iter()
        .map(|(pattern, mode)| RawRemoteRule {
            pattern: pattern.clone(),
            mode: *mode,
        })
        .collect()
}

fn vec_to_branch_rules(v: &[(String, BranchSyncMode)]) -> Vec<RawBranchRule> {
    v.iter()
        .map(|(pattern, mode)| RawBranchRule {
            pattern: pattern.clone(),
            mode: *mode,
        })
        .collect()
}

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
            },
            None => GlobalSettings::default(),
        };
        let trusted_hosts = raw.trusted_hosts.map(|m| m.into_iter().collect()).unwrap_or_default();
        let defaults = match raw.defaults {
            Some(d) => DefaultsConfig {
                remotes: remote_rules_to_vec(d.remotes),
                branches: branch_rules_to_vec(d.branches),
            },
            None => DefaultsConfig::default(),
        };
        let repos = match raw.repos {
            Some(m) => m
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        RepoConfig {
                            mode: v.mode,
                            refresh_interval: v.refresh_interval,
                            disabled: v.disabled,
                            branches: branch_rules_to_vec(v.branches),
                        },
                    )
                })
                .collect(),
            None => HashMap::new(),
        };
        Self {
            global,
            trusted_hosts,
            defaults,
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
        });
        let trusted_hosts = if cfg.trusted_hosts.is_empty() {
            None
        } else {
            Some(cfg.trusted_hosts.iter().map(|(k, v)| (k.clone(), *v)).collect())
        };
        let defaults = {
            let d = &cfg.defaults;
            if d.remotes.is_empty() && d.branches.is_empty() {
                None
            } else {
                Some(RawDefaultsConfig {
                    remotes: if d.remotes.is_empty() {
                        None
                    } else {
                        Some(vec_to_remote_rules(&d.remotes))
                    },
                    branches: if d.branches.is_empty() {
                        None
                    } else {
                        Some(vec_to_branch_rules(&d.branches))
                    },
                })
            }
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
                                mode: v.mode,
                                refresh_interval: v.refresh_interval,
                                disabled: v.disabled,
                                branches: if v.branches.is_empty() {
                                    None
                                } else {
                                    Some(vec_to_branch_rules(&v.branches))
                                },
                            },
                        )
                    })
                    .collect(),
            )
        };
        RawUserConfig {
            global,
            trusted_hosts,
            defaults,
            repos,
        }
    }
}

impl From<RawInRepoConfig> for InRepoConfig {
    fn from(raw: RawInRepoConfig) -> Self {
        Self {
            mode: raw.mode,
            refresh_interval: raw.refresh_interval,
            branches: branch_rules_to_vec(raw.branches),
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
            eprintln!("initialized config at {}", config_file.display());
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
}

// ---------------------------------------------------------------------------
// URL / glob helpers
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

/// Check if a remote URL matches a glob pattern using simple prefix/wildcard matching.
///
/// The pattern uses `*` as a wildcard that matches any sequence of characters.
/// This is a simple glob, not a full regex.
pub fn matches_remote_glob(url: &str, pattern: &str) -> bool {
    simple_glob_match(url, pattern)
}

/// Check if a branch name matches a pattern. Supports `*` wildcard.
pub fn matches_branch_glob(branch: &str, pattern: &str) -> bool {
    simple_glob_match(branch, pattern)
}

/// Simple glob matching with `*` wildcard.
fn simple_glob_match(text: &str, pattern: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        // No wildcard — exact match
        return text == pattern;
    }

    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            // First segment must be a prefix
            if !text[pos..].starts_with(part) {
                return false;
            }
            pos += part.len();
        } else if i == parts.len() - 1 {
            // Last segment must be a suffix
            if !text[pos..].ends_with(part) {
                return false;
            }
            pos = text.len();
        } else {
            // Middle segment must appear somewhere after pos
            match text[pos..].find(part) {
                Some(found) => pos += found + part.len(),
                None => return false,
            }
        }
    }
    true
}

/// Returns true if the pattern is an exact name (no wildcards).
fn is_exact_pattern(pattern: &str) -> bool {
    !pattern.contains('*')
}

// ---------------------------------------------------------------------------
// Config resolution
// ---------------------------------------------------------------------------

impl UserConfig {
    /// Resolve the effective repo sync mode.
    ///
    /// Resolution order (first match wins):
    /// 0. Remote host not trusted -> None
    /// 1. User per-repo disabled=true -> None (caller should treat as disabled)
    /// 2. User per-repo mode
    /// 3. In-repo .gitsitter.toml mode
    /// 4. First matching defaults.remotes glob
    /// 5. Fallback: Pull
    pub fn resolve_repo_mode(
        &self,
        remote_url: &str,
        repo_path: &str,
        in_repo_config: Option<&InRepoConfig>,
    ) -> RepoSyncMode {
        // Step 0: host trust check (local file:// URLs are always trusted)
        if !remote_url.starts_with("file://") {
            if let Some(host) = extract_host(remote_url) {
                if !self.is_host_trusted(&host) {
                    return RepoSyncMode::None;
                }
            } else {
                // Can't determine host — don't make network requests
                return RepoSyncMode::None;
            }
        }

        // Step 1 & 2: user per-repo config
        if let Some(repo_cfg) = self.repos.get(repo_path) {
            if repo_cfg.disabled == Some(true) {
                return RepoSyncMode::None;
            }
            if let Some(mode) = repo_cfg.mode {
                return mode;
            }
        }

        // Step 3: in-repo config
        if let Some(irc) = in_repo_config {
            if let Some(mode) = irc.mode {
                return mode;
            }
        }

        // Step 4: defaults.remotes globs (first match wins)
        for (pattern, mode) in &self.defaults.remotes {
            if matches_remote_glob(remote_url, pattern) {
                return *mode;
            }
        }

        // Step 5: fallback
        RepoSyncMode::Pull
    }

    /// Resolve the effective branch sync mode.
    ///
    /// Resolution order:
    /// 1. Exact user per-repo branch rule
    /// 2. Longest matching user per-repo branch glob
    /// 3. Exact in-repo branch rule
    /// 4. Longest matching in-repo branch glob
    /// 5. Exact defaults.branches rule
    /// 6. Longest matching defaults.branches glob
    /// 7. Inherit (from repo effective mode)
    ///
    /// Within any layer: exact name beats glob, longer glob beats shorter,
    /// declaration order breaks ties.
    pub fn resolve_branch_mode(
        &self,
        repo_path: &str,
        branch_name: &str,
        in_repo_config: Option<&InRepoConfig>,
        repo_effective_mode: RepoSyncMode,
    ) -> BranchSyncMode {
        // Helper: search a branch list for the best match.
        // Returns Some(mode) if an exact match or glob match is found.
        fn find_best_match(
            branches: &[(String, BranchSyncMode)],
            branch_name: &str,
        ) -> Option<BranchSyncMode> {
            // First pass: look for exact match (first one wins)
            for (pattern, mode) in branches {
                if is_exact_pattern(pattern) && pattern == branch_name {
                    return Some(*mode);
                }
            }
            // Second pass: longest matching glob (declaration order breaks ties)
            let mut best: Option<(usize, BranchSyncMode)> = None;
            for (pattern, mode) in branches {
                if !is_exact_pattern(pattern) && matches_branch_glob(branch_name, pattern) {
                    let len = pattern.len();
                    if best.is_none() || len > best.unwrap().0 {
                        best = Some((len, *mode));
                    }
                }
            }
            best.map(|(_, m)| m)
        }

        // Layer 1-2: user per-repo branch rules
        if let Some(repo_cfg) = self.repos.get(repo_path) {
            if let Some(mode) = find_best_match(&repo_cfg.branches, branch_name) {
                if mode != BranchSyncMode::Inherit {
                    return mode;
                }
                // Explicit inherit in user config means fall through to repo mode
                return branch_mode_from_repo(repo_effective_mode);
            }
        }

        // Layer 3-4: in-repo config branch rules
        if let Some(irc) = in_repo_config {
            if let Some(mode) = find_best_match(&irc.branches, branch_name) {
                if mode != BranchSyncMode::Inherit {
                    return mode;
                }
                return branch_mode_from_repo(repo_effective_mode);
            }
        }

        // Layer 5-6: defaults.branches
        if let Some(mode) = find_best_match(&self.defaults.branches, branch_name) {
            if mode != BranchSyncMode::Inherit {
                return mode;
            }
            return branch_mode_from_repo(repo_effective_mode);
        }

        // Layer 7: inherit from repo
        branch_mode_from_repo(repo_effective_mode)
    }

    /// Check if a repo is explicitly disabled in user config.
    pub fn is_repo_disabled(&self, repo_path: &str) -> bool {
        self.repos
            .get(repo_path)
            .and_then(|r| r.disabled)
            .unwrap_or(false)
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

/// Convert a repo-level sync mode to the equivalent branch-level mode
/// (used when a branch inherits from its repo).
fn branch_mode_from_repo(repo_mode: RepoSyncMode) -> BranchSyncMode {
    match repo_mode {
        RepoSyncMode::None => BranchSyncMode::None,
        RepoSyncMode::Fetch => BranchSyncMode::Fetch,
        RepoSyncMode::Pull => BranchSyncMode::Pull,
        RepoSyncMode::Push => BranchSyncMode::Push,
        RepoSyncMode::PushPull => BranchSyncMode::PushPull,
    }
}
