//! Typed branch sync model.
//!
//! Three concerns are kept deliberately separate:
//!
//! - [`BranchState`] — the *observable* state of a branch (what **is**). This is
//!   the single source of truth behind the status string, shell prompt, status
//!   table, and `SyncEvent`s. It replaces the old stringly-typed `sync_status`.
//! - [`SyncAction`] — what the executor should *do* (what **to do**). Only real
//!   executor instructions, plus `Hold` for the cases where gitsitter won't act.
//! - [`ActionError`] — why a write action failed, recorded *separately* from the
//!   branch's observable state.
//!
//! [`decide_branch_action`] is the pure policy function mapping observed inputs
//! to a [`SyncAction`]; the daemon executor turns that action into a final
//! [`BranchState`].

use serde::{Deserialize, Serialize};

use crate::git_ops::{HistoryRewrite, MergeAnalysis};

/// Observable state of a tracked branch — *what is*, not what to do about it.
///
/// Serializes to a stable status string (see [`BranchState::status_str`]) used
/// across the transport boundary, shell prompt, and status display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BranchState {
    /// Up to date with the remote (already synced, fast-forwarded, or pushed).
    Synced,
    /// Local commits on a branch the user doesn't own — held, not pushed.
    LocalAheadNotOwned,
    /// Fast-forward available but the worktree is dirty — held.
    DirtyWorktree,
    /// Upstream tracking ref was deleted.
    UpstreamGone,
    /// Diverged, and the remote tip looks like someone else's work — flagged.
    DivergedNotOwned,
    /// Diverged on an owned branch — held for manual resolution.
    Diverged,
    /// Local history was rewritten; remote has not advanced past the old tip.
    HistoryRewritten,
    /// Local history was rewritten and the remote advanced past the old tip.
    HistoryRewrittenRemoteAdvanced,
    /// Worktree is mid-merge/rebase with conflicts.
    MergeConflict,
    /// A write action failed; the error is the reason, recorded apart from state.
    Failed(ActionError),
}

/// Reason a write action (push / fast-forward) failed. Recorded alongside the
/// branch's [`BranchState`] rather than conflated into it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionError {
    PushRejected,
    AuthFailed,
    NetworkError,
    HookTimeout,
    /// Catch-all for fast-forward / update-ref / transport-level failures.
    Other,
}

impl ActionError {
    pub fn status_str(&self) -> &'static str {
        match self {
            ActionError::PushRejected => "push_rejected",
            ActionError::AuthFailed => "auth_failed",
            ActionError::NetworkError => "network_error",
            ActionError::HookTimeout => "push_blocked_hook_timeout",
            ActionError::Other => "error",
        }
    }
}

impl BranchState {
    /// Stable status string for transport, prompt, and status display.
    pub fn status_str(&self) -> &'static str {
        match self {
            BranchState::Synced => "synced",
            BranchState::LocalAheadNotOwned => "local_ahead",
            BranchState::DirtyWorktree => "pending_dirty",
            BranchState::UpstreamGone => "upstream_gone",
            BranchState::DivergedNotOwned => "diverged",
            BranchState::Diverged => "diverged_yours",
            BranchState::HistoryRewritten => "history_rewritten_remote_unchanged",
            BranchState::HistoryRewrittenRemoteAdvanced => "history_rewritten_remote_advanced",
            BranchState::MergeConflict => "merge_conflict",
            BranchState::Failed(e) => e.status_str(),
        }
    }

    /// Event detail line for a held state (the cases the executor records
    /// without performing a write).
    pub fn hold_detail(&self) -> &'static str {
        match self {
            BranchState::LocalAheadNotOwned => "local ahead, not owned — skipped",
            BranchState::DirtyWorktree => "skipped — worktree dirty",
            BranchState::UpstreamGone => "upstream ref deleted",
            BranchState::DivergedNotOwned => "diverged, not owned — flagged",
            BranchState::Diverged => "diverged — holding for manual resolution",
            BranchState::HistoryRewritten => "history rewrite detected, remote unchanged — holding",
            BranchState::HistoryRewrittenRemoteAdvanced => {
                "history rewrite detected, remote advanced — holding"
            }
            BranchState::MergeConflict => "merge conflict — resolve manually",
            BranchState::Synced => "up to date",
            BranchState::Failed(_) => "action failed",
        }
    }

    /// Stored error/explanation message for a held state, if any.
    pub fn hold_message(&self) -> Option<&'static str> {
        match self {
            BranchState::LocalAheadNotOwned => {
                Some("local commits on non-owned branch — push manually or create a new branch")
            }
            BranchState::DirtyWorktree => Some("dirty worktree — commit or stash to sync"),
            BranchState::UpstreamGone => Some("upstream ref deleted"),
            BranchState::DivergedNotOwned => Some("diverged (last remote commit by someone else)"),
            BranchState::Diverged => Some("diverged — resolve manually (merge or rebase)"),
            BranchState::HistoryRewritten => {
                Some("local history rewritten — push --force-with-lease when ready")
            }
            BranchState::HistoryRewrittenRemoteAdvanced => Some(
                "local history rewritten and remote advanced — force-push would discard remote \
                 commits; consider backing up and resetting to remote",
            ),
            BranchState::MergeConflict => Some("merge conflict — resolve manually"),
            BranchState::Synced | BranchState::Failed(_) => None,
        }
    }
}

/// What the executor should do with a single branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    /// Already in sync — record `Synced`, nothing to write.
    Noop,
    /// Fast-forward the checked-out worktree via merge.
    FastForwardMerge,
    /// Fast-forward via `update-ref` (branch not checked out).
    FastForwardRef,
    /// Push local commits to the remote.
    Push,
    /// Skip the push silently — a push backoff is active.
    SkipPushBackoff,
    /// Don't act; record the given observable state and hold.
    Hold(BranchState),
}

impl SyncAction {
    /// Stable name for the `sync_action` field of a [`crate::transport::SyncEvent`].
    pub fn label(&self) -> &'static str {
        match self {
            SyncAction::Noop => "UpToDate",
            SyncAction::FastForwardMerge => "FastForwardMerge",
            SyncAction::FastForwardRef => "FastForwardRef",
            SyncAction::Push => "Push",
            SyncAction::SkipPushBackoff => "SkipPushBackoff",
            SyncAction::Hold(state) => match state {
                BranchState::UpstreamGone => "UpstreamGone",
                BranchState::DirtyWorktree => "SkipDirty",
                BranchState::LocalAheadNotOwned => "LocalAheadNotOwned",
                BranchState::DivergedNotOwned => "DivergedNotOwned",
                _ => "Diverged",
            },
        }
    }
}

/// Observed inputs to the pure decision function. All the state needed to
/// decide what to do with a single branch, with no side effects.
#[derive(Debug, Clone)]
pub struct BranchInputs {
    pub analysis: MergeAnalysis,
    pub is_checked_out: bool,
    pub is_worktree_dirty: bool,
    pub is_owner: bool,
    /// `LocalAhead` with a merge commit incorporating remote changes.
    pub is_merge_of_remote: bool,
    pub push_backoff_active: bool,
    /// History-rewrite classification — only meaningful for owned divergence.
    pub rewrite: HistoryRewrite,
}

/// Decide what sync action to take for a branch, purely from observed state.
pub fn decide_branch_action(input: &BranchInputs) -> SyncAction {
    match input.analysis {
        MergeAnalysis::UpstreamGone => SyncAction::Hold(BranchState::UpstreamGone),
        MergeAnalysis::UpToDate => SyncAction::Noop,

        MergeAnalysis::FastForward => {
            if input.is_checked_out {
                if input.is_worktree_dirty {
                    SyncAction::Hold(BranchState::DirtyWorktree)
                } else {
                    SyncAction::FastForwardMerge
                }
            } else {
                SyncAction::FastForwardRef
            }
        }

        MergeAnalysis::LocalAhead => {
            if !input.is_owner && !input.is_merge_of_remote {
                SyncAction::Hold(BranchState::LocalAheadNotOwned)
            } else if input.push_backoff_active {
                SyncAction::SkipPushBackoff
            } else {
                SyncAction::Push
            }
        }

        MergeAnalysis::Diverged => {
            if !input.is_owner {
                return SyncAction::Hold(BranchState::DivergedNotOwned);
            }
            match input.rewrite {
                HistoryRewrite::RemoteUnchanged => SyncAction::Hold(BranchState::HistoryRewritten),
                HistoryRewrite::RemoteAdvanced => {
                    SyncAction::Hold(BranchState::HistoryRewrittenRemoteAdvanced)
                }
                HistoryRewrite::None => SyncAction::Hold(BranchState::Diverged),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> BranchInputs {
        BranchInputs {
            analysis: MergeAnalysis::UpToDate,
            is_checked_out: false,
            is_worktree_dirty: false,
            is_owner: false,
            is_merge_of_remote: false,
            push_backoff_active: false,
            rewrite: HistoryRewrite::None,
        }
    }

    #[test]
    fn up_to_date_is_noop() {
        assert_eq!(decide_branch_action(&base()), SyncAction::Noop);
    }

    #[test]
    fn upstream_gone_holds() {
        let input = BranchInputs {
            analysis: MergeAnalysis::UpstreamGone,
            ..base()
        };
        assert_eq!(
            decide_branch_action(&input),
            SyncAction::Hold(BranchState::UpstreamGone)
        );
    }

    #[test]
    fn ff_not_checked_out_uses_update_ref() {
        let input = BranchInputs {
            analysis: MergeAnalysis::FastForward,
            is_checked_out: false,
            ..base()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::FastForwardRef);
    }

    #[test]
    fn ff_checked_out_clean_merges() {
        let input = BranchInputs {
            analysis: MergeAnalysis::FastForward,
            is_checked_out: true,
            ..base()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::FastForwardMerge);
    }

    #[test]
    fn ff_checked_out_dirty_holds() {
        let input = BranchInputs {
            analysis: MergeAnalysis::FastForward,
            is_checked_out: true,
            is_worktree_dirty: true,
            ..base()
        };
        assert_eq!(
            decide_branch_action(&input),
            SyncAction::Hold(BranchState::DirtyWorktree)
        );
    }

    #[test]
    fn local_ahead_not_owned_holds() {
        let input = BranchInputs {
            analysis: MergeAnalysis::LocalAhead,
            ..base()
        };
        assert_eq!(
            decide_branch_action(&input),
            SyncAction::Hold(BranchState::LocalAheadNotOwned)
        );
    }

    #[test]
    fn local_ahead_owned_pushes() {
        let input = BranchInputs {
            analysis: MergeAnalysis::LocalAhead,
            is_owner: true,
            ..base()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::Push);
    }

    #[test]
    fn local_ahead_merge_of_remote_pushes() {
        let input = BranchInputs {
            analysis: MergeAnalysis::LocalAhead,
            is_merge_of_remote: true,
            ..base()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::Push);
    }

    #[test]
    fn local_ahead_backoff_skips() {
        let input = BranchInputs {
            analysis: MergeAnalysis::LocalAhead,
            is_owner: true,
            push_backoff_active: true,
            ..base()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::SkipPushBackoff);
    }

    #[test]
    fn diverged_not_owned_flags() {
        let input = BranchInputs {
            analysis: MergeAnalysis::Diverged,
            is_owner: false,
            ..base()
        };
        assert_eq!(
            decide_branch_action(&input),
            SyncAction::Hold(BranchState::DivergedNotOwned)
        );
    }

    #[test]
    fn diverged_owned_holds() {
        let input = BranchInputs {
            analysis: MergeAnalysis::Diverged,
            is_owner: true,
            ..base()
        };
        assert_eq!(
            decide_branch_action(&input),
            SyncAction::Hold(BranchState::Diverged)
        );
    }

    #[test]
    fn diverged_owned_rewrite_remote_unchanged() {
        let input = BranchInputs {
            analysis: MergeAnalysis::Diverged,
            is_owner: true,
            rewrite: HistoryRewrite::RemoteUnchanged,
            ..base()
        };
        assert_eq!(
            decide_branch_action(&input),
            SyncAction::Hold(BranchState::HistoryRewritten)
        );
    }

    #[test]
    fn diverged_owned_rewrite_remote_advanced() {
        let input = BranchInputs {
            analysis: MergeAnalysis::Diverged,
            is_owner: true,
            rewrite: HistoryRewrite::RemoteAdvanced,
            ..base()
        };
        assert_eq!(
            decide_branch_action(&input),
            SyncAction::Hold(BranchState::HistoryRewrittenRemoteAdvanced)
        );
    }

    #[test]
    fn status_strings_are_stable() {
        assert_eq!(BranchState::Synced.status_str(), "synced");
        assert_eq!(BranchState::Diverged.status_str(), "diverged_yours");
        assert_eq!(BranchState::DivergedNotOwned.status_str(), "diverged");
        assert_eq!(
            BranchState::HistoryRewritten.status_str(),
            "history_rewritten_remote_unchanged"
        );
        assert_eq!(BranchState::DirtyWorktree.status_str(), "pending_dirty");
        assert_eq!(
            BranchState::Failed(ActionError::PushRejected).status_str(),
            "push_rejected"
        );
    }
}
