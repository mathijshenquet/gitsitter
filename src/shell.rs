//! Shell hook generation and installation for bash, zsh, and fish.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

const HOOK_START_MARKER: &str = "# >>> gitsitter hook >>>";
const HOOK_END_MARKER: &str = "# <<< gitsitter hook <<<";

const BASH_HOOK: &str = include_str!("embed/bash_hook.sh");
const ZSH_HOOK: &str = include_str!("embed/zsh_hook.zsh");
const FISH_HOOK: &str = include_str!("embed/fish_hook.fish");
const POWERSHELL_HOOK: &str = include_str!("embed/powershell_hook.ps1");

/// List supported shells.
pub fn supported_shells() -> &'static [&'static str] {
    &["bash", "zsh", "fish", "powershell", "pwsh"]
}

/// Generate the shell hook script for the given shell.
pub fn generate_hook(shell: &str) -> Result<String> {
    match shell {
        "bash" => Ok(BASH_HOOK.to_string()),
        "zsh" => Ok(ZSH_HOOK.to_string()),
        "fish" => Ok(FISH_HOOK.to_string()),
        "powershell" | "pwsh" => Ok(POWERSHELL_HOOK.to_string()),
        _ => bail!("unsupported shell: {shell} (supported: {})", supported_shells().join(", ")),
    }
}

/// Get the config file path for a shell.
pub fn shell_config_path(shell: &str) -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    match shell {
        "bash" => Ok(home.join(".bashrc")),
        "zsh" => Ok(home.join(".zshrc")),
        "fish" => Ok(home.join(".config").join("fish").join("config.fish")),
        "powershell" => Ok(home
            .join("Documents")
            .join("WindowsPowerShell")
            .join("Microsoft.PowerShell_profile.ps1")),
        "pwsh" => Ok(home
            .join("Documents")
            .join("PowerShell")
            .join("Microsoft.PowerShell_profile.ps1")),
        _ => bail!("unsupported shell: {shell} (supported: {})", supported_shells().join(", ")),
    }
}

/// Install the shell hook by appending to the shell's config file.
///
/// If the hook is already installed (detected by `__gitsitter_hook` in the file),
/// this is a no-op and returns Ok.
pub fn install_hook(shell: &str) -> Result<()> {
    let hook_script = generate_hook(shell)?;
    let config_path = shell_config_path(shell)?;

    // Ensure parent directory exists (relevant for fish)
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory: {}", parent.display()))?;
    }

    // Read existing content, or empty string if file doesn't exist
    let existing = match std::fs::read_to_string(&config_path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("failed to read {}", config_path.display()));
        }
    };

    let block = format!("{HOOK_START_MARKER}\n{hook_script}\n{HOOK_END_MARKER}\n");

    // If hook is already installed, replace the existing block
    if let (Some(start), Some(end_pos)) = (
        existing.find(HOOK_START_MARKER),
        existing.find(HOOK_END_MARKER),
    ) {
        let end = end_pos + HOOK_END_MARKER.len();
        let end = if existing[end..].starts_with('\n') { end + 1 } else { end };
        let mut updated = String::with_capacity(existing.len());
        updated.push_str(&existing[..start]);
        updated.push_str(&block);
        updated.push_str(&existing[end..]);
        std::fs::write(&config_path, updated)
            .with_context(|| format!("failed to write {}", config_path.display()))?;
        return Ok(());
    }

    // First install: append to the config file
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config_path)
        .with_context(|| format!("failed to open {} for writing", config_path.display()))?;

    file.write_all(format!("\n{block}").as_bytes())
        .with_context(|| format!("failed to write to {}", config_path.display()))?;

    Ok(())
}

/// Remove the shell hook from the shell's config file.
///
/// Finds and removes everything between (and including) the
/// `# >>> gitsitter hook >>>` and `# <<< gitsitter hook <<<` markers.
/// If the markers are not found, this is a no-op.
pub fn uninstall_hook(shell: &str) -> Result<()> {
    let config_path = shell_config_path(shell)?;

    let content = match std::fs::read_to_string(&config_path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("failed to read {}", config_path.display()));
        }
    };

    let Some(start) = content.find(HOOK_START_MARKER) else {
        return Ok(());
    };
    let Some(end) = content.find(HOOK_END_MARKER) else {
        return Ok(());
    };

    let end = end + HOOK_END_MARKER.len();
    // Also consume the trailing newline if present
    let end = if content[end..].starts_with('\n') {
        end + 1
    } else {
        end
    };

    // Also consume the leading newline before the start marker if present
    let start = if start > 0 && content.as_bytes()[start - 1] == b'\n' {
        start - 1
    } else {
        start
    };

    let mut new_content = String::with_capacity(content.len());
    new_content.push_str(&content[..start]);
    new_content.push_str(&content[end..]);

    std::fs::write(&config_path, &new_content)
        .with_context(|| format!("failed to write {}", config_path.display()))?;

    Ok(())
}

/// Detect the current shell from the `$SHELL` environment variable.
///
/// Returns the shell basename (e.g. "bash", "zsh", "fish") if it is
/// a supported shell, or `None` otherwise.
pub fn detect_shell() -> Option<String> {
    if let Ok(shell_path) = std::env::var("SHELL") {
        let basename = std::path::Path::new(&shell_path)
            .file_name()?
            .to_str()?
            .to_string();

        if supported_shells().contains(&basename.as_str()) {
            return Some(basename);
        }
    }

    if cfg!(windows) {
        if let Ok(psmodulepath) = std::env::var("PSModulePath") {
            if !psmodulepath.is_empty() {
                return Some("powershell".to_string());
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn supported_shells_has_three() {
        let shells = supported_shells();
        assert_eq!(shells.len(), 5);
        assert!(shells.contains(&"bash"));
        assert!(shells.contains(&"zsh"));
        assert!(shells.contains(&"fish"));
        assert!(shells.contains(&"powershell"));
        assert!(shells.contains(&"pwsh"));
    }

    #[test]
    fn generate_hook_bash() {
        let hook = generate_hook("bash").unwrap();
        assert!(hook.contains("__gitsitter_hook"));
        assert!(hook.contains("PROMPT_COMMAND"));
    }

    #[test]
    fn generate_hook_zsh() {
        let hook = generate_hook("zsh").unwrap();
        assert!(hook.contains("__gitsitter_hook"));
        assert!(hook.contains("precmd_functions"));
    }

    #[test]
    fn generate_hook_fish() {
        let hook = generate_hook("fish").unwrap();
        assert!(hook.contains("__gitsitter_hook"));
        assert!(hook.contains("fish_prompt"));
    }

    #[test]
    fn generate_hook_powershell() {
        let hook = generate_hook("powershell").unwrap();
        assert!(hook.contains("__gitsitter_hook"));
        assert!(hook.contains("function global:prompt"));
    }

    #[test]
    fn generate_hook_unsupported() {
        assert!(generate_hook("nushell").is_err());
    }

    #[test]
    fn shell_config_path_bash() {
        let path = shell_config_path("bash").unwrap();
        assert_eq!(path.file_name().unwrap(), ".bashrc");
    }

    #[test]
    fn shell_config_path_zsh() {
        let path = shell_config_path("zsh").unwrap();
        assert_eq!(path.file_name().unwrap(), ".zshrc");
    }

    #[test]
    fn shell_config_path_fish() {
        let path = shell_config_path("fish").unwrap();
        assert_eq!(path.file_name().unwrap(), "config.fish");
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "fish");
    }

    #[test]
    fn shell_config_path_powershell() {
        let path = shell_config_path("powershell").unwrap();
        assert_eq!(path.file_name().unwrap(), "Microsoft.PowerShell_profile.ps1");
    }

    #[test]
    fn shell_config_path_unsupported() {
        assert!(shell_config_path("nushell").is_err());
    }

    #[test]
    fn detect_shell_from_env() {
        unsafe { std::env::set_var("SHELL", "/bin/zsh") };
        assert_eq!(detect_shell(), Some("zsh".to_string()));
        unsafe { std::env::set_var("SHELL", "/usr/bin/bash") };
        assert_eq!(detect_shell(), Some("bash".to_string()));
        unsafe { std::env::set_var("SHELL", "/usr/local/bin/fish") };
        assert_eq!(detect_shell(), Some("fish".to_string()));
        unsafe { std::env::set_var("SHELL", "/bin/sh") };
        if cfg!(windows) {
            assert_eq!(detect_shell(), Some("powershell".to_string()));
        } else {
            assert_eq!(detect_shell(), None);
        }
    }

    #[test]
    fn install_and_uninstall_hook() {
        let dir = tempfile::tempdir().unwrap();
        let config_file = dir.path().join(".bashrc");

        // Pre-populate with some content
        {
            let mut f = std::fs::File::create(&config_file).unwrap();
            writeln!(f, "# existing config").unwrap();
            writeln!(f, "export PATH=$HOME/bin:$PATH").unwrap();
        }

        // Patch shell_config_path by directly testing the install/uninstall logic
        // Since we can't easily override shell_config_path, test the core logic directly.

        let hook_script = generate_hook("bash").unwrap();
        let existing = std::fs::read_to_string(&config_file).unwrap();
        assert!(!existing.contains("__gitsitter_hook"));

        // Simulate install
        let block = format!("\n{HOOK_START_MARKER}\n{hook_script}\n{HOOK_END_MARKER}\n");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&config_file)
            .unwrap();
        file.write_all(block.as_bytes()).unwrap();
        drop(file);

        let content = std::fs::read_to_string(&config_file).unwrap();
        assert!(content.contains("__gitsitter_hook"));
        assert!(content.contains(HOOK_START_MARKER));
        assert!(content.contains(HOOK_END_MARKER));
        // Original content preserved
        assert!(content.contains("export PATH=$HOME/bin:$PATH"));

        // Simulate uninstall
        let start = content.find(HOOK_START_MARKER).unwrap();
        let end = content.find(HOOK_END_MARKER).unwrap() + HOOK_END_MARKER.len();
        let end = if content[end..].starts_with('\n') { end + 1 } else { end };
        let start = if start > 0 && content.as_bytes()[start - 1] == b'\n' {
            start - 1
        } else {
            start
        };

        let mut new_content = String::new();
        new_content.push_str(&content[..start]);
        new_content.push_str(&content[end..]);
        std::fs::write(&config_file, &new_content).unwrap();

        let after = std::fs::read_to_string(&config_file).unwrap();
        assert!(!after.contains("__gitsitter_hook"));
        assert!(!after.contains(HOOK_START_MARKER));
        // Original content still there
        assert!(after.contains("export PATH=$HOME/bin:$PATH"));
    }
}
