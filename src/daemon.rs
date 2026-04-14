//! Daemon sync loop and management.
//!
//! The daemon runs in the background, watches registered repos, and keeps
//! local branches in sync with their tracking remotes. It listens on a Unix
//! domain socket for CLI requests and periodically runs sync cycles.
//!
//! All runtime state lives in memory. Config.toml is the source of truth for
//! which repos are tracked. No SQLite database is used.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::{Notify, RwLock, watch};
use tracing::{error, info, warn};

/// Log macros that prefix messages with the canonical repo path (the .git dir).
/// This makes log output semi-structured and easy to filter by repo.
macro_rules! repo_info {
    ($repo:expr, $($arg:tt)*) => { info!("{} · {}", $repo, format_args!($($arg)*)) };
}
macro_rules! repo_warn {
    ($repo:expr, $($arg:tt)*) => { warn!("{} · {}", $repo, format_args!($($arg)*)) };
}
macro_rules! repo_error {
    ($repo:expr, $($arg:tt)*) => { error!("{} · {}", $repo, format_args!($($arg)*)) };
}
use tracing_subscriber::fmt::writer::MakeWriterExt;

use crate::config::{EffectiveConflictAction, InRepoConfig, UserConfig};
use crate::forge::ForgeCache;
use crate::git_ops::{self, HistoryRewrite, MergeAnalysis, PushResult};
use crate::paths::Paths;
use crate::transport::{
    self, BranchStatusData, RepoStatusData, Request, Response, StatusData, SyncEvent,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default timeout for git CLI commands (seconds).
const GIT_TIMEOUT_SECS: u64 = 30;

/// Initial backoff duration for fetch/push failures.
const INITIAL_BACKOFF_SECS: u64 = 30;

/// Maximum backoff duration (1 hour).
const MAX_BACKOFF_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Daemon state
// ---------------------------------------------------------------------------

/// Shared daemon state, held behind an `Arc` so multiple tasks can access it.
pub struct Daemon {
    pub paths: Paths,
    pub config: RwLock<UserConfig>,
    pub start_time: Instant,
    pub repos: RwLock<HashMap<String, TrackedRepo>>,
    pub sync_notify: Notify,
    pub shutdown_tx: watch::Sender<bool>,
    pub forge_cache: ForgeCache,
    /// Latest available version if newer than current, checked periodically.
    pub latest_version: RwLock<Option<String>>,
}

/// In-memory tracking info for a single repo.
pub struct TrackedRepo {
    pub repo_id: String,
    pub display_path: String,
    /// Map of remote_name -> URL for all git remotes.
    pub remote_urls: HashMap<String, String>,
    pub last_sync: Option<Instant>,
    pub last_sync_wall: Option<String>,
    pub sync_reason: Option<String>,
    pub in_sync: bool,
    pub backoff: BackoffState,
    pub branches: HashMap<String, BranchRuntimeState>,
    pub notification_cooldowns: HashMap<String, Instant>,
}

/// Runtime state for a single branch, kept in memory only.
#[derive(Debug, Clone)]
pub struct BranchRuntimeState {
    pub sync_status: String,
    pub last_pull_at: Option<String>,
    pub last_push_at: Option<String>,
    pub local_oid: Option<String>,
    pub remote_oid: Option<String>,
    pub error_message: Option<String>,
}

/// Backoff state for a repo's network operations.
pub struct BackoffState {
    /// Per-remote fetch backoff (covers auth/network failures).
    pub fetch_backoff_until: Option<Instant>,
    pub fetch_consecutive_failures: u32,
    /// Per-ref push backoff (covers push rejections).
    pub per_ref_backoff: HashMap<String, (Instant, u32)>,
}

impl BackoffState {
    fn new() -> Self {
        Self {
            fetch_backoff_until: None,
            fetch_consecutive_failures: 0,
            per_ref_backoff: HashMap::new(),
        }
    }

    fn should_skip_fetch(&self) -> bool {
        self.fetch_backoff_until
            .map(|until| Instant::now() < until)
            .unwrap_or(false)
    }

    fn record_fetch_failure(&mut self) {
        self.fetch_consecutive_failures += 1;
        let secs = std::cmp::min(
            INITIAL_BACKOFF_SECS * 2u64.saturating_pow(self.fetch_consecutive_failures - 1),
            MAX_BACKOFF_SECS,
        );
        self.fetch_backoff_until = Some(Instant::now() + Duration::from_secs(secs));
    }

    fn reset_fetch_backoff(&mut self) {
        self.fetch_backoff_until = None;
        self.fetch_consecutive_failures = 0;
    }

    fn should_skip_push(&self, ref_name: &str) -> bool {
        self.per_ref_backoff
            .get(ref_name)
            .map(|(until, _)| Instant::now() < *until)
            .unwrap_or(false)
    }

    fn record_push_failure(&mut self, ref_name: &str) {
        let (_, count) = self
            .per_ref_backoff
            .entry(ref_name.to_string())
            .or_insert((Instant::now(), 0));
        *count += 1;
        let secs = std::cmp::min(
            INITIAL_BACKOFF_SECS * 2u64.saturating_pow(*count - 1),
            MAX_BACKOFF_SECS,
        );
        let entry = self.per_ref_backoff.get_mut(ref_name).unwrap();
        entry.0 = Instant::now() + Duration::from_secs(secs);
    }

    fn reset_push_backoff(&mut self, ref_name: &str) {
        self.per_ref_backoff.remove(ref_name);
    }
}

type SharedDaemon = Arc<Daemon>;

impl TrackedRepo {
    pub fn new(
        repo_id: String,
        display_path: String,
        remote_urls: HashMap<String, String>,
    ) -> Self {
        Self {
            repo_id,
            display_path,
            remote_urls,
            last_sync: None,
            last_sync_wall: None,
            sync_reason: None,
            in_sync: false,
            backoff: BackoffState::new(),
            branches: HashMap::new(),
            notification_cooldowns: HashMap::new(),
        }
    }

    /// Get the URL for the "origin" remote, or the first available remote URL.
    #[allow(dead_code)]
    fn primary_remote_url(&self) -> &str {
        self.remote_urls
            .get("origin")
            .or_else(|| self.remote_urls.values().next())
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    fn set_branch(&mut self, name: &str, state: BranchRuntimeState) {
        self.branches.insert(name.to_string(), state);
    }

    #[allow(dead_code)]
    fn should_notify(&self, key: &str, cooldown: Duration) -> bool {
        match self.notification_cooldowns.get(key) {
            None => true,
            Some(last) => last.elapsed() >= cooldown,
        }
    }

    #[allow(dead_code)]
    fn record_notification(&mut self, key: &str) {
        self.notification_cooldowns
            .insert(key.to_string(), Instant::now());
    }
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run the daemon. This is the main entry point called by `gitsitter daemon run`.
pub async fn run_daemon(paths: &Paths) -> Result<()> {
    // 1. Ensure directories exist
    paths.ensure_dirs()?;

    // 2. Write PID file
    let pid = std::process::id();
    std::fs::write(&paths.daemon_pid, pid.to_string())
        .with_context(|| format!("failed to write PID file at {}", paths.daemon_pid.display()))?;

    // 3. Set up tracing — write to both stderr (for service managers) and daemon.log
    let log_dir = paths
        .daemon_log
        .parent()
        .expect("daemon_log has parent dir");
    let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("daemon.log")
        .max_log_files(1) // keep only today + yesterday; system journal is the primary log
        .build(log_dir)
        .expect("failed to create log appender");
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr.and(file_appender))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .try_init();

    info!("gitsitter daemon starting (pid={})", pid);

    // 4. Load config
    let config = UserConfig::load(paths).context("failed to load config")?;
    info!("initialized config from {}", paths.config_file.display());

    // 5. Seed repos from config.toml
    let mut repo_states: HashMap<String, TrackedRepo> = HashMap::new();
    for (repo_path, repo_cfg) in &config.repos {
        if repo_cfg
            .disabled
            .as_ref()
            .is_some_and(|d| d.is_repo_disabled())
        {
            continue;
        }
        let repo_id_path = PathBuf::from(repo_path);
        let display_path = git_ops::get_display_path(&repo_id_path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| repo_path.clone());
        let remote_urls = git_ops::get_all_remote_urls(&repo_id_path).unwrap_or_default();
        repo_states.insert(
            repo_path.clone(),
            TrackedRepo::new(repo_path.clone(), display_path, remote_urls),
        );
    }
    let repo_count = repo_states.len();
    info!("loaded {} repos from config", repo_count);

    // 6. Shutdown channel (created early so it can be stored in Daemon)
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // 7. Build shared state
    let daemon = Arc::new(Daemon {
        paths: paths.clone(),
        config: RwLock::new(config),
        start_time: Instant::now(),
        repos: RwLock::new(repo_states),
        sync_notify: Notify::new(),
        shutdown_tx,
        forge_cache: ForgeCache::new(),
        latest_version: RwLock::new(crate::self_update::cached_update_available()),
    });

    // 8. Remove stale endpoint if it exists
    transport::cleanup_endpoint(&paths.socket_path);

    // 9. Start listener
    let listener = transport::bind_listener(&paths.socket_path)?;
    info!("listening on {}", paths.socket_path.display());

    // 10. Spawn socket handler task
    let daemon_for_socket = Arc::clone(&daemon);
    let shutdown_rx_socket = shutdown_rx.clone();
    let socket_task = tokio::spawn(async move {
        socket_accept_loop(daemon_for_socket, listener, shutdown_rx_socket).await;
    });

    // 11. Spawn sync loop task
    let daemon_for_sync = Arc::clone(&daemon);
    let shutdown_rx_sync = shutdown_rx.clone();
    let sync_task = tokio::spawn(async move {
        sync_loop(daemon_for_sync, shutdown_rx_sync).await;
    });

    // 11b. Spawn file watcher task
    let daemon_for_watcher = Arc::clone(&daemon);
    let shutdown_rx_watcher = shutdown_rx.clone();
    let watcher_task = tokio::spawn(async move {
        crate::watcher::run(daemon_for_watcher, shutdown_rx_watcher).await;
    });

    // 11c. Spawn update check task (checks every 24h, best-effort)
    let daemon_for_update = Arc::clone(&daemon);
    let mut shutdown_rx_update = shutdown_rx.clone();
    let update_task = tokio::spawn(async move {
        // Check shortly after startup, then every 24h
        let mut interval = tokio::time::interval(Duration::from_secs(60 * 60));
        // First tick is immediate — run the check soon after boot
        interval.tick().await;
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match crate::self_update::check_for_update().await {
                        Ok(Some(v)) => {
                            info!("update available: {v}");
                            *daemon_for_update.latest_version.write().await = Some(v);
                        }
                        Ok(None) => {
                            *daemon_for_update.latest_version.write().await = None;
                        }
                        Err(e) => {
                            warn!("update check failed: {e:#}");
                        }
                    }
                }
                _ = shutdown_rx_update.changed() => {
                    if *shutdown_rx_update.borrow() { return; }
                }
            }
        }
    });

    // 12. Wait for shutdown signal
    let mut shutdown_rx_main = shutdown_rx.clone();
    wait_for_shutdown_signal(&mut shutdown_rx_main).await?;

    // 13. Signal shutdown to all tasks
    info!("shutting down...");
    let _ = daemon.shutdown_tx.send(true);

    // Give tasks a moment to finish
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = socket_task.await;
        let _ = sync_task.await;
        let _ = watcher_task.await;
        let _ = update_task.await;
    })
    .await;

    // 14. Cleanup: remove endpoint and PID file
    transport::cleanup_endpoint(&paths.socket_path);
    let _ = std::fs::remove_file(&paths.daemon_pid);

    info!("daemon stopped cleanly");
    Ok(())
}

// ---------------------------------------------------------------------------
// Socket handling
// ---------------------------------------------------------------------------

async fn socket_accept_loop(
    daemon: SharedDaemon,
    mut listener: transport::DaemonListener,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            result = transport::accept_connection(&mut listener, &daemon.paths.socket_path) => {
                match result {
                    Ok(stream) => {
                        let d = Arc::clone(&daemon);
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(d, stream).await {
                                warn!("connection handler error: {:#}", e);
                            }
                        });
                    }
                    Err(e) => {
                        error!("failed to accept connection: {}", e);
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("socket accept loop shutting down");
                    return;
                }
            }
        }
    }
}

async fn handle_connection(
    daemon: SharedDaemon,
    mut stream: transport::DaemonStream,
) -> Result<()> {
    let request = match transport::recv_request(&mut stream).await {
        Ok(req) => req,
        Err(e) if is_eof(&e) => return Ok(()), // probe or client disconnected
        Err(e) => return Err(e),
    };

    let response = process_request(&daemon, request).await;

    transport::send_response(&mut stream, &response).await?;
    Ok(())
}

fn is_eof(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>()
            && io_err.kind() == std::io::ErrorKind::UnexpectedEof
        {
            return true;
        }
    }
    false
}

async fn process_request(daemon: &SharedDaemon, request: Request) -> Response {
    match request {
        Request::Status { repo_path, global } => {
            if global {
                handle_global_status(daemon).await
            } else {
                handle_status(daemon, repo_path).await
            }
        }
        Request::Sync { repo_path, all } => handle_sync(daemon, repo_path, all).await,
        Request::PromptCheck { repo_path } => handle_prompt_check(daemon, &repo_path).await,
        Request::ReloadConfig => handle_reload_config(daemon).await,
        Request::DaemonStatus => handle_daemon_status(daemon).await,
        Request::Shutdown => handle_shutdown(daemon).await,
    }
}

async fn handle_status(daemon: &SharedDaemon, repo_path: Option<String>) -> Response {
    let repo_path = match repo_path {
        Some(p) => p,
        None => {
            return Response::Error {
                message: "repo_path is required for non-global status".into(),
            };
        }
    };

    let repo_id = match resolve_repo_id_from_path(&repo_path) {
        Ok(id) => id,
        Err(e) => {
            return Response::Error {
                message: format!("failed to resolve repo: {}", e),
            };
        }
    };
    let repo_id_str = repo_id.to_string_lossy().to_string();

    let repos = daemon.repos.read().await;
    let config = daemon.config.read().await;

    match repos.get(&repo_id_str) {
        Some(tr) => {
            let data = build_status_data(tr, &config);
            Response::Status { data }
        }
        None => Response::Error {
            message: format!("repo not registered: {}", repo_path),
        },
    }
}

async fn handle_global_status(daemon: &SharedDaemon) -> Response {
    let repos = daemon.repos.read().await;
    let config = daemon.config.read().await;

    let mut result = Vec::with_capacity(repos.len());
    for tr in repos.values() {
        let total = tr.branches.len();
        let synced = tr
            .branches
            .values()
            .filter(|b| b.sync_status == "synced" || b.sync_status == "up_to_date")
            .count();
        let diverged = tr
            .branches
            .values()
            .filter(|b| b.sync_status == "diverged")
            .count();

        let disabled = config.is_repo_disabled(&tr.repo_id);
        let status_summary = if disabled {
            "disabled".to_string()
        } else if diverged > 0 {
            format!("{}/{} diverged", diverged, total)
        } else {
            format!("{} synced", synced)
        };

        result.push(RepoStatusData {
            display_path: tr.display_path.clone(),
            status_summary,
            last_sync: tr.last_sync_wall.clone(),
        });
    }

    Response::GlobalStatus { repos: result }
}

async fn handle_sync(daemon: &SharedDaemon, repo_path: Option<String>, all: bool) -> Response {
    if all {
        // Collect all repo IDs, then sync each one.
        let repo_ids: Vec<String> = {
            let repos = daemon.repos.read().await;
            repos.keys().cloned().collect()
        };
        info!("⚡ cli sync requested (all repos)");
        for repo_id in &repo_ids {
            if let Err(e) = sync_repo(daemon, repo_id).await {
                repo_warn!(display_repo_label(repo_id), "sync error: {:#}", e);
            }
        }
        Response::Ok {
            message: format!("synced {} repos", repo_ids.len()),
        }
    } else if let Some(path) = repo_path {
        match resolve_repo_id_from_path(&path) {
            Ok(repo_id) => {
                let repo_id_str = repo_id.to_string_lossy().to_string();
                // Check that the repo is registered.
                {
                    let repos = daemon.repos.read().await;
                    if !repos.contains_key(&repo_id_str) {
                        return Response::Error {
                            message: format!("repo not registered: {}", path),
                        };
                    }
                }
                repo_info!(display_repo_label(&path), "⚡ cli sync requested");
                let sync_events = match sync_repo(daemon, &repo_id_str).await {
                    Ok(evts) => evts,
                    Err(e) => {
                        return Response::Error {
                            message: format!("sync failed: {:#}", e),
                        };
                    }
                };
                // Return full status + events after sync.
                let repos = daemon.repos.read().await;
                let config = daemon.config.read().await;
                match repos.get(&repo_id_str) {
                    Some(tr) => {
                        let data = build_status_data(tr, &config);
                        Response::SyncComplete {
                            data,
                            events: sync_events,
                        }
                    }
                    None => Response::Ok {
                        message: format!("synced {}", path),
                    },
                }
            }
            Err(e) => Response::Error {
                message: format!("failed to resolve repo: {}", e),
            },
        }
    } else {
        Response::Error {
            message: "repo_path required when --all is not set".into(),
        }
    }
}

/// Combined register + status for the prompt hook.
///
/// Ensures the repo is in config.toml (writes if missing), reloads config,
/// then returns status. All in one RPC round-trip.
async fn handle_prompt_check(daemon: &SharedDaemon, repo_path: &str) -> Response {
    let path = Path::new(repo_path);

    // Discover canonical repo_id
    let repo_id = match git_ops::discover_repo_id(path) {
        Ok(id) => id,
        Err(_) => {
            // Not a git repo — just return error, don't spam logs
            return Response::Error {
                message: "not a git repository".into(),
            };
        }
    };
    let repo_id_str = repo_id.to_string_lossy().to_string();

    // Check if already tracked — skip config write if so
    let already_tracked = {
        let repos = daemon.repos.read().await;
        repos.contains_key(&repo_id_str)
    };

    if !already_tracked {
        // Write to repos.toml under lock and reload
        let id = repo_id_str.clone();
        if let Err(e) = UserConfig::update_repo(&daemon.paths, &id, |_| {}) {
            repo_warn!(repo_id_str, "failed to register: {:#}", e);
        }
        reload_config(daemon).await;
    }

    let resp = handle_status(daemon, Some(repo_id_str)).await;

    if !already_tracked && let Response::Status { mut data } = resp {
        data.newly_registered = true;
        return Response::Status { data };
    }

    resp
}

async fn handle_reload_config(daemon: &SharedDaemon) -> Response {
    reload_config(daemon).await;
    Response::Ok {
        message: "config reloaded".into(),
    }
}

/// Reload config from disk and sync in-memory repo tracking.
///
/// Called from: ReloadConfig RPC, PromptCheck, and the file watcher.
pub(crate) async fn reload_config(daemon: &SharedDaemon) {
    let new_config = match UserConfig::load(&daemon.paths) {
        Ok(c) => c,
        Err(e) => {
            warn!("failed to reload config: {:#}", e);
            return;
        }
    };

    // Sync in-memory repos with new config
    {
        let mut repos = daemon.repos.write().await;
        // Add new repos from config
        for (repo_path, repo_cfg) in &new_config.repos {
            if repo_cfg
                .disabled
                .as_ref()
                .is_some_and(|d| d.is_repo_disabled())
            {
                continue;
            }
            let repo_id_path = PathBuf::from(repo_path);
            if let Some(tr) = repos.get_mut(repo_path) {
                // Refresh remote URLs for existing tracked repos
                let fresh_urls = git_ops::get_all_remote_urls(&repo_id_path).unwrap_or_default();
                if fresh_urls != tr.remote_urls {
                    repo_info!(repo_path, "remote URLs changed, refreshing");
                    tr.remote_urls = fresh_urls;
                }
            } else {
                let display_path = git_ops::get_display_path(&repo_id_path)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| repo_path.clone());
                let remote_urls = git_ops::get_all_remote_urls(&repo_id_path).unwrap_or_default();
                repos.insert(
                    repo_path.clone(),
                    TrackedRepo::new(repo_path.clone(), display_path, remote_urls),
                );
                repo_info!(repo_path, "tracking new repo");
            }
        }
        // Remove repos no longer in config
        let to_remove: Vec<String> = repos
            .keys()
            .filter(|id| !new_config.repos.contains_key(*id))
            .cloned()
            .collect();
        for id in &to_remove {
            repo_info!(id, "untracking repo");
        }
        for id in to_remove {
            repos.remove(&id);
        }
    }

    let mut config = daemon.config.write().await;
    *config = new_config;
    info!("config reloaded");
}

async fn handle_daemon_status(daemon: &SharedDaemon) -> Response {
    let uptime = daemon.start_time.elapsed();
    let repos = daemon.repos.read().await;
    let latest_version = daemon.latest_version.read().await.clone();
    Response::DaemonStatus {
        pid: std::process::id(),
        uptime_secs: uptime.as_secs(),
        repos_watched: repos.len(),
        latest_version,
    }
}

async fn handle_shutdown(daemon: &SharedDaemon) -> Response {
    info!("shutdown requested via socket");
    let _ = daemon.shutdown_tx.send(true);
    Response::Ok {
        message: "shutting down".into(),
    }
}

// ---------------------------------------------------------------------------
// Status building (from in-memory state)
// ---------------------------------------------------------------------------

fn build_status_data(tr: &TrackedRepo, config: &UserConfig) -> StatusData {
    let repo_id_path = PathBuf::from(&tr.repo_id);

    // Collect untrusted and disabled remote names for status warnings
    let mut untrusted_remotes = Vec::new();
    let mut untrusted_hosts = Vec::new();
    let mut disabled_remotes = Vec::new();
    for (remote_name, remote_url) in &tr.remote_urls {
        if !config.is_remote_trusted(remote_url) {
            untrusted_remotes.push(remote_name.clone());
            if let Some(host) = crate::config::extract_host(remote_url)
                && !untrusted_hosts.contains(&host)
            {
                untrusted_hosts.push(host);
            }
        }
        if config.is_remote_disabled(&tr.repo_id, remote_name) {
            disabled_remotes.push(remote_name.clone());
        }
    }

    // Look up upstream names from git2
    let git_branches = git_ops::list_branches(&repo_id_path).unwrap_or_default();
    let upstream_map: HashMap<&str, &str> = git_branches
        .iter()
        .filter_map(|b| b.upstream_name.as_deref().map(|u| (b.name.as_str(), u)))
        .collect();

    let branches = tr
        .branches
        .iter()
        .map(|(name, bs)| BranchStatusData {
            name: name.clone(),
            upstream: upstream_map.get(name.as_str()).map(|s| s.to_string()),
            status: bs.sync_status.clone(),
            last_action: bs
                .last_pull_at
                .as_ref()
                .or(bs.last_push_at.as_ref())
                .cloned(),
        })
        .collect();

    StatusData {
        repo_id: tr.repo_id.clone(),
        display_path: tr.display_path.clone(),
        last_sync: tr.last_sync_wall.clone(),
        branches,
        untrusted_remotes,
        untrusted_hosts,
        disabled_remotes,
        remote_urls: tr.remote_urls.clone(),
        newly_registered: false,
    }
}

// ---------------------------------------------------------------------------
// Sync loop
// ---------------------------------------------------------------------------

async fn sync_loop(daemon: SharedDaemon, mut shutdown_rx: watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = daemon.sync_notify.notified() => {
                // Immediate sync requested
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("sync loop shutting down");
                    return;
                }
            }
        }

        // Check shutdown
        if *shutdown_rx.borrow() {
            return;
        }

        // Collect repos that are due for a sync
        let repos_to_sync = {
            let repos = daemon.repos.read().await;
            let config = daemon.config.read().await;
            let mut due = Vec::new();

            for (repo_id, tracked) in repos.iter() {
                if tracked.in_sync {
                    continue;
                }

                // Skip disabled repos
                if config.is_repo_disabled(repo_id) {
                    continue;
                }

                let in_repo_config = InRepoConfig::load(Path::new(&tracked.display_path))
                    .unwrap_or_else(|e| {
                        repo_warn!(
                            repo_log_label(repo_id),
                            "failed to load .gitsitter.toml: {:#}",
                            e
                        );
                        None
                    });
                let refresh_interval =
                    config.effective_refresh_interval(repo_id, in_repo_config.as_ref());

                // sync_reason is set by CLI sync / watcher to bypass the timer
                let is_due = if tracked.sync_reason.is_some() {
                    true
                } else {
                    match tracked.last_sync {
                        None => true,
                        Some(last) => last.elapsed() >= refresh_interval,
                    }
                };

                if is_due {
                    due.push(repo_id.clone());
                }
            }
            due
        };

        // Sync each due repo
        for repo_id in repos_to_sync {
            if let Err(e) = sync_repo(&daemon, &repo_id).await {
                repo_warn!(repo_log_label(&repo_id), "sync error: {:#}", e);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-repo sync
// ---------------------------------------------------------------------------

pub async fn sync_repo(daemon: &SharedDaemon, repo_id: &str) -> Result<Vec<SyncEvent>> {
    {
        let mut repos = daemon.repos.write().await;
        if let Some(tr) = repos.get_mut(repo_id) {
            if tr.in_sync {
                return Ok(vec![]);
            }
            tr.in_sync = true;
        }
    }

    let result = sync_repo_inner(daemon, repo_id).await;

    {
        let mut repos = daemon.repos.write().await;
        if let Some(tr) = repos.get_mut(repo_id) {
            tr.in_sync = false;
        }
    }

    result
}

async fn sync_repo_inner(daemon: &SharedDaemon, repo_id: &str) -> Result<Vec<SyncEvent>> {
    let sync_start = Instant::now();
    let repo_path = PathBuf::from(repo_id);
    let repo_label = repo_log_label(repo_id);
    let mut had_activity = false;
    let mut events: Vec<SyncEvent> = Vec::new();

    // 1. Check repo exists
    if !repo_path.exists() {
        repo_warn!(repo_label, "repo path missing");
        // Update last_sync so we don't hammer every second
        let mut repos = daemon.repos.write().await;
        if let Some(tr) = repos.get_mut(repo_id) {
            tr.last_sync = Some(Instant::now());
        }
        return Ok(events);
    }

    // 2. Check for in-progress operations
    if git_ops::is_operation_in_progress(&repo_path) {
        // Skip silently — user is mid-operation
        let mut repos = daemon.repos.write().await;
        if let Some(tr) = repos.get_mut(repo_id) {
            tr.last_sync = Some(Instant::now());
        }
        return Ok(events);
    }

    // Load config and determine modes
    let config = daemon.config.read().await;

    // Skip disabled repos
    if config.is_repo_disabled(repo_id) {
        let mut repos = daemon.repos.write().await;
        if let Some(tr) = repos.get_mut(repo_id) {
            tr.last_sync = Some(Instant::now());
        }
        return Ok(events);
    }

    // Refresh remote URLs from git on every sync cycle so stale cached
    // values don't cause incorrect trust checks or status output (#8).
    let remote_urls = {
        let fresh_urls = git_ops::get_all_remote_urls(Path::new(repo_id)).unwrap_or_default();
        let mut repos = daemon.repos.write().await;
        if let Some(tr) = repos.get_mut(repo_id) {
            if fresh_urls != tr.remote_urls {
                tr.remote_urls = fresh_urls.clone();
            }
            fresh_urls
        } else {
            fresh_urls
        }
    };

    let git_path = config.global.git_path.clone();
    let git_path_ref = git_path.as_deref();

    // 3. Discover worktrees and build branch occupancy map
    let worktrees = git_ops::list_worktrees(&repo_path).unwrap_or_default();
    let occupancy = git_ops::branch_occupancy(&repo_path).unwrap_or_default();

    // 4. List branches (needed to determine remotes for fetch)
    let branches = git_ops::list_branches(&repo_path).unwrap_or_default();

    // 5. Fetch all trusted, non-disabled remotes
    {
        let skip_fetch = {
            let repos = daemon.repos.read().await;
            repos
                .get(repo_id)
                .map(|tr| tr.backoff.should_skip_fetch())
                .unwrap_or(false)
        };

        if !skip_fetch {
            // Determine the work directory for fetch (use the main worktree or repo path)
            let fetch_path = worktrees
                .first()
                .map(|wt| PathBuf::from(&wt.path))
                .unwrap_or_else(|| repo_path.clone());

            // Collect unique remotes from all tracked branches,
            // filtering out untrusted and disabled remotes.
            let mut remotes: Vec<String> = branches
                .iter()
                .filter(|b| b.upstream_name.is_some())
                .map(|b| b.remote.clone())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .filter(|remote_name| {
                    // Skip disabled remotes
                    if config.is_remote_disabled(repo_id, remote_name) {
                        return false;
                    }
                    // Skip untrusted remotes
                    if let Some(url) = remote_urls.get(remote_name) {
                        config.is_remote_trusted(url)
                    } else {
                        true // unknown remote URL — allow (will likely fail at fetch)
                    }
                })
                .collect();
            if remotes.is_empty() && !remote_urls.is_empty() {
                // Only add "origin" fallback if there are actual remotes configured
                if let Some(url) = remote_urls.get("origin")
                    && config.is_remote_trusted(url)
                    && !config.is_remote_disabled(repo_id, "origin")
                {
                    remotes.push("origin".to_string());
                }
            }

            let mut any_success = false;
            for remote in &remotes {
                match git_ops::git_fetch(&fetch_path, remote, git_path_ref, GIT_TIMEOUT_SECS).await
                {
                    Ok(()) => {
                        any_success = true;
                    }
                    Err(e) => {
                        repo_warn!(repo_label, "fetch failed for remote {}: {:#}", remote, e);
                    }
                }
            }

            if any_success {
                events.push(SyncEvent::Fetch {
                    remotes: remotes.clone(),
                });
                let mut repos = daemon.repos.write().await;
                if let Some(tr) = repos.get_mut(repo_id) {
                    tr.backoff.reset_fetch_backoff();
                }
            } else {
                let mut repos = daemon.repos.write().await;
                if let Some(tr) = repos.get_mut(repo_id) {
                    tr.backoff.record_fetch_failure();
                }
                // Continue anyway — we can still process local state
            }
        }
    }

    // Build a set of canonical branches per (remote, remote_ref) pair.
    // If multiple local branches track the same remote ref, the one whose
    // local name matches the remote ref is canonical; the rest are skipped.
    // If none match by name, skip all of them (ambiguous).
    let canonical: std::collections::HashSet<usize> = {
        let mut by_target: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
        for (i, b) in branches.iter().enumerate() {
            if b.upstream_name.is_some() {
                by_target
                    .entry((&b.remote, &b.remote_ref))
                    .or_default()
                    .push(i);
            }
        }
        let mut set = std::collections::HashSet::new();
        for ((remote, remote_ref), indices) in &by_target {
            if indices.len() == 1 {
                set.insert(indices[0]);
            } else {
                // Multiple locals track the same remote ref — pick the one whose name matches
                if let Some(&canon) = indices.iter().find(|&&i| branches[i].name == *remote_ref) {
                    set.insert(canon);
                    for &i in indices {
                        if i != canon {
                            repo_info!(
                                repo_label,
                                "skipping :{} — also tracks {}/{} which is canonical via :{}",
                                branches[i].name,
                                remote,
                                remote_ref,
                                branches[canon].name
                            );
                        }
                    }
                } else {
                    // No name match — ambiguous, skip all
                    for &i in indices {
                        repo_warn!(
                            repo_label,
                            "skipping :{} — multiple branches track {}/{} with no name match",
                            branches[i].name,
                            remote,
                            remote_ref
                        );
                    }
                }
            }
        }
        set
    };

    for (branch_idx, branch) in branches.iter().enumerate() {
        if !canonical.contains(&branch_idx) {
            continue;
        }

        // Only process branches with upstreams
        let upstream_name = match &branch.upstream_name {
            Some(u) => u.clone(),
            None => continue,
        };

        // 5a. Analyze merge status
        let analysis = match git_ops::analyze_merge(&repo_path, &branch.name) {
            Ok(a) => a,
            Err(e) => {
                repo_warn!(
                    repo_label,
                    "merge analysis failed for :{}: {:#}",
                    branch.name,
                    e
                );
                continue;
            }
        };

        let ref_name = format!("refs/heads/{}", branch.name);

        // Gather inputs for the pure decision function
        let is_checked_out = occupancy.contains_key(&branch.name);
        let is_worktree_dirty = if is_checked_out {
            let wt_path = occupancy.get(&branch.name).unwrap();
            git_ops::is_worktree_dirty(&PathBuf::from(wt_path)).unwrap_or(true)
        } else {
            false
        };

        let branch_remote_url = remote_urls.get(&branch.remote).map(|s| s.as_str());
        let is_owner = if matches!(
            analysis,
            MergeAnalysis::LocalAhead | MergeAnalysis::Diverged
        ) {
            git_ops::is_branch_owned_by_user_with_forge(
                &repo_path,
                &branch.name,
                branch_remote_url,
                &daemon.forge_cache,
            )
            .await
            .unwrap_or(false)
        } else {
            false
        };
        let is_merge_of_remote = if matches!(analysis, MergeAnalysis::LocalAhead) && !is_owner {
            git_ops::is_local_merge_of_remote(&repo_path, &branch.name).unwrap_or(false)
        } else {
            false
        };
        let push_backoff_active = {
            let repos = daemon.repos.read().await;
            repos
                .get(repo_id)
                .map(|tr| tr.backoff.should_skip_push(&ref_name))
                .unwrap_or(false)
        };

        let analysis_str = format!("{:?}", analysis);
        let sync_input = BranchSyncInput {
            merge_analysis: analysis,
            is_checked_out,
            is_worktree_dirty,
            is_owner,
            is_merge_of_remote,
            push_backoff_active,
        };
        let action = decide_branch_action(&sync_input);
        let action_str = format!("{:?}", action);

        // Helper to build a branch sync event.
        macro_rules! branch_event {
            ($status:expr, $detail:expr) => {
                branch_event!($status, $detail, None)
            };
            ($status:expr, $detail:expr, $rewrite:expr) => {
                SyncEvent::Branch {
                    branch: branch.name.clone(),
                    analysis: analysis_str.clone(),
                    sync_action: action_str.clone(),
                    rewrite: $rewrite,
                    status: $status.to_string(),
                    detail: $detail.to_string(),
                }
            };
        }

        // Execute the decided action
        match action {
            SyncAction::UpstreamGone => {
                repo_info!(repo_label, "upstream gone :{}", branch.name);
                let mut repos = daemon.repos.write().await;
                if let Some(tr) = repos.get_mut(repo_id) {
                    tr.set_branch(
                        &branch.name,
                        BranchRuntimeState {
                            sync_status: "upstream_gone".into(),
                            last_pull_at: None,
                            last_push_at: None,
                            local_oid: Some(branch.local_oid.clone()),
                            remote_oid: None,
                            error_message: Some("upstream ref deleted".into()),
                        },
                    );
                }
                events.push(branch_event!("upstream_gone", "upstream ref deleted"));
            }

            SyncAction::UpToDate => {
                let mut repos = daemon.repos.write().await;
                if let Some(tr) = repos.get_mut(repo_id) {
                    tr.set_branch(
                        &branch.name,
                        BranchRuntimeState {
                            sync_status: "synced".into(),
                            last_pull_at: None,
                            last_push_at: None,
                            local_oid: Some(branch.local_oid.clone()),
                            remote_oid: branch.remote_oid.clone(),
                            error_message: None,
                        },
                    );
                }
                events.push(branch_event!("synced", "up to date"));
            }

            SyncAction::SkipDirty | SyncAction::SkipRebaseDirty => {
                let label = if matches!(action, SyncAction::SkipDirty) {
                    "ff"
                } else {
                    "rebase"
                };
                repo_info!(
                    repo_label,
                    "skipping {} :{} — worktree dirty",
                    label,
                    branch.name
                );
                let mut repos = daemon.repos.write().await;
                if let Some(tr) = repos.get_mut(repo_id) {
                    tr.set_branch(
                        &branch.name,
                        BranchRuntimeState {
                            sync_status: "pending_dirty".into(),
                            last_pull_at: None,
                            last_push_at: None,
                            local_oid: Some(branch.local_oid.clone()),
                            remote_oid: branch.remote_oid.clone(),
                            error_message: Some("dirty worktree — commit or stash to sync".into()),
                        },
                    );
                }
                events.push(branch_event!("pending_dirty", "skipped — worktree dirty"));
            }

            SyncAction::FastForwardMerge => {
                let remote_oid = match &branch.remote_oid {
                    Some(oid) => oid.clone(),
                    None => continue,
                };
                let wt_path = occupancy.get(&branch.name).unwrap();
                let wt_path_buf = PathBuf::from(wt_path);

                match git_ops::git_ff_merge(
                    &wt_path_buf,
                    &upstream_name,
                    git_path_ref,
                    GIT_TIMEOUT_SECS,
                )
                .await
                {
                    Ok(()) => {
                        had_activity = true;
                        repo_info!(repo_label, "ff-merged :{}", branch.name);
                        let now = chrono::Utc::now().to_rfc3339();
                        let mut repos = daemon.repos.write().await;
                        if let Some(tr) = repos.get_mut(repo_id) {
                            tr.set_branch(
                                &branch.name,
                                BranchRuntimeState {
                                    sync_status: "synced".into(),
                                    last_pull_at: Some(now),
                                    last_push_at: None,
                                    local_oid: Some(remote_oid.clone()),
                                    remote_oid: Some(remote_oid),
                                    error_message: None,
                                },
                            );
                        }
                        events.push(branch_event!("synced", "fast-forwarded"));
                    }
                    Err(e) => {
                        repo_warn!(repo_label, "ff-merge failed :{}: {:#}", branch.name, e);
                        let mut repos = daemon.repos.write().await;
                        if let Some(tr) = repos.get_mut(repo_id) {
                            tr.set_branch(
                                &branch.name,
                                BranchRuntimeState {
                                    sync_status: "error".into(),
                                    last_pull_at: None,
                                    last_push_at: None,
                                    local_oid: Some(branch.local_oid.clone()),
                                    remote_oid: Some(remote_oid),
                                    error_message: Some(format!("{:#}", e)),
                                },
                            );
                        }
                        events.push(branch_event!("error", "fast-forward failed"));
                    }
                }
            }

            SyncAction::FastForwardRef => {
                let remote_oid = match &branch.remote_oid {
                    Some(oid) => oid.clone(),
                    None => continue,
                };
                match git_ops::git_update_ref(
                    &repo_path,
                    &ref_name,
                    &remote_oid,
                    &branch.local_oid,
                    git_path_ref,
                )
                .await
                {
                    Ok(()) => {
                        had_activity = true;
                        repo_info!(repo_label, "update-ref :{}", branch.name);
                        let now = chrono::Utc::now().to_rfc3339();
                        let mut repos = daemon.repos.write().await;
                        if let Some(tr) = repos.get_mut(repo_id) {
                            tr.set_branch(
                                &branch.name,
                                BranchRuntimeState {
                                    sync_status: "synced".into(),
                                    last_pull_at: Some(now),
                                    last_push_at: None,
                                    local_oid: Some(remote_oid.clone()),
                                    remote_oid: Some(remote_oid),
                                    error_message: None,
                                },
                            );
                        }
                        events.push(branch_event!("synced", "fast-forwarded (ref update)"));
                    }
                    Err(e) => {
                        repo_warn!(repo_label, "update-ref failed :{}: {:#}", branch.name, e);
                        events.push(branch_event!("error", "fast-forward ref update failed"));
                    }
                }
            }

            SyncAction::LocalAheadNotOwned => {
                let mut repos = daemon.repos.write().await;
                if let Some(tr) = repos.get_mut(repo_id) {
                    tr.set_branch(&branch.name, BranchRuntimeState {
                        sync_status: "local_ahead".into(),
                        last_pull_at: None,
                        last_push_at: None,
                        local_oid: Some(branch.local_oid.clone()),
                        remote_oid: branch.remote_oid.clone(),
                        error_message: Some("local commits on non-owned branch — push manually or create a new branch".into()),
                    });
                }
                events.push(branch_event!(
                    "local_ahead",
                    "local ahead, not owned — skipped"
                ));
            }

            SyncAction::SkipPushBackoff => {
                // Silently skip — backoff active
            }

            SyncAction::Push => {
                let push_path = worktrees
                    .first()
                    .map(|wt| PathBuf::from(&wt.path))
                    .unwrap_or_else(|| repo_path.clone());

                match git_ops::git_push(
                    &push_path,
                    &branch.remote,
                    &branch.name,
                    &branch.remote_ref,
                    git_path_ref,
                    GIT_TIMEOUT_SECS,
                )
                .await
                {
                    Ok(PushResult::Success) => {
                        had_activity = true;
                        repo_info!(repo_label, "pushed :{}", branch.name);
                        let now = chrono::Utc::now().to_rfc3339();
                        let mut repos = daemon.repos.write().await;
                        if let Some(tr) = repos.get_mut(repo_id) {
                            tr.backoff.reset_push_backoff(&ref_name);
                            tr.set_branch(
                                &branch.name,
                                BranchRuntimeState {
                                    sync_status: "synced".into(),
                                    last_pull_at: None,
                                    last_push_at: Some(now),
                                    local_oid: Some(branch.local_oid.clone()),
                                    remote_oid: Some(branch.local_oid.clone()),
                                    error_message: None,
                                },
                            );
                        }
                        events.push(branch_event!("synced", "pushed"));
                    }
                    Ok(PushResult::Rejected(msg)) => {
                        repo_warn!(repo_label, "push rejected :{}: {}", branch.name, msg);
                        let mut repos = daemon.repos.write().await;
                        if let Some(tr) = repos.get_mut(repo_id) {
                            tr.backoff.record_push_failure(&ref_name);
                            tr.set_branch(
                                &branch.name,
                                BranchRuntimeState {
                                    sync_status: "push_rejected".into(),
                                    last_pull_at: None,
                                    last_push_at: None,
                                    local_oid: Some(branch.local_oid.clone()),
                                    remote_oid: branch.remote_oid.clone(),
                                    error_message: Some(msg),
                                },
                            );
                        }
                    }
                    Ok(PushResult::AuthFailed(msg)) => {
                        repo_warn!(repo_label, "push auth failed :{}: {}", branch.name, msg);
                        let mut repos = daemon.repos.write().await;
                        if let Some(tr) = repos.get_mut(repo_id) {
                            tr.backoff.record_fetch_failure();
                            tr.set_branch(
                                &branch.name,
                                BranchRuntimeState {
                                    sync_status: "auth_failed".into(),
                                    last_pull_at: None,
                                    last_push_at: None,
                                    local_oid: Some(branch.local_oid.clone()),
                                    remote_oid: branch.remote_oid.clone(),
                                    error_message: Some(msg),
                                },
                            );
                        }
                    }
                    Ok(PushResult::NetworkError(msg)) => {
                        repo_warn!(repo_label, "push network error :{}: {}", branch.name, msg);
                        let mut repos = daemon.repos.write().await;
                        if let Some(tr) = repos.get_mut(repo_id) {
                            tr.backoff.record_fetch_failure();
                            tr.set_branch(
                                &branch.name,
                                BranchRuntimeState {
                                    sync_status: "network_error".into(),
                                    last_pull_at: None,
                                    last_push_at: None,
                                    local_oid: Some(branch.local_oid.clone()),
                                    remote_oid: branch.remote_oid.clone(),
                                    error_message: Some(msg),
                                },
                            );
                        }
                    }
                    Ok(PushResult::HookTimeout) => {
                        repo_warn!(repo_label, "push hook timeout :{}", branch.name);
                        let mut repos = daemon.repos.write().await;
                        if let Some(tr) = repos.get_mut(repo_id) {
                            tr.backoff.record_push_failure(&ref_name);
                            tr.set_branch(
                                &branch.name,
                                BranchRuntimeState {
                                    sync_status: "push_blocked_hook_timeout".into(),
                                    last_pull_at: None,
                                    last_push_at: None,
                                    local_oid: Some(branch.local_oid.clone()),
                                    remote_oid: branch.remote_oid.clone(),
                                    error_message: Some("push hook timed out".into()),
                                },
                            );
                        }
                    }
                    Err(e) => {
                        repo_error!(repo_label, "push error :{}: {:#}", branch.name, e);
                        events.push(branch_event!("error", "push failed"));
                    }
                }
            }

            SyncAction::DivergedNotOwned => {
                repo_warn!(repo_label, "diverged :{}", branch.name);
                let mut repos = daemon.repos.write().await;
                if let Some(tr) = repos.get_mut(repo_id) {
                    tr.set_branch(
                        &branch.name,
                        BranchRuntimeState {
                            sync_status: "diverged".into(),
                            last_pull_at: None,
                            last_push_at: None,
                            local_oid: Some(branch.local_oid.clone()),
                            remote_oid: branch.remote_oid.clone(),
                            error_message: Some(
                                "diverged (last remote commit by someone else)".into(),
                            ),
                        },
                    );
                }
                events.push(branch_event!("diverged", "diverged, not owned — flagged"));
            }

            SyncAction::RebaseThenPush => {
                // Before attempting rebase, check if the local branch
                // was intentionally rewritten (e.g. rebase -i, squash).
                let rewrite = git_ops::detect_history_rewrite(&repo_path, &branch.name)
                    .unwrap_or(HistoryRewrite::None);
                let rewrite_str = Some(format!("{:?}", rewrite));

                match rewrite {
                    HistoryRewrite::RemoteUnchanged => {
                        repo_info!(
                            repo_label,
                            "history rewritten (remote unchanged) :{}",
                            branch.name
                        );
                        let mut repos = daemon.repos.write().await;
                        if let Some(tr) = repos.get_mut(repo_id) {
                            tr.set_branch(
                                &branch.name,
                                BranchRuntimeState {
                                    sync_status: "history_rewritten_remote_unchanged".into(),
                                    last_pull_at: None,
                                    last_push_at: None,
                                    local_oid: Some(branch.local_oid.clone()),
                                    remote_oid: branch.remote_oid.clone(),
                                    error_message: Some(
                                        "local history rewritten — push --force-with-lease when ready".into(),
                                    ),
                                },
                            );
                        }
                        events.push(branch_event!(
                            "history_rewritten_remote_unchanged",
                            "history rewrite detected, remote unchanged — holding",
                            rewrite_str.clone()
                        ));
                        continue;
                    }
                    HistoryRewrite::RemoteAdvanced => {
                        repo_info!(
                            repo_label,
                            "history rewritten (remote advanced) :{}",
                            branch.name
                        );
                        let mut repos = daemon.repos.write().await;
                        if let Some(tr) = repos.get_mut(repo_id) {
                            tr.set_branch(
                                &branch.name,
                                BranchRuntimeState {
                                    sync_status: "history_rewritten_remote_advanced".into(),
                                    last_pull_at: None,
                                    last_push_at: None,
                                    local_oid: Some(branch.local_oid.clone()),
                                    remote_oid: branch.remote_oid.clone(),
                                    error_message: Some(
                                        "local history rewritten and remote advanced — force-push would discard remote commits; consider backing up and resetting to remote".into(),
                                    ),
                                },
                            );
                        }
                        events.push(branch_event!(
                            "history_rewritten_remote_advanced",
                            "history rewrite detected, remote advanced — holding",
                            rewrite_str.clone()
                        ));
                        continue;
                    }
                    HistoryRewrite::None => {
                        // Ordinary divergence — proceed with rebase
                    }
                }

                let rebase_path = worktrees
                    .first()
                    .map(|wt| PathBuf::from(&wt.path))
                    .unwrap_or_else(|| repo_path.clone());

                let upstream_ref = format!("{}/{}", branch.remote, branch.remote_ref);
                let rebase_ok = git_ops::git_rebase(
                    &rebase_path,
                    &upstream_ref,
                    git_path_ref,
                    GIT_TIMEOUT_SECS,
                )
                .await
                .unwrap_or(false);

                if rebase_ok {
                    repo_info!(repo_label, "rebased :{} onto {}", branch.name, upstream_ref);
                    let push_path = worktrees
                        .first()
                        .map(|wt| PathBuf::from(&wt.path))
                        .unwrap_or_else(|| repo_path.clone());

                    match git_ops::git_push(
                        &push_path,
                        &branch.remote,
                        &branch.name,
                        &branch.remote_ref,
                        git_path_ref,
                        GIT_TIMEOUT_SECS,
                    )
                    .await
                    {
                        Ok(PushResult::Success) => {
                            had_activity = true;
                            repo_info!(repo_label, "pushed (after rebase) :{}", branch.name);
                            let now = chrono::Utc::now().to_rfc3339();
                            let mut repos = daemon.repos.write().await;
                            if let Some(tr) = repos.get_mut(repo_id) {
                                tr.backoff.reset_push_backoff(&ref_name);
                                tr.set_branch(
                                    &branch.name,
                                    BranchRuntimeState {
                                        sync_status: "synced".into(),
                                        last_pull_at: None,
                                        last_push_at: Some(now),
                                        local_oid: Some(branch.local_oid.clone()),
                                        remote_oid: Some(branch.local_oid.clone()),
                                        error_message: None,
                                    },
                                );
                            }
                            events.push(branch_event!(
                                "synced",
                                format!("rebased onto {}, pushed", upstream_ref),
                                rewrite_str.clone()
                            ));
                        }
                        _ => {
                            repo_warn!(repo_label, "push after rebase failed :{}", branch.name);
                            let mut repos = daemon.repos.write().await;
                            if let Some(tr) = repos.get_mut(repo_id) {
                                tr.backoff.record_push_failure(&ref_name);
                                tr.set_branch(
                                    &branch.name,
                                    BranchRuntimeState {
                                        sync_status: "diverged_yours".into(),
                                        last_pull_at: None,
                                        last_push_at: None,
                                        local_oid: Some(branch.local_oid.clone()),
                                        remote_oid: branch.remote_oid.clone(),
                                        error_message: Some(
                                            "rebased but push failed — will retry".into(),
                                        ),
                                    },
                                );
                            }
                            events.push(branch_event!(
                                "diverged_yours",
                                format!("rebased onto {}, push failed", upstream_ref),
                                rewrite_str.clone()
                            ));
                        }
                    }
                } else {
                    // Rebase had conflicts — apply on_conflict policy
                    repo_warn!(repo_label, "rebase conflicts :{}", branch.name);
                    let has_agent = config.global.resolve_agent.is_some();
                    let conflict_action = config.global.on_conflict.effective(has_agent);

                    match conflict_action {
                        EffectiveConflictAction::Revert => {
                            let _ = git_ops::git_rebase_abort(
                                &rebase_path,
                                git_path_ref,
                                GIT_TIMEOUT_SECS,
                            )
                            .await;
                            let mut repos = daemon.repos.write().await;
                            if let Some(tr) = repos.get_mut(repo_id) {
                                tr.set_branch(
                                    &branch.name,
                                    BranchRuntimeState {
                                        sync_status: "diverged_yours".into(),
                                        last_pull_at: None,
                                        last_push_at: None,
                                        local_oid: Some(branch.local_oid.clone()),
                                        remote_oid: branch.remote_oid.clone(),
                                        error_message: Some(
                                            "diverged — rebase has conflicts, resolve manually"
                                                .into(),
                                        ),
                                    },
                                );
                            }
                            events.push(branch_event!(
                                "diverged_yours",
                                "rebase conflicts, reverted",
                                rewrite_str.clone()
                            ));
                        }
                        EffectiveConflictAction::Leave => {
                            let mut repos = daemon.repos.write().await;
                            if let Some(tr) = repos.get_mut(repo_id) {
                                tr.set_branch(&branch.name, BranchRuntimeState {
                                    sync_status: "merge_conflict".into(),
                                    last_pull_at: None,
                                    last_push_at: None,
                                    local_oid: Some(branch.local_oid.clone()),
                                    remote_oid: branch.remote_oid.clone(),
                                    error_message: Some("rebase conflicts — resolve manually or run `gitsitter auto-resolve`".into()),
                                });
                            }
                            events.push(branch_event!(
                                "merge_conflict",
                                "rebase conflicts, left in place",
                                rewrite_str.clone()
                            ));
                        }
                        EffectiveConflictAction::ResolveAgent => {
                            let agent = config.global.resolve_agent.as_deref().unwrap_or("claude");
                            let agent_path = config.global.resolve_agent_path.as_deref();
                            repo_info!(
                                repo_label,
                                "running resolve agent '{}' :{}",
                                agent,
                                branch.name
                            );

                            const RESOLVE_TIMEOUT_SECS: u64 = 180;
                            match git_ops::run_resolve_agent(
                                &rebase_path,
                                agent,
                                agent_path,
                                RESOLVE_TIMEOUT_SECS,
                            )
                            .await
                            {
                                Ok(result) if result.completed => {
                                    repo_info!(
                                        repo_label,
                                        "resolve agent succeeded :{}",
                                        branch.name
                                    );
                                    match git_ops::git_push(
                                        &rebase_path,
                                        &branch.remote,
                                        &branch.name,
                                        &branch.remote_ref,
                                        git_path_ref,
                                        GIT_TIMEOUT_SECS,
                                    )
                                    .await
                                    {
                                        Ok(PushResult::Success) => {
                                            had_activity = true;
                                            repo_info!(
                                                repo_label,
                                                "pushed (after resolve agent) :{}",
                                                branch.name
                                            );
                                            let now = chrono::Utc::now().to_rfc3339();
                                            let mut repos = daemon.repos.write().await;
                                            if let Some(tr) = repos.get_mut(repo_id) {
                                                tr.backoff.reset_push_backoff(&ref_name);
                                                tr.set_branch(
                                                    &branch.name,
                                                    BranchRuntimeState {
                                                        sync_status: "synced".into(),
                                                        last_pull_at: None,
                                                        last_push_at: Some(now),
                                                        local_oid: Some(branch.local_oid.clone()),
                                                        remote_oid: Some(branch.local_oid.clone()),
                                                        error_message: None,
                                                    },
                                                );
                                            }
                                        }
                                        _ => {
                                            repo_warn!(
                                                repo_label,
                                                "push after resolve agent failed :{}",
                                                branch.name
                                            );
                                            let mut repos = daemon.repos.write().await;
                                            if let Some(tr) = repos.get_mut(repo_id) {
                                                tr.backoff.record_push_failure(&ref_name);
                                                tr.set_branch(
                                                    &branch.name,
                                                    BranchRuntimeState {
                                                        sync_status: "diverged_yours".into(),
                                                        last_pull_at: None,
                                                        last_push_at: None,
                                                        local_oid: Some(branch.local_oid.clone()),
                                                        remote_oid: branch.remote_oid.clone(),
                                                        error_message: Some(
                                                            "resolved but push failed — will retry"
                                                                .into(),
                                                        ),
                                                    },
                                                );
                                            }
                                        }
                                    }
                                }
                                Ok(result) => {
                                    repo_warn!(
                                        repo_label,
                                        "resolve agent did not complete :{}",
                                        branch.name
                                    );
                                    let msg = if result.agent_output.is_empty() {
                                        "resolve agent could not fully resolve conflicts — finish manually or run `gitsitter auto-resolve`".to_string()
                                    } else {
                                        let trimmed: String = result
                                            .agent_output
                                            .lines()
                                            .rev()
                                            .take(3)
                                            .collect::<Vec<_>>()
                                            .into_iter()
                                            .rev()
                                            .collect::<Vec<_>>()
                                            .join("\n");
                                        format!("resolve agent partial: {}", trimmed)
                                    };
                                    let mut repos = daemon.repos.write().await;
                                    if let Some(tr) = repos.get_mut(repo_id) {
                                        tr.set_branch(
                                            &branch.name,
                                            BranchRuntimeState {
                                                sync_status: "merge_conflict".into(),
                                                last_pull_at: None,
                                                last_push_at: None,
                                                local_oid: Some(branch.local_oid.clone()),
                                                remote_oid: branch.remote_oid.clone(),
                                                error_message: Some(msg),
                                            },
                                        );
                                    }
                                }
                                Err(e) => {
                                    repo_warn!(
                                        repo_label,
                                        "resolve agent failed :{}: {:#}",
                                        branch.name,
                                        e
                                    );
                                    let mut repos = daemon.repos.write().await;
                                    if let Some(tr) = repos.get_mut(repo_id) {
                                        tr.set_branch(
                                            &branch.name,
                                            BranchRuntimeState {
                                                sync_status: "merge_conflict".into(),
                                                last_pull_at: None,
                                                last_push_at: None,
                                                local_oid: Some(branch.local_oid.clone()),
                                                remote_oid: branch.remote_oid.clone(),
                                                error_message: Some(format!(
                                                    "resolve agent error: {:#}",
                                                    e
                                                )),
                                            },
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 6. Update sync timestamp
    let now_wall = chrono::Utc::now().to_rfc3339();
    let reason = {
        let mut repos = daemon.repos.write().await;
        let reason = repos.get(repo_id).and_then(|tr| tr.sync_reason.clone());
        if let Some(tr) = repos.get_mut(repo_id) {
            tr.last_sync = Some(Instant::now());
            tr.last_sync_wall = Some(now_wall);
            tr.sync_reason = None;
        }
        reason
    };

    let elapsed = sync_start.elapsed();
    let remote_count = remote_urls.len();
    let branch_count = branches.len();
    let summary = format!("{} remote(s), {} branch(es)", remote_count, branch_count);
    match reason {
        Some(r) if had_activity => repo_info!(
            repo_label,
            "✅ sync completed in {:.1?}, {} ({})",
            elapsed,
            summary,
            r
        ),
        Some(r) => repo_info!(
            repo_label,
            "• scan completed in {:.1?}, {} ({})",
            elapsed,
            summary,
            r
        ),
        None if had_activity => repo_info!(
            repo_label,
            "✅ sync completed in {:.1?}, {} (scheduled)",
            elapsed,
            summary
        ),
        None => repo_info!(
            repo_label,
            "• scheduled scan completed in {:.1?}, {}",
            elapsed,
            summary
        ),
    }

    Ok(events)
}

// ---------------------------------------------------------------------------
// Pure sync decision logic
// ---------------------------------------------------------------------------

/// The action that the sync loop should take for a single branch.
#[derive(Debug, Clone, PartialEq)]
pub enum SyncAction {
    /// Nothing to do — branch is in sync.
    UpToDate,
    /// Upstream tracking ref was deleted.
    UpstreamGone,
    /// Fast-forward the branch (it's checked out in a worktree).
    FastForwardMerge,
    /// Fast-forward the branch via update-ref (not checked out).
    FastForwardRef,
    /// Skip fast-forward because the worktree is dirty.
    SkipDirty,
    /// Push local commits to the remote.
    Push,
    /// Local is ahead but user doesn't own the branch — don't push.
    LocalAheadNotOwned,
    /// Skip push because we're in backoff.
    SkipPushBackoff,
    /// Rebase local onto remote, then push (user owns both sides).
    RebaseThenPush,
    /// Skip rebase because worktree is dirty.
    SkipRebaseDirty,
    /// Diverged but user doesn't own the remote tip — flag it.
    DivergedNotOwned,
}

/// Inputs to the sync decision function. All the state needed to decide
/// what to do with a single branch, without any side effects.
#[derive(Debug, Clone)]
pub struct BranchSyncInput {
    pub merge_analysis: MergeAnalysis,
    pub is_checked_out: bool,
    pub is_worktree_dirty: bool,
    pub is_owner: bool,
    /// For LocalAhead with a merge commit incorporating remote changes.
    pub is_merge_of_remote: bool,
    pub push_backoff_active: bool,
}

/// Decide what sync action to take for a branch, purely from its state.
pub fn decide_branch_action(input: &BranchSyncInput) -> SyncAction {
    match input.merge_analysis {
        MergeAnalysis::UpstreamGone => SyncAction::UpstreamGone,
        MergeAnalysis::UpToDate => SyncAction::UpToDate,

        MergeAnalysis::FastForward => {
            if input.is_checked_out {
                if input.is_worktree_dirty {
                    SyncAction::SkipDirty
                } else {
                    SyncAction::FastForwardMerge
                }
            } else {
                SyncAction::FastForwardRef
            }
        }

        MergeAnalysis::LocalAhead => {
            if !input.is_owner && !input.is_merge_of_remote {
                return SyncAction::LocalAheadNotOwned;
            }
            if input.push_backoff_active {
                return SyncAction::SkipPushBackoff;
            }
            SyncAction::Push
        }

        MergeAnalysis::Diverged => {
            if input.is_owner {
                if input.is_checked_out && input.is_worktree_dirty {
                    SyncAction::SkipRebaseDirty
                } else {
                    SyncAction::RebaseThenPush
                }
            } else {
                SyncAction::DivergedNotOwned
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a user-provided path to a canonical repo_id.
fn resolve_repo_id_from_path(path: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    git_ops::discover_repo_id(p)
}

pub(crate) fn display_repo_label(path: &str) -> String {
    strip_windows_device_prefix(path)
        .trim_end_matches(['\\', '/'])
        .to_string()
}

pub(crate) fn display_path_label(path: &Path) -> String {
    strip_windows_device_prefix(&path.display().to_string())
}

fn repo_log_label(repo_id: &str) -> String {
    display_repo_label(repo_id)
}

fn strip_windows_device_prefix(path: &str) -> String {
    path.strip_prefix(r"\\?\")
        .unwrap_or(path)
        .replace('\\', "/")
}

async fn wait_for_shutdown_signal(shutdown_rx: &mut watch::Receiver<bool>) -> Result<()> {
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("failed to register SIGTERM handler")?;
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .context("failed to register SIGINT handler")?;

        tokio::select! {
            _ = sigterm.recv() => info!("received SIGTERM"),
            _ = sigint.recv() => info!("received SIGINT"),
            _ = shutdown_rx.changed() => info!("shutdown requested via socket"),
        }

        Ok(())
    }

    #[cfg(windows)]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => info!("received Ctrl-C"),
            _ = shutdown_rx.changed() => info!("shutdown requested via socket"),
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input() -> BranchSyncInput {
        BranchSyncInput {
            merge_analysis: MergeAnalysis::UpToDate,
            is_checked_out: false,
            is_worktree_dirty: false,
            is_owner: false,
            is_merge_of_remote: false,
            push_backoff_active: false,
        }
    }

    // --- UpToDate / UpstreamGone ---

    #[test]
    fn up_to_date() {
        let input = BranchSyncInput {
            merge_analysis: MergeAnalysis::UpToDate,
            ..base_input()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::UpToDate);
    }

    #[test]
    fn upstream_gone() {
        let input = BranchSyncInput {
            merge_analysis: MergeAnalysis::UpstreamGone,
            ..base_input()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::UpstreamGone);
    }

    // --- FastForward ---

    #[test]
    fn ff_not_checked_out_uses_update_ref() {
        let input = BranchSyncInput {
            merge_analysis: MergeAnalysis::FastForward,
            is_checked_out: false,
            ..base_input()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::FastForwardRef);
    }

    #[test]
    fn ff_checked_out_clean_does_merge() {
        let input = BranchSyncInput {
            merge_analysis: MergeAnalysis::FastForward,
            is_checked_out: true,
            is_worktree_dirty: false,
            ..base_input()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::FastForwardMerge);
    }

    #[test]
    fn ff_checked_out_dirty_skips() {
        let input = BranchSyncInput {
            merge_analysis: MergeAnalysis::FastForward,
            is_checked_out: true,
            is_worktree_dirty: true,
            ..base_input()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::SkipDirty);
    }

    // --- LocalAhead ---

    #[test]
    fn local_ahead_not_owned() {
        let input = BranchSyncInput {
            merge_analysis: MergeAnalysis::LocalAhead,
            is_owner: false,
            is_merge_of_remote: false,
            ..base_input()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::LocalAheadNotOwned);
    }

    #[test]
    fn local_ahead_owned_pushes() {
        let input = BranchSyncInput {
            merge_analysis: MergeAnalysis::LocalAhead,
            is_owner: true,
            ..base_input()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::Push);
    }

    #[test]
    fn local_ahead_merge_of_remote_pushes() {
        let input = BranchSyncInput {
            merge_analysis: MergeAnalysis::LocalAhead,
            is_owner: false,
            is_merge_of_remote: true,
            ..base_input()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::Push);
    }

    #[test]
    fn local_ahead_push_backoff() {
        let input = BranchSyncInput {
            merge_analysis: MergeAnalysis::LocalAhead,
            is_owner: true,
            push_backoff_active: true,
            ..base_input()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::SkipPushBackoff);
    }

    // --- Diverged ---

    #[test]
    fn diverged_owner_clean_rebases() {
        let input = BranchSyncInput {
            merge_analysis: MergeAnalysis::Diverged,
            is_owner: true,
            is_checked_out: true,
            is_worktree_dirty: false,
            ..base_input()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::RebaseThenPush);
    }

    #[test]
    fn diverged_owner_not_checked_out_rebases() {
        let input = BranchSyncInput {
            merge_analysis: MergeAnalysis::Diverged,
            is_owner: true,
            is_checked_out: false,
            ..base_input()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::RebaseThenPush);
    }

    #[test]
    fn diverged_owner_dirty_skips() {
        let input = BranchSyncInput {
            merge_analysis: MergeAnalysis::Diverged,
            is_owner: true,
            is_checked_out: true,
            is_worktree_dirty: true,
            ..base_input()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::SkipRebaseDirty);
    }

    #[test]
    fn diverged_not_owned() {
        let input = BranchSyncInput {
            merge_analysis: MergeAnalysis::Diverged,
            is_owner: false,
            ..base_input()
        };
        assert_eq!(decide_branch_action(&input), SyncAction::DivergedNotOwned);
    }
}
