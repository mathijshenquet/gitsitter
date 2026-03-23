//! Shared query/view layer for building status from DB + config + git2.
//!
//! Used by both the daemon (socket request handlers) and the CLI (fallback
//! when daemon is not running). Ensures identical output regardless of
//! whether the daemon is up.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::{InRepoConfig, UserConfig};
use crate::git_ops;
use crate::state::StateDb;
use crate::transport::{BranchStatusData, RepoStatusData, StatusData};

/// Load the in-repo `.gitsitter.toml` config if it exists.
///
/// `repo_id` is the common git dir (e.g. `/path/to/repo/.git`).
pub fn load_in_repo_config(repo_id: &Path) -> Result<Option<InRepoConfig>> {
    let display_path = git_ops::get_display_path(repo_id)?;
    InRepoConfig::load(&display_path)
}

/// Build full [`StatusData`] for a single repo.
pub fn build_repo_status(
    db: &StateDb,
    config: &UserConfig,
    repo_id: &str,
) -> Result<StatusData> {
    let repo_state = db
        .get_repo(repo_id)?
        .ok_or_else(|| anyhow::anyhow!("repo not registered: {}", repo_id))?;

    let branches_db = db.list_branches(repo_id)?;

    let remote_url = repo_state.remote_url.as_deref().unwrap_or("");
    let repo_id_path = PathBuf::from(repo_id);
    let in_repo = load_in_repo_config(&repo_id_path).ok().flatten();
    let mode = config.resolve_repo_mode(remote_url, repo_id, in_repo.as_ref());

    // Look up upstream names from git2
    let git_branches = git_ops::list_branches(&repo_id_path).unwrap_or_default();
    let upstream_map: HashMap<&str, &str> = git_branches
        .iter()
        .filter_map(|b| b.upstream_name.as_deref().map(|u| (b.name.as_str(), u)))
        .collect();

    let branches = branches_db
        .iter()
        .map(|b| BranchStatusData {
            name: b.branch_name.clone(),
            upstream: upstream_map
                .get(b.branch_name.as_str())
                .map(|s| s.to_string()),
            status: b.sync_status.clone(),
            last_action: b
                .last_pull_at
                .as_ref()
                .or(b.last_push_at.as_ref())
                .cloned(),
        })
        .collect();

    Ok(StatusData {
        repo_id: repo_id.to_string(),
        display_path: repo_state.display_path,
        mode: mode.to_string(),
        last_sync: repo_state.last_sync_at,
        branches,
    })
}

/// Build global status for all repos.
pub fn build_global_status(
    db: &StateDb,
    config: &UserConfig,
) -> Result<Vec<RepoStatusData>> {
    let repos = db.list_repos()?;
    let mut result = Vec::with_capacity(repos.len());

    for rs in &repos {
        let remote_url = rs.remote_url.as_deref().unwrap_or("");
        let repo_id_path = PathBuf::from(&rs.repo_id);
        let in_repo = load_in_repo_config(&repo_id_path).ok().flatten();
        let mode = config.resolve_repo_mode(remote_url, &rs.repo_id, in_repo.as_ref());

        let branches = db.list_branches(&rs.repo_id).unwrap_or_default();
        let total = branches.len();
        let synced = branches
            .iter()
            .filter(|b| b.sync_status == "synced" || b.sync_status == "up_to_date")
            .count();
        let diverged = branches
            .iter()
            .filter(|b| b.sync_status == "diverged")
            .count();

        let status_summary = if rs.status == "disabled" {
            "disabled".to_string()
        } else if rs.status == "missing" {
            "missing".to_string()
        } else if diverged > 0 {
            format!("{}/{} diverged", diverged, total)
        } else {
            format!("{} synced", synced)
        };

        result.push(RepoStatusData {
            display_path: rs.display_path.clone(),
            mode: mode.to_string(),
            status_summary,
            last_sync: rs.last_sync_at.clone(),
        });
    }

    Ok(result)
}
