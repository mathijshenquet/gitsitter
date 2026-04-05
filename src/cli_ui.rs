//! Shared CLI display helpers.
//!
//! Provides consistent formatting for repo headers, mode indicators,
//! daemon warnings, and change hints across all CLI commands.

use crossterm::style::Stylize;

/// Display settings extracted from config.
#[derive(Debug, Clone, Copy)]
pub struct DisplayOpts {
    pub emoji: bool,
    pub colors: bool,
}

/// Format the repo header: `📦 ~/path/.git` or just `~/path/.git`.
pub fn repo_header(path: &str, opts: DisplayOpts) -> String {
    let path_str = if opts.colors {
        format!("{}", path.blue())
    } else {
        path.to_string()
    };
    if opts.emoji {
        format!("\u{1F4E6} {}", path_str)
    } else {
        path_str
    }
}

/// Format the sync mode summary line.
pub fn mode_line(opts: DisplayOpts) -> String {
    let label = "Sync: auto (fetch + pull always, push if owner)";
    if opts.colors {
        format!("{}", label.blue())
    } else {
        label.to_string()
    }
}

/// Format the daemon warning line.
pub fn daemon_warning(opts: DisplayOpts) -> String {
    let icon = if opts.emoji { "\u{26A0}" } else { "!" };
    let msg = format!("{} Daemon is not running \u{2014} start with: gitsitter daemon start", icon);
    if opts.colors {
        format!("{}", msg.yellow())
    } else {
        msg
    }
}

/// Format the change hint line.
pub fn change_hint() -> String {
    "Manage with: gitsitter enable/disable".to_string()
}

/// Format a success prefix icon.
pub fn success_icon(opts: DisplayOpts) -> &'static str {
    if opts.emoji { "\u{2713}" } else { "\u{2713}" }
}

/// Format a celebration prefix (for register).
pub fn celebrate_icon(opts: DisplayOpts) -> &'static str {
    if opts.emoji { "\u{1F389}" } else { "+" }
}

/// Format a pause icon (for disable).
pub fn pause_icon(opts: DisplayOpts) -> &'static str {
    if opts.emoji { "\u{23F8}" } else { "-" }
}

/// Format a warning icon.
pub fn warning_icon(opts: DisplayOpts) -> &'static str {
    if opts.emoji { "\u{26A0}\u{FE0F}" } else { "!" }
}

/// Status icon for a branch sync status string.
pub fn branch_status_icon(status: &str, opts: DisplayOpts) -> &'static str {
    if opts.emoji {
        match status {
            "synced" | "up_to_date" => "\u{2705}",
            "local_ahead" => "\u{2B06}\u{FE0F}",
            "fast_forward" | "remote_ahead" => "\u{2B07}\u{FE0F}",
            "diverged" => "\u{26A0}\u{FE0F}",
            "error" => "\u{274C}",
            _ => "\u{2753}",
        }
    } else {
        match status {
            "synced" | "up_to_date" => "synced",
            "local_ahead" => "local ahead",
            "fast_forward" | "remote_ahead" => "remote ahead",
            "diverged" => "diverged",
            "error" => "error",
            _ => "unknown",
        }
    }
}

/// Human-readable label for a branch sync status string.
pub fn branch_status_label(status: &str) -> &'static str {
    match status {
        "synced" | "up_to_date" => "synced",
        "local_ahead" => "local ahead",
        "fast_forward" | "remote_ahead" => "remote ahead",
        "diverged" => "diverged, ff not possible",
        "error" => "error",
        _ => "unknown",
    }
}

/// Format a colored branch status label.
pub fn branch_status_styled(status: &str, opts: DisplayOpts) -> String {
    let label = branch_status_label(status);
    if !opts.colors {
        return label.to_string();
    }
    match status {
        "synced" | "up_to_date" => format!("{}", label.green()),
        "local_ahead" => format!("{}", label.blue()),
        "fast_forward" | "remote_ahead" => format!("{}", label.yellow()),
        "diverged" => format!("{}", label.yellow()),
        "error" => format!("{}", label.red()),
        _ => label.to_string(),
    }
}

/// Print the standard repo info block used by register and enable.
/// Includes optional daemon warning and change hint.
pub fn print_repo_info_block(daemon_running: bool, opts: DisplayOpts) {
    if !daemon_running {
        println!("   {}", daemon_warning(opts));
    }
    println!();
    println!("   {}", mode_line(opts));
    println!();
    println!("   {}", change_hint());
}
