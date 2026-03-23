//! Daemon sync loop and management.
//!
//! The daemon runs in the background, watches registered repos, and keeps
//! local branches in sync with their tracking remotes. It listens on a Unix
//! domain socket for CLI requests and periodically runs sync cycles.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify, RwLock, watch};
use tracing::{error, info, warn};

use crate::config::{
    BranchSyncMode, RepoSyncMode, UserConfig,
};
use crate::git_ops::{
    self, MergeAnalysis, PushResult,
};
use crate::paths;
use crate::state::{BranchState, StateDb, WorktreeState};
use crate::transport::{
    self, Request, Response,
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
    pub config: RwLock<UserConfig>,
    pub db: Mutex<StateDb>,
    pub start_time: Instant,
    pub repos: RwLock<HashMap<String, TrackedRepo>>,
    pub sync_notify: Notify,
}

/// In-memory tracking info for a single repo.
pub struct TrackedRepo {
    pub repo_id: String,
    pub display_path: String,
    pub remote_url: Option<String>,
    pub last_sync: Option<Instant>,
    pub backoff: BackoffState,
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
        self.fetch_backoff_until = Some(Instant::now() + std::time::Duration::from_secs(secs));
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
        entry.0 = Instant::now() + std::time::Duration::from_secs(secs);
    }

    fn reset_push_backoff(&mut self, ref_name: &str) {
        self.per_ref_backoff.remove(ref_name);
    }
}

type SharedDaemon = Arc<Daemon>;

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run the daemon. This is the main entry point called by `gitsitter daemon run`.
pub async fn run_daemon() -> Result<()> {
    // 1. Ensure directories exist
    paths::ensure_dirs()?;

    // 2. Write PID file
    let pid = std::process::id();
    let pid_path = paths::daemon_pid();
    std::fs::write(&pid_path, pid.to_string())
        .with_context(|| format!("failed to write PID file at {}", pid_path.display()))?;

    // 3. Set up tracing (log to stderr — captured by systemd/journald,
    //    redirected to daemon.log by `daemon start`, or visible in terminal
    //    when running `daemon run` interactively)
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    info!("gitsitter daemon starting (pid={})", pid);

    // 4. Load config
    let config = UserConfig::load().context("failed to load config")?;
    info!("loaded config from {}", paths::config_file().display());

    // 5. Open state database
    let db = StateDb::open().context("failed to open state database")?;
    info!("opened state database at {}", paths::state_db().display());

    // 6. Load known repos from DB into memory
    let repo_states = {
        let repos = db.list_repos()?;
        let mut map = HashMap::new();
        for rs in repos {
            map.insert(
                rs.repo_id.clone(),
                TrackedRepo {
                    repo_id: rs.repo_id.clone(),
                    display_path: rs.display_path.clone(),
                    remote_url: rs.remote_url.clone(),
                    last_sync: None, // will sync on first cycle
                    backoff: BackoffState::new(),
                },
            );
        }
        map
    };
    let repo_count = repo_states.len();
    info!("loaded {} repos from state database", repo_count);

    // 7. Build shared state
    let daemon = Arc::new(Daemon {
        config: RwLock::new(config),
        db: Mutex::new(db),
        start_time: Instant::now(),
        repos: RwLock::new(repo_states),
        sync_notify: Notify::new(),
    });

    // 8. Remove stale socket file if it exists
    let socket_path = paths::socket_path();
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }

    // 9. Start Unix socket listener
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind socket at {}", socket_path.display()))?;
    info!("listening on {}", socket_path.display());

    // 10. Shutdown channel
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // 11. Spawn socket handler task
    let daemon_for_socket = Arc::clone(&daemon);
    let shutdown_rx_socket = shutdown_rx.clone();
    let socket_task = tokio::spawn(async move {
        socket_accept_loop(daemon_for_socket, listener, shutdown_rx_socket).await;
    });

    // 12. Spawn sync loop task
    let daemon_for_sync = Arc::clone(&daemon);
    let shutdown_rx_sync = shutdown_rx.clone();
    let sync_task = tokio::spawn(async move {
        sync_loop(daemon_for_sync, shutdown_rx_sync).await;
    });

    // 12b. Spawn file watcher task
    let daemon_for_watcher = Arc::clone(&daemon);
    let shutdown_rx_watcher = shutdown_rx.clone();
    let watcher_task = tokio::spawn(async move {
        crate::watcher::run(daemon_for_watcher, shutdown_rx_watcher).await;
    });

    // 13. Wait for shutdown signal (SIGTERM / SIGINT)
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("failed to register SIGTERM handler")?;
    let mut sigint =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .context("failed to register SIGINT handler")?;

    tokio::select! {
        _ = sigterm.recv() => info!("received SIGTERM"),
        _ = sigint.recv() => info!("received SIGINT"),
        _ = wait_for_shutdown_request(&daemon) => info!("shutdown requested via socket"),
    }

    // 14. Signal shutdown to all tasks
    info!("shutting down...");
    let _ = shutdown_tx.send(true);

    // Give tasks a moment to finish
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        async {
            let _ = socket_task.await;
            let _ = sync_task.await;
            let _ = watcher_task.await;
        },
    )
    .await;

    // 15. Cleanup: remove socket file and PID file
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&pid_path);

    info!("daemon stopped cleanly");
    Ok(())
}

/// Waits until a shutdown is requested via the `sync_notify` mechanism.
/// We use a dedicated atomic flag for shutdown requests via socket.
async fn wait_for_shutdown_request(daemon: &SharedDaemon) {
    // We'll repurpose a simple approach: check a flag in a loop.
    // The socket handler sets this when it receives a Shutdown request.
    // We use a simple polling approach since shutdown via socket is rare.
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        // Check if shutdown was requested (we store it as a special repo entry)
        let repos = daemon.repos.read().await;
        if repos.contains_key("__shutdown__") {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Socket handling
// ---------------------------------------------------------------------------

async fn socket_accept_loop(
    daemon: SharedDaemon,
    listener: UnixListener,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _addr)) => {
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

async fn handle_connection(daemon: SharedDaemon, mut stream: UnixStream) -> Result<()> {
    let (mut reader, mut writer) = stream.split();
    let request = transport::recv_request(&mut reader).await?;

    let response = process_request(&daemon, request).await;

    transport::send_response(&mut writer, &response).await?;
    Ok(())
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
        Request::Register { repo_path } => handle_register(daemon, &repo_path).await,
        Request::Enable { repo_path } => handle_enable(daemon, &repo_path).await,
        Request::Disable { repo_path, purge } => {
            handle_disable(daemon, &repo_path, purge).await
        }
        Request::ConfigUpdate { .. } => handle_config_update(daemon).await,
        Request::DaemonStatus => handle_daemon_status(daemon).await,
        Request::Shutdown => handle_shutdown(daemon).await,
        Request::Log {
            repo_path,
            global,
            follow: _,
            since: _,
        } => handle_log(daemon, repo_path, global).await,
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

    let db = daemon.db.lock().await;
    let config = daemon.config.read().await;

    match crate::queries::build_repo_status(&db, &config, &repo_id_str) {
        Ok(data) => Response::Status { data },
        Err(e) => Response::Error {
            message: format!("{}", e),
        },
    }
}

async fn handle_global_status(daemon: &SharedDaemon) -> Response {
    let db = daemon.db.lock().await;
    let config = daemon.config.read().await;

    match crate::queries::build_global_status(&db, &config) {
        Ok(repos) => Response::GlobalStatus { repos },
        Err(e) => Response::Error {
            message: format!("database error: {}", e),
        },
    }
}

async fn handle_sync(
    daemon: &SharedDaemon,
    repo_path: Option<String>,
    all: bool,
) -> Response {
    if all {
        // Reset last_sync on all repos to force immediate re-sync
        let mut repos = daemon.repos.write().await;
        for tr in repos.values_mut() {
            tr.last_sync = None;
        }
        drop(repos);
        daemon.sync_notify.notify_one();
        Response::Ok {
            message: "sync triggered for all repos".into(),
        }
    } else if let Some(path) = repo_path {
        match resolve_repo_id_from_path(&path) {
            Ok(repo_id) => {
                let repo_id_str = repo_id.to_string_lossy().to_string();
                let mut repos = daemon.repos.write().await;
                if let Some(tr) = repos.get_mut(&repo_id_str) {
                    tr.last_sync = None;
                    drop(repos);
                    daemon.sync_notify.notify_one();
                    Response::Ok {
                        message: format!("sync triggered for {}", path),
                    }
                } else {
                    drop(repos);
                    Response::Error {
                        message: format!("repo not registered: {}", path),
                    }
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

async fn handle_register(daemon: &SharedDaemon, repo_path: &str) -> Response {
    let path = Path::new(repo_path);

    // Discover canonical repo_id
    let repo_id = match git_ops::discover_repo_id(path) {
        Ok(id) => id,
        Err(e) => {
            return Response::Error {
                message: format!("not a git repository: {}", e),
            };
        }
    };
    let repo_id_str = repo_id.to_string_lossy().to_string();

    // Get display path
    let display_path = git_ops::get_display_path(&repo_id)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| repo_path.to_string());

    // Get remote URL
    let remote_url = git_ops::get_remote_url(&repo_id)
        .ok()
        .flatten();

    // Upsert into DB
    {
        let db = daemon.db.lock().await;
        if let Err(e) = db.upsert_repo(&repo_id_str, &display_path, remote_url.as_deref()) {
            return Response::Error {
                message: format!("database error: {}", e),
            };
        }
    }

    // Add to in-memory tracking
    {
        let mut repos = daemon.repos.write().await;
        repos.entry(repo_id_str.clone()).or_insert_with(|| TrackedRepo {
            repo_id: repo_id_str.clone(),
            display_path: display_path.clone(),
            remote_url: remote_url.clone(),
            last_sync: None,
            backoff: BackoffState::new(),
        });
    }

    info!("registered repo: {} ({})", display_path, repo_id_str);
    Response::Ok {
        message: format!("registered {}", display_path),
    }
}

async fn handle_enable(daemon: &SharedDaemon, repo_path: &str) -> Response {
    match resolve_repo_id_from_path(repo_path) {
        Ok(repo_id) => {
            let repo_id_str = repo_id.to_string_lossy().to_string();
            let db = daemon.db.lock().await;
            match db.set_repo_status(&repo_id_str, "active") {
                Ok(()) => {
                    info!("enabled repo: {}", repo_path);
                    Response::Ok {
                        message: format!("enabled {}", repo_path),
                    }
                }
                Err(e) => Response::Error {
                    message: format!("database error: {}", e),
                },
            }
        }
        Err(e) => Response::Error {
            message: format!("failed to resolve repo: {}", e),
        },
    }
}

async fn handle_disable(daemon: &SharedDaemon, repo_path: &str, purge: bool) -> Response {
    match resolve_repo_id_from_path(repo_path) {
        Ok(repo_id) => {
            let repo_id_str = repo_id.to_string_lossy().to_string();
            let db = daemon.db.lock().await;
            if purge {
                match db.remove_repo(&repo_id_str) {
                    Ok(()) => {
                        drop(db);
                        let mut repos = daemon.repos.write().await;
                        repos.remove(&repo_id_str);
                        info!("purged repo: {}", repo_path);
                        Response::Ok {
                            message: format!("purged {}", repo_path),
                        }
                    }
                    Err(e) => Response::Error {
                        message: format!("database error: {}", e),
                    },
                }
            } else {
                match db.set_repo_status(&repo_id_str, "disabled") {
                    Ok(()) => {
                        info!("disabled repo: {}", repo_path);
                        Response::Ok {
                            message: format!("disabled {}", repo_path),
                        }
                    }
                    Err(e) => Response::Error {
                        message: format!("database error: {}", e),
                    },
                }
            }
        }
        Err(e) => Response::Error {
            message: format!("failed to resolve repo: {}", e),
        },
    }
}

async fn handle_config_update(daemon: &SharedDaemon) -> Response {
    match UserConfig::load() {
        Ok(new_config) => {
            let mut config = daemon.config.write().await;
            *config = new_config;
            info!("config reloaded from disk");
            Response::Ok {
                message: "config reloaded".into(),
            }
        }
        Err(e) => Response::Error {
            message: format!("failed to reload config: {}", e),
        },
    }
}

async fn handle_daemon_status(daemon: &SharedDaemon) -> Response {
    let uptime = daemon.start_time.elapsed();
    let repos = daemon.repos.read().await;
    Response::DaemonStatus {
        pid: std::process::id(),
        uptime_secs: uptime.as_secs(),
        repos_watched: repos.len(),
    }
}

async fn handle_shutdown(daemon: &SharedDaemon) -> Response {
    info!("shutdown requested via socket");
    // Signal shutdown by inserting a sentinel key
    let mut repos = daemon.repos.write().await;
    repos.insert(
        "__shutdown__".to_string(),
        TrackedRepo {
            repo_id: "__shutdown__".to_string(),
            display_path: String::new(),
            remote_url: None,
            last_sync: None,
            backoff: BackoffState::new(),
        },
    );
    Response::Ok {
        message: "shutting down".into(),
    }
}

async fn handle_log(
    _daemon: &SharedDaemon,
    _repo_path: Option<String>,
    _global: bool,
) -> Response {
    // Read recent entries from the log file
    let log_path = paths::daemon_log();
    match std::fs::read_to_string(&log_path) {
        Ok(content) => {
            // Return the last 100 lines
            let lines: Vec<&str> = content.lines().collect();
            let start = if lines.len() > 100 {
                lines.len() - 100
            } else {
                0
            };
            let recent = lines[start..].join("\n");
            Response::Ok { message: recent }
        }
        Err(e) => Response::Error {
            message: format!("failed to read log: {}", e),
        },
    }
}

// ---------------------------------------------------------------------------
// Sync loop
// ---------------------------------------------------------------------------

async fn sync_loop(daemon: SharedDaemon, mut shutdown_rx: watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
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
                // Skip the shutdown sentinel
                if repo_id == "__shutdown__" {
                    continue;
                }

                let refresh_interval = config.effective_refresh_interval(
                    repo_id,
                    None, // in-repo config loaded per-sync
                );

                let is_due = match tracked.last_sync {
                    None => true,
                    Some(last) => last.elapsed() >= refresh_interval,
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
                warn!("sync error for {}: {:#}", repo_id, e);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-repo sync
// ---------------------------------------------------------------------------

async fn sync_repo(daemon: &SharedDaemon, repo_id: &str) -> Result<()> {
    let sync_start = Instant::now();
    let repo_path = PathBuf::from(repo_id);

    // 1. Check repo exists
    if !repo_path.exists() {
        warn!("repo path missing: {}", repo_id);
        let db = daemon.db.lock().await;
        db.set_repo_missing(repo_id)?;
        drop(db);
        // Update last_sync so we don't hammer every second
        let mut repos = daemon.repos.write().await;
        if let Some(tr) = repos.get_mut(repo_id) {
            tr.last_sync = Some(Instant::now());
        }
        return Ok(());
    }

    // Check repo is active (not disabled/missing)
    {
        let db = daemon.db.lock().await;
        if let Some(rs) = db.get_repo(repo_id)? {
            if rs.status == "disabled" {
                // Update last_sync to avoid rechecking every second
                drop(db);
                let mut repos = daemon.repos.write().await;
                if let Some(tr) = repos.get_mut(repo_id) {
                    tr.last_sync = Some(Instant::now());
                }
                return Ok(());
            }
            // If it was marked missing but now exists, restore to active
            if rs.status == "missing" {
                db.set_repo_status(repo_id, "active")?;
                info!("repo restored (was missing): {}", repo_id);
            }
        }
    }

    // 2. Check for in-progress operations
    if git_ops::is_operation_in_progress(&repo_path) {
        // Skip silently — user is mid-operation
        let mut repos = daemon.repos.write().await;
        if let Some(tr) = repos.get_mut(repo_id) {
            tr.last_sync = Some(Instant::now());
        }
        return Ok(());
    }

    // Load config and determine modes
    let config = daemon.config.read().await;
    let remote_url = {
        let repos = daemon.repos.read().await;
        repos
            .get(repo_id)
            .and_then(|tr| tr.remote_url.clone())
            .unwrap_or_default()
    };

    let in_repo_config = crate::queries::load_in_repo_config(&repo_path).ok().flatten();
    let repo_mode = config.resolve_repo_mode(
        &remote_url,
        repo_id,
        in_repo_config.as_ref(),
    );

    // If mode is None, skip entirely
    if repo_mode == RepoSyncMode::None {
        let mut repos = daemon.repos.write().await;
        if let Some(tr) = repos.get_mut(repo_id) {
            tr.last_sync = Some(Instant::now());
        }
        drop(repos);
        drop(config);
        return Ok(());
    }

    let git_path = config.global.git_path.clone();
    let git_path_ref = git_path.as_deref();

    // 3. Discover worktrees and build branch occupancy map
    let worktrees = git_ops::list_worktrees(&repo_path)
        .unwrap_or_default();
    let occupancy = git_ops::branch_occupancy(&repo_path)
        .unwrap_or_default();

    // Persist worktree state
    {
        let db = daemon.db.lock().await;
        let now = chrono::Utc::now().to_rfc3339();
        for wt in &worktrees {
            let wt_state = WorktreeState {
                path: wt.path.clone(),
                current_head: wt.head_branch.clone(),
                is_clean: wt.is_clean,
                last_seen: now.clone(),
            };
            let _ = db.upsert_worktree(repo_id, &wt_state);
        }
        let current_paths: Vec<&str> = worktrees.iter().map(|wt| wt.path.as_str()).collect();
        let _ = db.remove_stale_worktrees(repo_id, &current_paths);
    }

    // 4. Fetch (if mode includes fetch capability)
    let should_fetch = matches!(
        repo_mode,
        RepoSyncMode::Fetch | RepoSyncMode::Pull | RepoSyncMode::Push | RepoSyncMode::PushPull
    );

    if should_fetch {
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

            match git_ops::git_fetch(&fetch_path, "origin", git_path_ref, GIT_TIMEOUT_SECS).await
            {
                Ok(()) => {
                    let mut repos = daemon.repos.write().await;
                    if let Some(tr) = repos.get_mut(repo_id) {
                        tr.backoff.reset_fetch_backoff();
                    }
                    drop(repos);
                    let db = daemon.db.lock().await;
                    let _ = db.update_repo_fetch_time(repo_id);
                }
                Err(e) => {
                    let err_msg = format!("{:#}", e);
                    warn!("fetch failed for {}: {}", repo_id, err_msg);
                    let mut repos = daemon.repos.write().await;
                    if let Some(tr) = repos.get_mut(repo_id) {
                        tr.backoff.record_fetch_failure();
                    }
                    // Continue anyway — we can still process local state
                }
            }
        }
    }

    // 5. Process each tracked branch
    let branches = git_ops::list_branches(&repo_path).unwrap_or_default();

    for branch in &branches {
        // Only process branches with upstreams
        let upstream_name = match &branch.upstream_name {
            Some(u) => u.clone(),
            None => continue,
        };

        // Resolve branch mode
        let branch_mode = config.resolve_branch_mode(
            repo_id,
            &branch.name,
            in_repo_config.as_ref(),
            repo_mode,
        );

        if branch_mode == BranchSyncMode::None {
            continue;
        }

        let allows_pull = matches!(
            branch_mode,
            BranchSyncMode::Pull | BranchSyncMode::PushPull
        );
        let allows_push = matches!(
            branch_mode,
            BranchSyncMode::Push | BranchSyncMode::PushPull
        );

        // 5a. Analyze merge status
        let analysis = match git_ops::analyze_merge(&repo_path, &branch.name) {
            Ok(a) => a,
            Err(e) => {
                warn!(
                    "merge analysis failed for {}:{}: {:#}",
                    repo_id, branch.name, e
                );
                continue;
            }
        };

        let ref_name = format!("refs/heads/{}", branch.name);

        match analysis {
            // 5b. Upstream gone
            MergeAnalysis::UpstreamGone => {
                info!("upstream gone for {}:{}", repo_id, branch.name);
                let db = daemon.db.lock().await;
                let _ = db.upsert_branch(
                    repo_id,
                    &BranchState {
                        branch_name: branch.name.clone(),
                        sync_status: "upstream_gone".into(),
                        last_pull_at: None,
                        last_push_at: None,
                        local_oid: Some(branch.local_oid.clone()),
                        remote_oid: None,
                        error_message: Some("upstream ref deleted".into()),
                        push_backoff_until: None,
                    },
                );
            }

            // 5c. Up to date
            MergeAnalysis::UpToDate => {
                let db = daemon.db.lock().await;
                let _ = db.upsert_branch(
                    repo_id,
                    &BranchState {
                        branch_name: branch.name.clone(),
                        sync_status: "synced".into(),
                        last_pull_at: None,
                        last_push_at: None,
                        local_oid: Some(branch.local_oid.clone()),
                        remote_oid: branch.remote_oid.clone(),
                        error_message: None,
                        push_backoff_until: None,
                    },
                );
            }

            // 5d. Remote ahead (fast-forward possible)
            MergeAnalysis::FastForward => {
                if !allows_pull {
                    // Mode doesn't allow pulling — just record state
                    let db = daemon.db.lock().await;
                    let _ = db.upsert_branch(
                        repo_id,
                        &BranchState {
                            branch_name: branch.name.clone(),
                            sync_status: "remote_ahead".into(),
                            last_pull_at: None,
                            last_push_at: None,
                            local_oid: Some(branch.local_oid.clone()),
                            remote_oid: branch.remote_oid.clone(),
                            error_message: None,
                            push_backoff_until: None,
                        },
                    );
                    continue;
                }

                let remote_oid = match &branch.remote_oid {
                    Some(oid) => oid.clone(),
                    None => continue,
                };

                if let Some(wt_path) = occupancy.get(&branch.name) {
                    // Branch is checked out in a worktree
                    let wt_path_buf = PathBuf::from(wt_path);

                    // Check if worktree is clean
                    let is_dirty = git_ops::is_worktree_dirty(&wt_path_buf).unwrap_or(true);
                    if is_dirty {
                        info!(
                            "skipping ff for {}:{} — worktree dirty",
                            repo_id, branch.name
                        );
                        let db = daemon.db.lock().await;
                        let _ = db.upsert_branch(
                            repo_id,
                            &BranchState {
                                branch_name: branch.name.clone(),
                                sync_status: "pending_ff_dirty".into(),
                                last_pull_at: None,
                                last_push_at: None,
                                local_oid: Some(branch.local_oid.clone()),
                                remote_oid: Some(remote_oid),
                                error_message: Some("worktree dirty, ff pending".into()),
                                push_backoff_until: None,
                            },
                        );
                        continue;
                    }

                    // Worktree is clean — ff-merge
                    match git_ops::git_ff_merge(
                        &wt_path_buf,
                        &upstream_name,
                        git_path_ref,
                        GIT_TIMEOUT_SECS,
                    )
                    .await
                    {
                        Ok(()) => {
                            info!("ff-merged {}:{}", repo_id, branch.name);
                            let now = chrono::Utc::now().to_rfc3339();
                            let db = daemon.db.lock().await;
                            let _ = db.upsert_branch(
                                repo_id,
                                &BranchState {
                                    branch_name: branch.name.clone(),
                                    sync_status: "synced".into(),
                                    last_pull_at: Some(now),
                                    last_push_at: None,
                                    local_oid: Some(remote_oid.clone()),
                                    remote_oid: Some(remote_oid),
                                    error_message: None,
                                    push_backoff_until: None,
                                },
                            );
                        }
                        Err(e) => {
                            warn!(
                                "ff-merge failed for {}:{}: {:#}",
                                repo_id, branch.name, e
                            );
                            let db = daemon.db.lock().await;
                            let _ = db.upsert_branch(
                                repo_id,
                                &BranchState {
                                    branch_name: branch.name.clone(),
                                    sync_status: "error".into(),
                                    last_pull_at: None,
                                    last_push_at: None,
                                    local_oid: Some(branch.local_oid.clone()),
                                    remote_oid: Some(remote_oid),
                                    error_message: Some(format!("{:#}", e)),
                                    push_backoff_until: None,
                                },
                            );
                        }
                    }
                } else {
                    // Branch NOT checked out — use update-ref
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
                            info!("update-ref {}:{}", repo_id, branch.name);
                            let now = chrono::Utc::now().to_rfc3339();
                            let db = daemon.db.lock().await;
                            let _ = db.upsert_branch(
                                repo_id,
                                &BranchState {
                                    branch_name: branch.name.clone(),
                                    sync_status: "synced".into(),
                                    last_pull_at: Some(now),
                                    last_push_at: None,
                                    local_oid: Some(remote_oid.clone()),
                                    remote_oid: Some(remote_oid),
                                    error_message: None,
                                    push_backoff_until: None,
                                },
                            );
                        }
                        Err(e) => {
                            warn!(
                                "update-ref failed for {}:{}: {:#}",
                                repo_id, branch.name, e
                            );
                            // Likely a race — retry next cycle
                        }
                    }
                }
            }

            // 5e. Local ahead — push
            MergeAnalysis::LocalAhead => {
                if !allows_push {
                    let db = daemon.db.lock().await;
                    let _ = db.upsert_branch(
                        repo_id,
                        &BranchState {
                            branch_name: branch.name.clone(),
                            sync_status: "local_ahead".into(),
                            last_pull_at: None,
                            last_push_at: None,
                            local_oid: Some(branch.local_oid.clone()),
                            remote_oid: branch.remote_oid.clone(),
                            error_message: None,
                            push_backoff_until: None,
                        },
                    );
                    continue;
                }

                // Check push backoff
                let skip_push = {
                    let repos = daemon.repos.read().await;
                    repos
                        .get(repo_id)
                        .map(|tr| tr.backoff.should_skip_push(&ref_name))
                        .unwrap_or(false)
                };
                if skip_push {
                    continue;
                }

                // Determine push path (use main worktree)
                let push_path = worktrees
                    .first()
                    .map(|wt| PathBuf::from(&wt.path))
                    .unwrap_or_else(|| repo_path.clone());

                match git_ops::git_push(
                    &push_path,
                    "origin",
                    &branch.name,
                    git_path_ref,
                    GIT_TIMEOUT_SECS,
                )
                .await
                {
                    Ok(PushResult::Success) => {
                        info!("pushed {}:{}", repo_id, branch.name);
                        let now = chrono::Utc::now().to_rfc3339();
                        let mut repos = daemon.repos.write().await;
                        if let Some(tr) = repos.get_mut(repo_id) {
                            tr.backoff.reset_push_backoff(&ref_name);
                        }
                        drop(repos);
                        let db = daemon.db.lock().await;
                        let _ = db.upsert_branch(
                            repo_id,
                            &BranchState {
                                branch_name: branch.name.clone(),
                                sync_status: "synced".into(),
                                last_pull_at: None,
                                last_push_at: Some(now),
                                local_oid: Some(branch.local_oid.clone()),
                                remote_oid: Some(branch.local_oid.clone()),
                                error_message: None,
                                push_backoff_until: None,
                            },
                        );
                    }
                    Ok(PushResult::Rejected(msg)) => {
                        warn!("push rejected for {}:{}: {}", repo_id, branch.name, msg);
                        let mut repos = daemon.repos.write().await;
                        if let Some(tr) = repos.get_mut(repo_id) {
                            tr.backoff.record_push_failure(&ref_name);
                        }
                        drop(repos);
                        let db = daemon.db.lock().await;
                        let _ = db.upsert_branch(
                            repo_id,
                            &BranchState {
                                branch_name: branch.name.clone(),
                                sync_status: "push_rejected".into(),
                                last_pull_at: None,
                                last_push_at: None,
                                local_oid: Some(branch.local_oid.clone()),
                                remote_oid: branch.remote_oid.clone(),
                                error_message: Some(msg),
                                push_backoff_until: None,
                            },
                        );
                    }
                    Ok(PushResult::AuthFailed(msg)) => {
                        warn!("push auth failed for {}:{}: {}", repo_id, branch.name, msg);
                        // Auth failure is per-remote, record on fetch backoff
                        let mut repos = daemon.repos.write().await;
                        if let Some(tr) = repos.get_mut(repo_id) {
                            tr.backoff.record_fetch_failure();
                        }
                        drop(repos);
                        let db = daemon.db.lock().await;
                        let _ = db.upsert_branch(
                            repo_id,
                            &BranchState {
                                branch_name: branch.name.clone(),
                                sync_status: "auth_failed".into(),
                                last_pull_at: None,
                                last_push_at: None,
                                local_oid: Some(branch.local_oid.clone()),
                                remote_oid: branch.remote_oid.clone(),
                                error_message: Some(msg),
                                push_backoff_until: None,
                            },
                        );
                    }
                    Ok(PushResult::NetworkError(msg)) => {
                        warn!("push network error for {}:{}: {}", repo_id, branch.name, msg);
                        let mut repos = daemon.repos.write().await;
                        if let Some(tr) = repos.get_mut(repo_id) {
                            tr.backoff.record_fetch_failure();
                        }
                        drop(repos);
                        let db = daemon.db.lock().await;
                        let _ = db.upsert_branch(
                            repo_id,
                            &BranchState {
                                branch_name: branch.name.clone(),
                                sync_status: "network_error".into(),
                                last_pull_at: None,
                                last_push_at: None,
                                local_oid: Some(branch.local_oid.clone()),
                                remote_oid: branch.remote_oid.clone(),
                                error_message: Some(msg),
                                push_backoff_until: None,
                            },
                        );
                    }
                    Ok(PushResult::HookTimeout) => {
                        warn!("push hook timeout for {}:{}", repo_id, branch.name);
                        let mut repos = daemon.repos.write().await;
                        if let Some(tr) = repos.get_mut(repo_id) {
                            tr.backoff.record_push_failure(&ref_name);
                        }
                        drop(repos);
                        let db = daemon.db.lock().await;
                        let _ = db.upsert_branch(
                            repo_id,
                            &BranchState {
                                branch_name: branch.name.clone(),
                                sync_status: "push_blocked_hook_timeout".into(),
                                last_pull_at: None,
                                last_push_at: None,
                                local_oid: Some(branch.local_oid.clone()),
                                remote_oid: branch.remote_oid.clone(),
                                error_message: Some("push hook timed out".into()),
                                push_backoff_until: None,
                            },
                        );
                    }
                    Err(e) => {
                        error!("push error for {}:{}: {:#}", repo_id, branch.name, e);
                    }
                }
            }

            // 5f. Diverged
            MergeAnalysis::Diverged => {
                warn!("diverged: {}:{}", repo_id, branch.name);
                let db = daemon.db.lock().await;
                let _ = db.upsert_branch(
                    repo_id,
                    &BranchState {
                        branch_name: branch.name.clone(),
                        sync_status: "diverged".into(),
                        last_pull_at: None,
                        last_push_at: None,
                        local_oid: Some(branch.local_oid.clone()),
                        remote_oid: branch.remote_oid.clone(),
                        error_message: Some("branch has diverged, ff not possible".into()),
                        push_backoff_until: None,
                    },
                );
            }
        }
    }

    // 6. Update sync timestamp
    {
        let db = daemon.db.lock().await;
        let _ = db.update_repo_sync_time(repo_id);
    }
    {
        let mut repos = daemon.repos.write().await;
        if let Some(tr) = repos.get_mut(repo_id) {
            tr.last_sync = Some(Instant::now());
        }
    }

    let elapsed = sync_start.elapsed();
    info!("✅ sync completed for {} in {:.1?}", repo_id, elapsed);

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a user-provided path to a canonical repo_id.
fn resolve_repo_id_from_path(path: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    git_ops::discover_repo_id(p)
}

