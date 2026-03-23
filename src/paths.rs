//! XDG and platform-specific path resolution for gitsitter.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Returns the gitsitter config directory.
///
/// Resolution order:
/// 1. `GITSITTER_CONFIG_DIR` (for testing)
/// 2. `$XDG_CONFIG_HOME/gitsitter/`
/// 3. `~/.config/gitsitter/`
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("GITSITTER_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::config_dir())
        .unwrap_or_else(|| {
            let mut p = dirs::home_dir().expect("cannot determine home directory");
            p.push(".config");
            p
        })
        .join("gitsitter")
}

/// Returns the path to `config.toml`.
pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// Returns the gitsitter state directory.
///
/// Resolution order:
/// 1. `GITSITTER_STATE_DIR` (for testing)
/// 2. `$XDG_STATE_HOME/gitsitter/`
/// 3. `~/.local/state/gitsitter/`
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("GITSITTER_STATE_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::state_dir())
        .unwrap_or_else(|| {
            let mut p = dirs::home_dir().expect("cannot determine home directory");
            p.push(".local/state");
            p
        })
        .join("gitsitter")
}

/// Returns the path to `state.db`.
pub fn state_db() -> PathBuf {
    state_dir().join("state.db")
}

/// Returns the path to `daemon.log`.
pub fn daemon_log() -> PathBuf {
    state_dir().join("daemon.log")
}

/// Returns the path to `daemon.pid`.
pub fn daemon_pid() -> PathBuf {
    state_dir().join("daemon.pid")
}

/// Returns the daemon IPC endpoint path.
///
/// Resolution order:
/// 1. `GITSITTER_SOCKET_PATH` (for testing)
/// 2. Platform default runtime path / named pipe
pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("GITSITTER_SOCKET_PATH") {
        return PathBuf::from(path);
    }
    platform_socket_path()
}

/// Creates the config and state directories if they don't exist.
pub fn ensure_dirs() -> Result<()> {
    std::fs::create_dir_all(config_dir())
        .context("failed to create config directory")?;
    std::fs::create_dir_all(state_dir())
        .context("failed to create state directory")?;
    Ok(())
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
    fn config_file_ends_with_config_toml() {
        let p = config_file();
        assert_eq!(p.file_name().unwrap(), "config.toml");
        assert!(p.parent().unwrap().ends_with("gitsitter"));
    }

    #[test]
    fn state_paths_under_state_dir() {
        let dir = state_dir();
        assert!(state_db().starts_with(&dir));
        assert!(daemon_log().starts_with(&dir));
        assert!(daemon_pid().starts_with(&dir));
    }

    #[test]
    fn socket_path_is_absolute() {
        assert!(socket_path().is_absolute());
    }

    #[test]
    fn respects_xdg_config_home() {
        if cfg!(windows) {
            return;
        }
        // Temporarily override — won't affect other threads in parallel tests,
        // but fine for a unit test.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/tmp/test-xdg-config") };
        assert_eq!(config_dir(), PathBuf::from("/tmp/test-xdg-config/gitsitter"));
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }

    #[test]
    fn respects_xdg_state_home() {
        if cfg!(windows) {
            return;
        }
        unsafe { std::env::set_var("XDG_STATE_HOME", "/tmp/test-xdg-state") };
        assert_eq!(state_dir(), PathBuf::from("/tmp/test-xdg-state/gitsitter"));
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
    }
}
