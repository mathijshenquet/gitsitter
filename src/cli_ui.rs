//! Shared CLI display helpers.
//!
//! Provides consistent formatting for repo headers, status icons,
//! and daemon warnings across all CLI commands.

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

/// Format the daemon warning line.
pub fn daemon_warning(opts: DisplayOpts) -> String {
    let icon = if opts.emoji { "\u{26A0}" } else { "!" };
    let msg = format!(
        "{} Daemon is not running \u{2014} start with: gitsitter daemon start",
        icon
    );
    if opts.colors {
        format!("{}", msg.yellow())
    } else {
        msg
    }
}

/// Format a success prefix icon.
pub fn success_icon(_opts: DisplayOpts) -> &'static str {
    "\u{2713}"
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
            "diverged" | "diverged_yours" => "\u{26A0}\u{FE0F}",
            "history_rewritten_remote_unchanged" | "history_rewritten_remote_advanced" => {
                "\u{270D}\u{FE0F}"
            }
            "pending_dirty" => "\u{270F}\u{FE0F}",
            "merge_conflict" => "\u{1F527}",
            "error" => "\u{274C}",
            _ => "\u{2753}",
        }
    } else {
        match status {
            "synced" | "up_to_date" => "synced",
            "local_ahead" => "local ahead",
            "fast_forward" | "remote_ahead" => "remote ahead",
            "diverged" | "diverged_yours" => "diverged",
            "history_rewritten_remote_unchanged" => "rewritten",
            "history_rewritten_remote_advanced" => "rewritten",
            "pending_dirty" => "dirty",
            "merge_conflict" => "conflict",
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
        "diverged" => "diverged (someone else)",
        "diverged_yours" => "diverged (rebase needed)",
        "history_rewritten_remote_unchanged" => "history rewritten (force-push ready)",
        "history_rewritten_remote_advanced" => "history rewritten (remote advanced)",
        "pending_dirty" => "dirty worktree",
        "merge_conflict" => "merge conflict",
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
        "diverged" | "diverged_yours" => format!("{}", label.yellow()),
        "history_rewritten_remote_unchanged" => format!("{}", label.yellow()),
        "history_rewritten_remote_advanced" => format!("{}", label.red()),
        "pending_dirty" => format!("{}", label.yellow()),
        "merge_conflict" => format!("{}", label.red()),
        "error" => format!("{}", label.red()),
        _ => label.to_string(),
    }
}

/// Format a branch ↔ upstream sync pair.
pub fn sync_pair(branch: &str, upstream: &str) -> String {
    format!("{} \u{2194} {}", branch, upstream)
}

/// Print daemon warning if not running (used by register and enable).
pub fn print_daemon_warning(daemon_running: bool, opts: DisplayOpts) {
    if !daemon_running {
        println!("  {}", daemon_warning(opts));
    }
}
