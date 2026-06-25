//! Shared CLI display helpers.
//!
//! Provides consistent formatting for repo headers, status icons,
//! and daemon warnings across all CLI commands.

use crossterm::style::Stylize;

use crate::sync::BranchState;

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

/// Status icon for a branch state.
pub fn branch_status_icon(status: &BranchState, opts: DisplayOpts) -> &'static str {
    if opts.emoji {
        match status {
            BranchState::Synced => "\u{2705}",
            BranchState::LocalAheadNotOwned => "\u{2B06}\u{FE0F}",
            BranchState::DivergedNotOwned | BranchState::Diverged => "\u{26A0}\u{FE0F}",
            BranchState::HistoryRewritten | BranchState::HistoryRewrittenRemoteAdvanced => {
                "\u{270D}\u{FE0F}"
            }
            BranchState::DirtyWorktree => "\u{270F}\u{FE0F}",
            BranchState::MergeConflict => "\u{1F527}",
            BranchState::UpstreamGone => "\u{1F6AB}",
            BranchState::Failed(_) => "\u{274C}",
        }
    } else {
        match status {
            BranchState::Synced => "synced",
            BranchState::LocalAheadNotOwned => "local ahead",
            BranchState::DivergedNotOwned | BranchState::Diverged => "diverged",
            BranchState::HistoryRewritten | BranchState::HistoryRewrittenRemoteAdvanced => {
                "rewritten"
            }
            BranchState::DirtyWorktree => "dirty",
            BranchState::MergeConflict => "conflict",
            BranchState::UpstreamGone => "upstream gone",
            BranchState::Failed(_) => "error",
        }
    }
}

/// Human-readable label for a branch state.
pub fn branch_status_label(status: &BranchState) -> &'static str {
    match status {
        BranchState::Synced => "synced",
        BranchState::LocalAheadNotOwned => "local ahead",
        BranchState::DivergedNotOwned => "diverged (someone else)",
        BranchState::Diverged => "diverged (resolve manually)",
        BranchState::HistoryRewritten => "history rewritten (force-push ready)",
        BranchState::HistoryRewrittenRemoteAdvanced => "history rewritten (remote advanced)",
        BranchState::DirtyWorktree => "dirty worktree",
        BranchState::MergeConflict => "merge conflict",
        BranchState::UpstreamGone => "upstream gone",
        BranchState::Failed(_) => "error",
    }
}

/// Format a colored branch status label.
pub fn branch_status_styled(status: &BranchState, opts: DisplayOpts) -> String {
    let label = branch_status_label(status);
    if !opts.colors {
        return label.to_string();
    }
    match status {
        BranchState::Synced => format!("{}", label.green()),
        BranchState::LocalAheadNotOwned => format!("{}", label.blue()),
        BranchState::DivergedNotOwned | BranchState::Diverged => format!("{}", label.yellow()),
        BranchState::HistoryRewritten => format!("{}", label.yellow()),
        BranchState::DirtyWorktree => format!("{}", label.yellow()),
        BranchState::HistoryRewrittenRemoteAdvanced
        | BranchState::MergeConflict
        | BranchState::UpstreamGone
        | BranchState::Failed(_) => format!("{}", label.red()),
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
