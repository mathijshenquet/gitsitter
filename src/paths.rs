//! Path resolution for gitsitter.
//!
//! [`Paths`] is resolved once at startup and threaded through the program.
//! Tests construct it directly with temp directories — no env var races.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Resolved set of gitsitter paths. Immutable after construction.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_file: PathBuf,
    pub repos_file: PathBuf,
    pub trusted_hosts_file: PathBuf,
    pub daemon_log: PathBuf,
    pub daemon_pid: PathBuf,
    pub socket_path: PathBuf,
}

impl Paths {
    /// Resolve paths from environment variables and platform defaults.
    pub fn resolve() -> Self {
        let config_dir = std::env::var_os("GITSITTER_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(dirs::config_dir)
            .expect("cannot determine config directory")
            .join("gitsitter");

        let state_dir = std::env::var_os("GITSITTER_STATE_DIR")
            .map(PathBuf::from)
            .or_else(dirs::state_dir)
            .or_else(dirs::data_local_dir) // Windows has no state dir, use local app data as fallback
            .expect("cannot determine state directory")
            .join("gitsitter");

        let socket_path = std::env::var_os("GITSITTER_SOCKET_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(platform_socket_path);

        Self {
            config_file: config_dir.join("config.toml"),
            repos_file: config_dir.join("repos.toml"),
            trusted_hosts_file: config_dir.join("trusted_hosts"),
            daemon_log: state_dir.join("daemon.log"),
            daemon_pid: state_dir.join("daemon.pid"),
            socket_path,
        }
    }

    /// Creates parent directories for all paths if they don't exist.
    pub fn ensure_dirs(&self) -> Result<()> {
        for path in [
            &self.config_file,
            &self.repos_file,
            &self.daemon_log,
            &self.daemon_pid,
        ] {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create directory: {}", parent.display()))?;
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn platform_socket_path() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime).join("gitsitter.sock")
    } else {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/gitsitter-{uid}.sock"))
    }
}

#[cfg(windows)]
fn platform_socket_path() -> PathBuf {
    let user = std::env::var("USERNAME")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "default".to_string());
    let sanitized: String = user
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    PathBuf::from(format!(r"\\.\pipe\gitsitter-{}", sanitized))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_produces_absolute_paths() {
        let p = Paths::resolve();
        assert!(p.config_file.is_absolute());
        assert!(p.socket_path.is_absolute());
    }

    #[test]
    fn direct_construction() {
        let p = Paths {
            config_file: PathBuf::from("/tmp/test/config.toml"),
            repos_file: PathBuf::from("/tmp/test/repos.toml"),
            trusted_hosts_file: PathBuf::from("/tmp/test/trusted_hosts"),
            daemon_log: PathBuf::from("/tmp/test/daemon.log"),
            daemon_pid: PathBuf::from("/tmp/test/daemon.pid"),
            socket_path: PathBuf::from("/tmp/test/test.sock"),
        };
        assert_eq!(p.config_file.file_name().unwrap(), "config.toml");
        assert_eq!(p.repos_file.file_name().unwrap(), "repos.toml");
        assert_eq!(p.trusted_hosts_file.file_name().unwrap(), "trusted_hosts");
        assert_eq!(p.daemon_log.file_name().unwrap(), "daemon.log");
    }
}
