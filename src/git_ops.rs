//! Git operations (git2 + git CLI)
//!
//! Hybrid interface: git2 for fast in-process read-only operations,
//! git CLI for network and write operations.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use tokio::process::Command as TokioCommand;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RepoBranch {
    pub name: String,
    pub local_oid: String,
    pub upstream_name: Option<String>,
    pub remote_oid: Option<String>,
    pub is_head: bool,
    /// The remote configured for this branch (`branch.<name>.remote`), defaults to "origin".
    pub remote: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MergeAnalysis {
    UpToDate,
    FastForward,
    LocalAhead,
    Diverged,
    UpstreamGone,
}

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: String,
    pub head_branch: Option<String>,
    pub is_clean: bool,
}

#[derive(Debug)]
pub enum PushResult {
    Success,
    Rejected(String),
    AuthFailed(String),
    NetworkError(String),
    HookTimeout,
}

// ---------------------------------------------------------------------------
// git2 functions
// ---------------------------------------------------------------------------

/// Discover the git repo from a path (handles worktrees).
/// Returns the canonicalized path to the common git dir (repo_id).
pub fn discover_repo_id(path: &Path) -> Result<PathBuf> {
    let mut current = if path.is_file() {
        path.parent().unwrap_or(path).to_path_buf()
    } else {
        path.to_path_buf()
    };

    let repo = loop {
        match git2::Repository::open(&current) {
            Ok(repo) => break repo,
            Err(_) => {
                let Some(parent) = current.parent() else {
                    bail!("failed to discover git repo at {}", path.display());
                };
                current = parent.to_path_buf();
            }
        }
    };

    let common = repo.commondir().to_path_buf();
    match common.canonicalize() {
        Ok(canonical) => Ok(canonical),
        Err(_) if cfg!(windows) => Ok(common),
        Err(err) => Err(err)
            .with_context(|| format!("failed to canonicalize {}", common.display())),
    }
}

/// Get the display path (working tree path) for a repo.
pub fn get_display_path(repo_id: &Path) -> Result<PathBuf> {
    let repo = git2::Repository::open(repo_id)
        .with_context(|| format!("failed to open repo at {}", repo_id.display()))?;
    if let Some(workdir) = repo.workdir() {
        Ok(workdir.to_path_buf())
    } else {
        // Bare repo or similar — fall back to the git dir's parent.
        Ok(repo_id
            .parent()
            .unwrap_or(repo_id)
            .to_path_buf())
    }
}

/// Get the primary remote URL for a repo (usually "origin").
pub fn get_remote_url(repo_id: &Path) -> Result<Option<String>> {
    let repo = git2::Repository::open(repo_id)
        .with_context(|| format!("failed to open repo at {}", repo_id.display()))?;
    let remote = match repo.find_remote("origin") {
        Ok(r) => r,
        Err(_) => {
            // No "origin" remote — try to find any remote.
            let remotes = repo.remotes()?;
            if remotes.is_empty() {
                return Ok(None);
            }
            let name = remotes.get(0).unwrap_or("origin");
            match repo.find_remote(name) {
                Ok(r) => r,
                Err(_) => return Ok(None),
            }
        }
    };
    Ok(remote.url().map(|s| s.to_string()))
}

/// Get a map of remote_name -> URL for all remotes in a repo.
pub fn get_all_remote_urls(repo_id: &Path) -> Result<HashMap<String, String>> {
    let repo = git2::Repository::open(repo_id)
        .with_context(|| format!("failed to open repo at {}", repo_id.display()))?;
    let remote_names = repo.remotes()?;
    let mut map = HashMap::new();
    for i in 0..remote_names.len() {
        if let Some(name) = remote_names.get(i) {
            if let Ok(remote) = repo.find_remote(name) {
                if let Some(url) = remote.url() {
                    map.insert(name.to_string(), url.to_string());
                }
            }
        }
    }
    Ok(map)
}

/// List all local tracking branches with their upstream info.
pub fn list_branches(repo_id: &Path) -> Result<Vec<RepoBranch>> {
    let repo = git2::Repository::open(repo_id)
        .with_context(|| format!("failed to open repo at {}", repo_id.display()))?;

    let head_ref = repo.head().ok();
    let head_name = head_ref
        .as_ref()
        .and_then(|r| r.shorthand().map(|s| s.to_string()));

    let mut branches = Vec::new();
    for entry in repo.branches(Some(git2::BranchType::Local))? {
        let (branch, _btype) = entry?;
        let name = match branch.name()? {
            Some(n) => n.to_string(),
            None => continue,
        };
        let reference = branch.get();
        let local_oid = match reference.target() {
            Some(oid) => oid.to_string(),
            None => continue,
        };

        let is_head = head_name.as_deref() == Some(name.as_str());

        let (upstream_name, remote_oid) = match branch.upstream() {
            Ok(upstream) => {
                let uname = upstream
                    .name()?
                    .map(|s| s.to_string());
                let uoid = upstream
                    .get()
                    .target()
                    .map(|oid| oid.to_string());
                (uname, uoid)
            }
            Err(_) => (None, None),
        };

        // Read branch.<name>.remote from git config, default to "origin".
        let remote = repo
            .config()
            .ok()
            .and_then(|cfg| {
                cfg.get_string(&format!("branch.{}.remote", name)).ok()
            })
            .unwrap_or_else(|| "origin".to_string());

        branches.push(RepoBranch {
            name,
            local_oid,
            upstream_name,
            remote_oid,
            is_head,
            remote,
        });
    }
    Ok(branches)
}

/// Analyze merge status between a local branch and its upstream.
pub fn analyze_merge(repo_id: &Path, branch_name: &str) -> Result<MergeAnalysis> {
    let repo = git2::Repository::open(repo_id)
        .with_context(|| format!("failed to open repo at {}", repo_id.display()))?;

    let branch = repo
        .find_branch(branch_name, git2::BranchType::Local)
        .with_context(|| format!("branch '{}' not found", branch_name))?;

    let upstream = match branch.upstream() {
        Ok(u) => u,
        Err(_) => return Ok(MergeAnalysis::UpstreamGone),
    };

    let local_oid = branch
        .get()
        .target()
        .with_context(|| format!("branch '{}' has no target", branch_name))?;
    let remote_oid = match upstream.get().target() {
        Some(oid) => oid,
        None => return Ok(MergeAnalysis::UpstreamGone),
    };

    if local_oid == remote_oid {
        return Ok(MergeAnalysis::UpToDate);
    }

    let local_is_ancestor = repo.graph_descendant_of(remote_oid, local_oid)?;
    let remote_is_ancestor = repo.graph_descendant_of(local_oid, remote_oid)?;

    match (local_is_ancestor, remote_is_ancestor) {
        // remote_oid descends from local_oid → remote is ahead, ff possible
        (true, false) => Ok(MergeAnalysis::FastForward),
        // local_oid descends from remote_oid → local is ahead
        (false, true) => Ok(MergeAnalysis::LocalAhead),
        // both descend from each other should not happen, but treat as up-to-date
        (true, true) => Ok(MergeAnalysis::UpToDate),
        // neither descends from the other → diverged
        (false, false) => Ok(MergeAnalysis::Diverged),
    }
}

/// List all worktrees (main + linked) for a repo.
pub fn list_worktrees(repo_id: &Path) -> Result<Vec<WorktreeInfo>> {
    let repo = git2::Repository::open(repo_id)
        .with_context(|| format!("failed to open repo at {}", repo_id.display()))?;

    let mut result = Vec::new();

    // Main worktree
    if let Some(workdir) = repo.workdir() {
        let head_branch = repo
            .head()
            .ok()
            .and_then(|r| {
                if r.is_branch() {
                    r.shorthand().map(|s| s.to_string())
                } else {
                    None
                }
            });
        let is_clean = !is_worktree_dirty(workdir).unwrap_or(true);
        result.push(WorktreeInfo {
            path: workdir.to_string_lossy().to_string(),
            head_branch,
            is_clean,
        });
    }

    // Linked worktrees
    let wt_names = repo.worktrees()?;
    for i in 0..wt_names.len() {
        let name = match wt_names.get(i) {
            Some(n) => n,
            None => continue,
        };
        let wt = match repo.find_worktree(name) {
            Ok(wt) => wt,
            Err(_) => continue,
        };
        let wt_path = wt.path();
        let wt_repo = match git2::Repository::open(wt_path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let head_branch = wt_repo
            .head()
            .ok()
            .and_then(|r| {
                if r.is_branch() {
                    r.shorthand().map(|s| s.to_string())
                } else {
                    None
                }
            });
        let is_clean = !is_worktree_dirty(wt_path).unwrap_or(true);
        result.push(WorktreeInfo {
            path: wt_path.to_string_lossy().to_string(),
            head_branch,
            is_clean,
        });
    }

    Ok(result)
}

/// Check if a repo path exists and is a valid git repo.
pub fn is_valid_repo(path: &Path) -> bool {
    git2::Repository::open(path).is_ok()
}

/// Check if any git operation is in progress (rebase, merge, cherry-pick, bisect).
pub fn is_operation_in_progress(repo_id: &Path) -> bool {
    let checks = [
        "index.lock",
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "BISECT_LOG",
    ];
    for name in &checks {
        let p = repo_id.join(name);
        if p.exists() {
            return true;
        }
    }
    false
}

/// Check if a worktree has any staged or unstaged changes to tracked files.
/// Untracked files do NOT count as dirty.
pub fn is_worktree_dirty(worktree_path: &Path) -> Result<bool> {
    let repo = git2::Repository::open(worktree_path)
        .with_context(|| format!("failed to open repo at {}", worktree_path.display()))?;

    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(false)
        .include_ignored(false)
        .include_unmodified(false)
        .exclude_submodules(true);

    let statuses = repo
        .statuses(Some(&mut opts))
        .context("failed to get worktree status")?;

    Ok(!statuses.is_empty())
}

/// Build a map of branch_name -> worktree_path for all checked-out branches.
pub fn branch_occupancy(repo_id: &Path) -> Result<HashMap<String, String>> {
    let worktrees = list_worktrees(repo_id)?;
    let mut map = HashMap::new();
    for wt in worktrees {
        if let Some(branch) = wt.head_branch {
            map.insert(branch, wt.path);
        }
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// git CLI functions
// ---------------------------------------------------------------------------

/// Build a `tokio::process::Command` with standard daemon-safe settings.
fn git_command(repo_path: &Path, git_path: Option<&str>) -> TokioCommand {
    let bin = git_path.unwrap_or("git");
    let mut cmd = TokioCommand::new(bin);
    cmd.arg("-C")
        .arg(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Run a git fetch for the given remote.
pub async fn git_fetch(
    repo_path: &Path,
    remote: &str,
    git_path: Option<&str>,
    timeout_secs: u64,
) -> Result<()> {
    let mut cmd = git_command(repo_path, git_path);
    cmd.arg("fetch").arg(remote);

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        cmd.output(),
    )
    .await
    .with_context(|| format!("git fetch timed out after {}s", timeout_secs))?
    .context("failed to spawn git fetch")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git fetch failed: {}", stderr.trim());
    }
    Ok(())
}

/// Stash uncommitted changes.
/// Returns Ok(true) if something was stashed, Ok(false) if nothing to stash.
pub async fn git_stash(
    worktree_path: &Path,
    git_path: Option<&str>,
    timeout_secs: u64,
) -> Result<bool> {
    let mut cmd = git_command(worktree_path, git_path);
    cmd.arg("stash").arg("push").arg("-m").arg("gitsitter: auto-stash for sync");

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        cmd.output(),
    )
    .await
    .with_context(|| format!("git stash timed out after {}s", timeout_secs))?
    .context("failed to spawn git stash")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git stash failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // "No local changes to save" means nothing was stashed
    Ok(!stdout.contains("No local changes"))
}

/// Pop the most recent stash entry.
/// Returns Ok(true) if pop succeeded cleanly, Ok(false) if there were conflicts.
pub async fn git_stash_pop(
    worktree_path: &Path,
    git_path: Option<&str>,
    timeout_secs: u64,
) -> Result<bool> {
    let mut cmd = git_command(worktree_path, git_path);
    cmd.arg("stash").arg("pop");

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        cmd.output(),
    )
    .await
    .with_context(|| format!("git stash pop timed out after {}s", timeout_secs))?
    .context("failed to spawn git stash pop")?;

    Ok(output.status.success())
}

/// Fast-forward merge the current branch.
pub async fn git_ff_merge(
    worktree_path: &Path,
    upstream_ref: &str,
    git_path: Option<&str>,
    timeout_secs: u64,
) -> Result<()> {
    let mut cmd = git_command(worktree_path, git_path);
    cmd.arg("merge").arg("--ff-only").arg(upstream_ref);

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        cmd.output(),
    )
    .await
    .with_context(|| format!("git merge --ff-only timed out after {}s", timeout_secs))?
    .context("failed to spawn git merge")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git merge --ff-only failed: {}", stderr.trim());
    }
    Ok(())
}

/// Push a branch to its upstream remote.
pub async fn git_push(
    repo_path: &Path,
    remote: &str,
    branch: &str,
    git_path: Option<&str>,
    timeout_secs: u64,
) -> Result<PushResult> {
    let mut cmd = git_command(repo_path, git_path);
    cmd.arg("push").arg(remote).arg(branch);

    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        cmd.output(),
    )
    .await
    {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(e).context("failed to spawn git push"),
        Err(_) => return Ok(PushResult::HookTimeout),
    };

    if output.status.success() {
        return Ok(PushResult::Success);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stderr_lower = stderr.to_lowercase();

    if stderr_lower.contains("rejected")
        || stderr_lower.contains("protected branch")
        || stderr_lower.contains("[remote rejected]")
        || stderr_lower.contains("non-fast-forward")
    {
        Ok(PushResult::Rejected(stderr))
    } else if stderr_lower.contains("authentication")
        || stderr_lower.contains("permission denied")
        || stderr_lower.contains("could not read from remote")
        || stderr_lower.contains("fatal: could not read username")
        || stderr_lower.contains("invalid credentials")
    {
        Ok(PushResult::AuthFailed(stderr))
    } else if stderr_lower.contains("could not resolve host")
        || stderr_lower.contains("network is unreachable")
        || stderr_lower.contains("connection refused")
        || stderr_lower.contains("connection timed out")
        || stderr_lower.contains("no route to host")
    {
        Ok(PushResult::NetworkError(stderr))
    } else {
        // Unknown push failure — treat as rejected with full stderr.
        Ok(PushResult::Rejected(stderr))
    }
}

/// Get the current user's email from git config.
pub fn get_current_user_email(repo_id: &Path) -> Result<Option<String>> {
    let repo = git2::Repository::open(repo_id)
        .with_context(|| format!("failed to open repo at {}", repo_id.display()))?;
    let config = repo.config().context("failed to read git config")?;
    match config.get_string("user.email") {
        Ok(email) => Ok(Some(email)),
        Err(_) => Ok(None),
    }
}

/// Check if the local tip is a merge commit that has the upstream tip as a
/// direct parent. In that case, pushing is safe even if we don't "own" the
/// remote branch, because the merge incorporates all remote changes.
pub fn is_local_merge_of_remote(repo_id: &Path, branch_name: &str) -> Result<bool> {
    let repo = git2::Repository::open(repo_id)
        .with_context(|| format!("failed to open repo at {}", repo_id.display()))?;

    let branch = repo
        .find_branch(branch_name, git2::BranchType::Local)
        .with_context(|| format!("branch '{}' not found", branch_name))?;

    let local_oid = match branch.get().target() {
        Some(oid) => oid,
        None => return Ok(false),
    };

    let upstream = match branch.upstream() {
        Ok(u) => u,
        Err(_) => return Ok(false),
    };

    let upstream_oid = match upstream.get().target() {
        Some(oid) => oid,
        None => return Ok(false),
    };

    let local_commit = repo
        .find_commit(local_oid)
        .with_context(|| format!("failed to find local commit {}", local_oid))?;

    // Must be a merge commit (>1 parent) with the upstream tip as a direct parent
    if local_commit.parent_count() < 2 {
        return Ok(false);
    }

    for i in 0..local_commit.parent_count() {
        if local_commit.parent_id(i) == Ok(upstream_oid) {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Check if the current user "owns" a branch by comparing the tip commit
/// of the upstream ref to the current user's email (case-insensitive).
///
/// Returns true if the upstream tip was committed by the current user,
/// meaning auto-push is allowed.
pub fn is_branch_owned_by_user(repo_id: &Path, branch_name: &str) -> Result<bool> {
    let repo = git2::Repository::open(repo_id)
        .with_context(|| format!("failed to open repo at {}", repo_id.display()))?;

    let user_email = match repo.config().ok().and_then(|c| c.get_string("user.email").ok()) {
        Some(e) => e,
        None => return Ok(false),
    };

    let committer_email = get_upstream_committer_email(&repo, branch_name)?;
    let committer_email = match committer_email {
        Some(e) => e,
        None => return Ok(false),
    };

    Ok(committer_email.eq_ignore_ascii_case(&user_email))
}

/// Like `is_branch_owned_by_user` but with forge fallbacks:
/// 1. Local committer email match (same as above)
/// 2. Committer email matches any verified email of the forge user
/// 3. Authenticated user is creator/assignee of a PR for this branch
pub async fn is_branch_owned_by_user_with_forge(
    repo_id: &Path,
    branch_name: &str,
    remote_url: Option<&str>,
    forge_cache: &crate::forge::ForgeCache,
) -> Result<bool> {
    // 1. Local committer email match
    if is_branch_owned_by_user(repo_id, branch_name)? {
        return Ok(true);
    }

    let gh_repo = remote_url.and_then(crate::forge::parse_github_remote);

    // 2. Check committer email against forge emails
    if gh_repo.is_some() {
        let repo = git2::Repository::open(repo_id)?;
        if let Some(committer_email) = get_upstream_committer_email(&repo, branch_name)? {
            if forge_cache.email_matches_user(&committer_email).await {
                return Ok(true);
            }
        }
    }

    // 3. Check PR ownership
    if let Some(ref gh_repo) = gh_repo {
        if forge_cache.user_owns_pr(gh_repo, branch_name).await {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Get the committer email of the upstream tip for a branch.
fn get_upstream_committer_email(
    repo: &git2::Repository,
    branch_name: &str,
) -> Result<Option<String>> {
    let branch = repo
        .find_branch(branch_name, git2::BranchType::Local)
        .with_context(|| format!("branch '{}' not found", branch_name))?;

    let upstream = match branch.upstream() {
        Ok(u) => u,
        Err(_) => return Ok(None),
    };

    let upstream_oid = match upstream.get().target() {
        Some(oid) => oid,
        None => return Ok(None),
    };

    let commit = repo
        .find_commit(upstream_oid)
        .with_context(|| format!("failed to find upstream commit {}", upstream_oid))?;

    Ok(Some(
        commit.committer().email().unwrap_or("").to_string(),
    ))
}

/// Rebase the current branch onto its upstream.
/// Returns Ok(true) if rebase succeeded, Ok(false) if there were conflicts
/// (rebase is aborted automatically).
pub async fn git_rebase(
    worktree_path: &Path,
    upstream_ref: &str,
    git_path: Option<&str>,
    timeout_secs: u64,
) -> Result<bool> {
    let mut cmd = git_command(worktree_path, git_path);
    cmd.arg("rebase").arg(upstream_ref);

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        cmd.output(),
    )
    .await
    .with_context(|| format!("git rebase timed out after {}s", timeout_secs))?
    .context("failed to spawn git rebase")?;

    if output.status.success() {
        return Ok(true);
    }

    // Rebase failed (conflicts) — abort it so we don't leave a dirty state
    let mut abort = git_command(worktree_path, git_path);
    abort.arg("rebase").arg("--abort");
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        abort.output(),
    )
    .await;

    Ok(false)
}

/// Update a ref to a new OID, with expected-old-OID for safety.
pub async fn git_update_ref(
    repo_path: &Path,
    ref_name: &str,
    new_oid: &str,
    old_oid: &str,
    git_path: Option<&str>,
) -> Result<()> {
    let mut cmd = git_command(repo_path, git_path);
    cmd.arg("update-ref").arg(ref_name).arg(new_oid).arg(old_oid);

    let output = cmd.output().await.context("failed to spawn git update-ref")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git update-ref failed: {}", stderr.trim());
    }
    Ok(())
}
