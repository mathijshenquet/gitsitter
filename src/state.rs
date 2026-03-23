//! SQLite state storage for the gitsitter daemon.
//!
//! Persists repo metadata, branch sync status, worktree info, and notification
//! cooldowns in a single SQLite database at the path returned by
//! [`crate::paths::state_db()`].

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};

use crate::paths;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RepoState {
    pub repo_id: String,
    pub display_path: String,
    pub remote_url: Option<String>,
    pub status: String,
    pub last_fetch_at: Option<String>,
    pub last_sync_at: Option<String>,
    pub missing_since: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct BranchState {
    pub branch_name: String,
    pub sync_status: String,
    pub last_pull_at: Option<String>,
    pub last_push_at: Option<String>,
    pub local_oid: Option<String>,
    pub remote_oid: Option<String>,
    pub error_message: Option<String>,
    pub push_backoff_until: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorktreeState {
    pub path: String,
    pub current_head: Option<String>,
    pub is_clean: bool,
    pub last_seen: String,
}

// ---------------------------------------------------------------------------
// StateDb
// ---------------------------------------------------------------------------

pub struct StateDb {
    conn: Connection,
}

impl StateDb {
    /// Opens the state database at the default path (`paths::state_db()`),
    /// creating the file and parent directories if they don't exist.
    pub fn open() -> Result<Self> {
        paths::ensure_dirs()?;
        Self::open_at(&paths::state_db())
    }

    /// Opens (or creates) a state database at an arbitrary path.
    /// Intended for testing — does **not** call `ensure_dirs`.
    pub fn open_at(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open state db at {}", path.display()))?;

        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )
        .context("failed to set pragmas")?;

        let db = Self { conn };
        db.create_tables()?;
        Ok(db)
    }

    fn create_tables(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS repos (
                    repo_id TEXT PRIMARY KEY,
                    display_path TEXT NOT NULL,
                    remote_url TEXT,
                    status TEXT NOT NULL DEFAULT 'active',
                    last_fetch_at TEXT,
                    last_sync_at TEXT,
                    missing_since TEXT,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS branches (
                    repo_id TEXT NOT NULL,
                    branch_name TEXT NOT NULL,
                    sync_status TEXT NOT NULL DEFAULT 'unknown',
                    last_pull_at TEXT,
                    last_push_at TEXT,
                    local_oid TEXT,
                    remote_oid TEXT,
                    error_message TEXT,
                    push_backoff_until TEXT,
                    PRIMARY KEY (repo_id, branch_name),
                    FOREIGN KEY (repo_id) REFERENCES repos(repo_id) ON DELETE CASCADE
                );

                CREATE TABLE IF NOT EXISTS worktrees (
                    repo_id TEXT NOT NULL,
                    path TEXT NOT NULL,
                    current_head TEXT,
                    is_clean INTEGER NOT NULL DEFAULT 1,
                    last_seen TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY (repo_id, path),
                    FOREIGN KEY (repo_id) REFERENCES repos(repo_id) ON DELETE CASCADE
                );

                CREATE TABLE IF NOT EXISTS notification_cooldowns (
                    repo_id TEXT NOT NULL,
                    notification_type TEXT NOT NULL,
                    last_shown_at TEXT NOT NULL,
                    PRIMARY KEY (repo_id, notification_type),
                    FOREIGN KEY (repo_id) REFERENCES repos(repo_id) ON DELETE CASCADE
                );",
            )
            .context("failed to create tables")?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Repos
    // -----------------------------------------------------------------------

    pub fn upsert_repo(
        &self,
        repo_id: &str,
        display_path: &str,
        remote_url: Option<&str>,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO repos (repo_id, display_path, remote_url)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(repo_id) DO UPDATE SET
                     display_path = excluded.display_path,
                     remote_url = excluded.remote_url",
                params![repo_id, display_path, remote_url],
            )
            .context("upsert_repo")?;
        Ok(())
    }

    pub fn get_repo(&self, repo_id: &str) -> Result<Option<RepoState>> {
        self.conn
            .query_row(
                "SELECT repo_id, display_path, remote_url, status,
                        last_fetch_at, last_sync_at, missing_since, created_at
                 FROM repos WHERE repo_id = ?1",
                params![repo_id],
                |row| {
                    Ok(RepoState {
                        repo_id: row.get(0)?,
                        display_path: row.get(1)?,
                        remote_url: row.get(2)?,
                        status: row.get(3)?,
                        last_fetch_at: row.get(4)?,
                        last_sync_at: row.get(5)?,
                        missing_since: row.get(6)?,
                        created_at: row.get(7)?,
                    })
                },
            )
            .optional()
            .context("get_repo")
    }

    pub fn list_repos(&self) -> Result<Vec<RepoState>> {
        let mut stmt = self.conn.prepare(
            "SELECT repo_id, display_path, remote_url, status,
                    last_fetch_at, last_sync_at, missing_since, created_at
             FROM repos ORDER BY repo_id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(RepoState {
                    repo_id: row.get(0)?,
                    display_path: row.get(1)?,
                    remote_url: row.get(2)?,
                    status: row.get(3)?,
                    last_fetch_at: row.get(4)?,
                    last_sync_at: row.get(5)?,
                    missing_since: row.get(6)?,
                    created_at: row.get(7)?,
                })
            })
            .context("list_repos")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("list_repos: row iteration")
    }

    pub fn set_repo_status(&self, repo_id: &str, status: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE repos SET status = ?2 WHERE repo_id = ?1",
                params![repo_id, status],
            )
            .context("set_repo_status")?;
        Ok(())
    }

    pub fn update_repo_fetch_time(&self, repo_id: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "UPDATE repos SET last_fetch_at = ?2 WHERE repo_id = ?1",
                params![repo_id, now],
            )
            .context("update_repo_fetch_time")?;
        Ok(())
    }

    pub fn update_repo_sync_time(&self, repo_id: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "UPDATE repos SET last_sync_at = ?2 WHERE repo_id = ?1",
                params![repo_id, now],
            )
            .context("update_repo_sync_time")?;
        Ok(())
    }

    pub fn set_repo_missing(&self, repo_id: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "UPDATE repos SET status = 'missing',
                        missing_since = COALESCE(missing_since, ?2)
                 WHERE repo_id = ?1",
                params![repo_id, now],
            )
            .context("set_repo_missing")?;
        Ok(())
    }

    pub fn remove_repo(&self, repo_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM repos WHERE repo_id = ?1", params![repo_id])
            .context("remove_repo")?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Branches
    // -----------------------------------------------------------------------

    pub fn upsert_branch(&self, repo_id: &str, branch: &BranchState) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO branches
                    (repo_id, branch_name, sync_status, last_pull_at, last_push_at,
                     local_oid, remote_oid, error_message, push_backoff_until)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(repo_id, branch_name) DO UPDATE SET
                     sync_status = excluded.sync_status,
                     last_pull_at = excluded.last_pull_at,
                     last_push_at = excluded.last_push_at,
                     local_oid = excluded.local_oid,
                     remote_oid = excluded.remote_oid,
                     error_message = excluded.error_message,
                     push_backoff_until = excluded.push_backoff_until",
                params![
                    repo_id,
                    branch.branch_name,
                    branch.sync_status,
                    branch.last_pull_at,
                    branch.last_push_at,
                    branch.local_oid,
                    branch.remote_oid,
                    branch.error_message,
                    branch.push_backoff_until,
                ],
            )
            .context("upsert_branch")?;
        Ok(())
    }

    pub fn get_branch(
        &self,
        repo_id: &str,
        branch_name: &str,
    ) -> Result<Option<BranchState>> {
        self.conn
            .query_row(
                "SELECT branch_name, sync_status, last_pull_at, last_push_at,
                        local_oid, remote_oid, error_message, push_backoff_until
                 FROM branches
                 WHERE repo_id = ?1 AND branch_name = ?2",
                params![repo_id, branch_name],
                |row| {
                    Ok(BranchState {
                        branch_name: row.get(0)?,
                        sync_status: row.get(1)?,
                        last_pull_at: row.get(2)?,
                        last_push_at: row.get(3)?,
                        local_oid: row.get(4)?,
                        remote_oid: row.get(5)?,
                        error_message: row.get(6)?,
                        push_backoff_until: row.get(7)?,
                    })
                },
            )
            .optional()
            .context("get_branch")
    }

    pub fn list_branches(&self, repo_id: &str) -> Result<Vec<BranchState>> {
        let mut stmt = self.conn.prepare(
            "SELECT branch_name, sync_status, last_pull_at, last_push_at,
                    local_oid, remote_oid, error_message, push_backoff_until
             FROM branches
             WHERE repo_id = ?1
             ORDER BY branch_name",
        )?;
        let rows = stmt
            .query_map(params![repo_id], |row| {
                Ok(BranchState {
                    branch_name: row.get(0)?,
                    sync_status: row.get(1)?,
                    last_pull_at: row.get(2)?,
                    last_push_at: row.get(3)?,
                    local_oid: row.get(4)?,
                    remote_oid: row.get(5)?,
                    error_message: row.get(6)?,
                    push_backoff_until: row.get(7)?,
                })
            })
            .context("list_branches")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("list_branches: row iteration")
    }

    pub fn remove_branch(&self, repo_id: &str, branch_name: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM branches WHERE repo_id = ?1 AND branch_name = ?2",
                params![repo_id, branch_name],
            )
            .context("remove_branch")?;
        Ok(())
    }

    pub fn remove_branches_for_repo(&self, repo_id: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM branches WHERE repo_id = ?1",
                params![repo_id],
            )
            .context("remove_branches_for_repo")?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Worktrees
    // -----------------------------------------------------------------------

    pub fn upsert_worktree(&self, repo_id: &str, wt: &WorktreeState) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO worktrees (repo_id, path, current_head, is_clean, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(repo_id, path) DO UPDATE SET
                     current_head = excluded.current_head,
                     is_clean = excluded.is_clean,
                     last_seen = excluded.last_seen",
                params![
                    repo_id,
                    wt.path,
                    wt.current_head,
                    wt.is_clean as i32,
                    wt.last_seen,
                ],
            )
            .context("upsert_worktree")?;
        Ok(())
    }

    pub fn list_worktrees(&self, repo_id: &str) -> Result<Vec<WorktreeState>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, current_head, is_clean, last_seen
             FROM worktrees
             WHERE repo_id = ?1
             ORDER BY path",
        )?;
        let rows = stmt
            .query_map(params![repo_id], |row| {
                let is_clean_int: i32 = row.get(2)?;
                Ok(WorktreeState {
                    path: row.get(0)?,
                    current_head: row.get(1)?,
                    is_clean: is_clean_int != 0,
                    last_seen: row.get(3)?,
                })
            })
            .context("list_worktrees")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("list_worktrees: row iteration")
    }

    /// Removes worktrees for `repo_id` whose path is **not** in `current_paths`.
    pub fn remove_stale_worktrees(
        &self,
        repo_id: &str,
        current_paths: &[&str],
    ) -> Result<()> {
        if current_paths.is_empty() {
            // Remove all worktrees for this repo.
            self.conn
                .execute(
                    "DELETE FROM worktrees WHERE repo_id = ?1",
                    params![repo_id],
                )
                .context("remove_stale_worktrees (all)")?;
            return Ok(());
        }

        // Build a parameterised IN clause.
        let placeholders: Vec<String> = (0..current_paths.len())
            .map(|i| format!("?{}", i + 2))
            .collect();
        let sql = format!(
            "DELETE FROM worktrees WHERE repo_id = ?1 AND path NOT IN ({})",
            placeholders.join(", ")
        );

        let mut stmt = self.conn.prepare(&sql).context("remove_stale_worktrees")?;

        // Bind repo_id at index 1, then each path.
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> =
            Vec::with_capacity(current_paths.len() + 1);
        param_values.push(Box::new(repo_id.to_owned()));
        for p in current_paths {
            param_values.push(Box::new((*p).to_owned()));
        }
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|b| b.as_ref()).collect();

        stmt.execute(param_refs.as_slice())
            .context("remove_stale_worktrees")?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Notification cooldowns
    // -----------------------------------------------------------------------

    /// Returns `true` if a notification of this type should be shown — i.e.
    /// either no previous notification exists, or the cooldown has elapsed.
    pub fn should_notify(
        &self,
        repo_id: &str,
        notification_type: &str,
        cooldown: Duration,
    ) -> Result<bool> {
        let last_shown: Option<String> = self
            .conn
            .query_row(
                "SELECT last_shown_at FROM notification_cooldowns
                 WHERE repo_id = ?1 AND notification_type = ?2",
                params![repo_id, notification_type],
                |row| row.get(0),
            )
            .optional()
            .context("should_notify")?;

        let Some(last_shown) = last_shown else {
            return Ok(true);
        };

        let last_shown_dt = chrono::DateTime::parse_from_rfc3339(&last_shown)
            .or_else(|_| {
                // Fall back to parsing SQLite's default datetime format
                // (e.g. "2024-01-15 12:30:00") as UTC.
                chrono::NaiveDateTime::parse_from_str(&last_shown, "%Y-%m-%d %H:%M:%S")
                    .map(|naive| naive.and_utc().fixed_offset())
            })
            .context("should_notify: failed to parse last_shown_at")?;

        let cooldown_chrono = chrono::Duration::from_std(cooldown)
            .unwrap_or(chrono::Duration::MAX);
        let threshold = last_shown_dt + cooldown_chrono;
        Ok(Utc::now() >= threshold)
    }

    pub fn record_notification(
        &self,
        repo_id: &str,
        notification_type: &str,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "INSERT INTO notification_cooldowns (repo_id, notification_type, last_shown_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(repo_id, notification_type) DO UPDATE SET
                     last_shown_at = excluded.last_shown_at",
                params![repo_id, notification_type, now],
            )
            .context("record_notification")?;
        Ok(())
    }
}
