//! Forge integration (GitHub, GitLab) for relaxed ownership checks.
//!
//! Provides two fallback mechanisms beyond the local committer-email check:
//! 1. Match the committer email against all verified emails of the authenticated
//!    forge user (e.g. `gh api /user/emails`).
//! 2. Check if the authenticated user is the creator or assignee of a pull
//!    request associated with the branch.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Remote URL parsing
// ---------------------------------------------------------------------------

/// Parsed GitHub remote: owner and repo name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GitHubRepo {
    pub owner: String,
    pub repo: String,
}

/// Parse a GitHub remote URL into owner/repo.
/// Handles SSH (`git@github.com:owner/repo.git`) and
/// HTTPS (`https://github.com/owner/repo.git`) formats.
pub fn parse_github_remote(url: &str) -> Option<GitHubRepo> {
    let url = url.trim();

    // SSH: git@github.com:owner/repo.git
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        let parts: Vec<&str> = rest.splitn(2, '/').collect();
        if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return Some(GitHubRepo {
                owner: parts[0].to_string(),
                repo: parts[1].to_string(),
            });
        }
        return None;
    }

    // HTTPS: https://github.com/owner/repo.git
    // Also handle http:// and ssh://git@github.com/...
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"));

    if let Some(path) = path {
        let path = path.strip_suffix(".git").unwrap_or(path);
        let path = path.strip_suffix('/').unwrap_or(path);
        let parts: Vec<&str> = path.splitn(2, '/').collect();
        if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return Some(GitHubRepo {
                owner: parts[0].to_string(),
                repo: parts[1].to_string(),
            });
        }
    }

    None
}

// ---------------------------------------------------------------------------
// GitHub CLI helpers
// ---------------------------------------------------------------------------

/// Check if `gh` CLI is available and authenticated.
async fn gh_is_available() -> bool {
    let result = tokio::process::Command::new("gh")
        .args(["auth", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    matches!(result, Ok(status) if status.success())
}

/// Call `gh api <endpoint>` and return parsed JSON.
async fn gh_api(endpoint: &str) -> Result<serde_json::Value> {
    let output = tokio::process::Command::new("gh")
        .args(["api", endpoint])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to spawn gh")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh api {} failed: {}", endpoint, stderr.trim());
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("failed to parse gh api response")?;
    Ok(json)
}

// ---------------------------------------------------------------------------
// Forge cache
// ---------------------------------------------------------------------------

type PrCacheMap = HashMap<(String, String, String), (bool, Instant)>;

/// Cached forge data, shared across sync cycles.
pub struct ForgeCache {
    /// Whether `gh` is available. Checked once at first use.
    gh_available: Mutex<Option<bool>>,
    /// Cached GitHub username + verified emails.
    /// (username, emails, fetched_at)
    user_info: Mutex<Option<(String, Vec<String>, Instant)>>,
    /// Cached PR ownership lookups.
    /// Key: (owner, repo, branch) -> (is_owned, fetched_at)
    pr_cache: Mutex<PrCacheMap>,
}

const USER_INFO_TTL: Duration = Duration::from_secs(3600); // 1 hour
const PR_CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes

impl Default for ForgeCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ForgeCache {
    pub fn new() -> Self {
        Self {
            gh_available: Mutex::new(None),
            user_info: Mutex::new(None),
            pr_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Check if `gh` is available (cached after first check).
    async fn ensure_gh(&self) -> bool {
        {
            let cached = self.gh_available.lock().unwrap();
            if let Some(available) = *cached {
                return available;
            }
        }
        let available = gh_is_available().await;
        if !available {
            debug!("gh CLI not available or not authenticated");
        }
        *self.gh_available.lock().unwrap() = Some(available);
        available
    }

    /// Invalidate the gh availability cache (e.g. after an auth failure).
    fn invalidate_gh(&self) {
        *self.gh_available.lock().unwrap() = None;
    }

    /// Get the authenticated GitHub user's username and verified emails.
    async fn get_user_info(&self) -> Result<(String, Vec<String>)> {
        // Check cache
        {
            let cached = self.user_info.lock().unwrap();
            if let Some((ref username, ref emails, fetched_at)) = *cached
                && fetched_at.elapsed() < USER_INFO_TTL
            {
                return Ok((username.clone(), emails.clone()));
            }
        }

        // Fetch username
        let user_json = gh_api("/user").await?;
        let username = user_json["login"]
            .as_str()
            .context("missing login in /user response")?
            .to_string();

        // Fetch verified emails
        let emails_json = gh_api("/user/emails").await?;
        let emails: Vec<String> = emails_json
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter(|e| e["verified"].as_bool().unwrap_or(false))
            .filter_map(|e| e["email"].as_str().map(|s| s.to_string()))
            .collect();

        debug!(
            "fetched GitHub user info: {} with {} verified emails",
            username,
            emails.len()
        );

        *self.user_info.lock().unwrap() = Some((username.clone(), emails.clone(), Instant::now()));
        Ok((username, emails))
    }

    /// Check if a committer email matches any verified email of the GitHub user.
    pub async fn email_matches_user(&self, committer_email: &str) -> bool {
        if !self.ensure_gh().await {
            return false;
        }

        match self.get_user_info().await {
            Ok((_username, emails)) => emails
                .iter()
                .any(|e| e.eq_ignore_ascii_case(committer_email)),
            Err(e) => {
                warn!("failed to fetch GitHub user emails: {:#}", e);
                self.invalidate_gh();
                false
            }
        }
    }

    /// Check if the authenticated user owns a PR for the given branch
    /// (is creator or assignee).
    pub async fn user_owns_pr(&self, gh_repo: &GitHubRepo, branch: &str) -> bool {
        if !self.ensure_gh().await {
            return false;
        }

        // Check cache
        let cache_key = (
            gh_repo.owner.clone(),
            gh_repo.repo.clone(),
            branch.to_string(),
        );
        {
            let cache = self.pr_cache.lock().unwrap();
            if let Some((is_owned, fetched_at)) = cache.get(&cache_key)
                && fetched_at.elapsed() < PR_CACHE_TTL
            {
                return *is_owned;
            }
        }

        let username = match self.get_user_info().await {
            Ok((username, _)) => username,
            Err(e) => {
                warn!("failed to fetch GitHub username for PR check: {:#}", e);
                return false;
            }
        };

        let endpoint = format!(
            "/repos/{}/{}/pulls?head={}:{}&state=open",
            gh_repo.owner, gh_repo.repo, gh_repo.owner, branch
        );

        let is_owned = match gh_api(&endpoint).await {
            Ok(prs) => {
                let empty = vec![];
                let prs = prs.as_array().unwrap_or(&empty);
                prs.iter().any(|pr| {
                    // Check creator
                    let is_creator = pr["user"]["login"]
                        .as_str()
                        .is_some_and(|l| l.eq_ignore_ascii_case(&username));

                    // Check assignees
                    let is_assignee =
                        pr["assignees"]
                            .as_array()
                            .unwrap_or(&vec![])
                            .iter()
                            .any(|a| {
                                a["login"]
                                    .as_str()
                                    .is_some_and(|l| l.eq_ignore_ascii_case(&username))
                            });

                    is_creator || is_assignee
                })
            }
            Err(e) => {
                warn!(
                    "failed to check PRs for {}/{}:{}: {:#}",
                    gh_repo.owner, gh_repo.repo, branch, e
                );
                false
            }
        };

        debug!(
            "PR ownership for {}/{}:{} = {}",
            gh_repo.owner, gh_repo.repo, branch, is_owned
        );

        self.pr_cache
            .lock()
            .unwrap()
            .insert(cache_key, (is_owned, Instant::now()));

        is_owned
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ssh_url() {
        let r = parse_github_remote("git@github.com:user/repo.git").unwrap();
        assert_eq!(r.owner, "user");
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn parse_ssh_url_no_suffix() {
        let r = parse_github_remote("git@github.com:user/repo").unwrap();
        assert_eq!(r.owner, "user");
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn parse_https_url() {
        let r = parse_github_remote("https://github.com/user/repo.git").unwrap();
        assert_eq!(r.owner, "user");
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn parse_https_url_no_suffix() {
        let r = parse_github_remote("https://github.com/user/repo").unwrap();
        assert_eq!(r.owner, "user");
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn parse_https_trailing_slash() {
        let r = parse_github_remote("https://github.com/user/repo/").unwrap();
        assert_eq!(r.owner, "user");
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn parse_ssh_scheme_url() {
        let r = parse_github_remote("ssh://git@github.com/user/repo.git").unwrap();
        assert_eq!(r.owner, "user");
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn parse_non_github_returns_none() {
        assert!(parse_github_remote("git@gitlab.com:user/repo.git").is_none());
        assert!(parse_github_remote("https://gitlab.com/user/repo").is_none());
    }

    #[test]
    fn parse_malformed_returns_none() {
        assert!(parse_github_remote("git@github.com:repo.git").is_none());
        assert!(parse_github_remote("https://github.com/user").is_none());
        assert!(parse_github_remote("not a url").is_none());
    }
}
