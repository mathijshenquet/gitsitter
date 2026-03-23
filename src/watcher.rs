//! Filesystem watcher for near-instant reaction to local git changes.
//!
//! Uses the `notify` crate to watch `.git/refs/heads/`, `.git/HEAD`, and
//! `.git/refs/remotes/` per registered repo. Events are debounced per-repo
//! (configurable, default 1s) before triggering a sync cycle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher, Event};
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use crate::daemon::Daemon;

/// Default debounce interval for filesystem events.
const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(200);

/// Paths within a git dir that we watch.
const WATCH_SUBDIRS: &[&str] = &["refs/heads", "refs/remotes"];
const WATCH_FILES: &[&str] = &["HEAD"];

/// Run the file watcher loop. Spawned as a tokio task from the daemon.
pub async fn run(
    daemon: Arc<Daemon>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let debounce = daemon
        .config
        .read()
        .await
        .global
        .watcher_debounce
        .unwrap_or(DEFAULT_DEBOUNCE);

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<(PathBuf, Event)>();

    // Create the notify watcher — events are forwarded into the tokio channel.
    let tx = event_tx.clone();
    let mut watcher: RecommendedWatcher = match notify::recommended_watcher(
        move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                // Only react to actual writes — ignore Access events
                // (e.g. VSCode's git extension constantly reads .git/HEAD).
                match event.kind {
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {}
                    _ => return,
                }
                for path in &event.paths {
                    let _ = tx.send((path.clone(), event.clone()));
                }
            }
        },
    ) {
        Ok(w) => w,
        Err(e) => {
            error!("🔍  failed to create file watcher: {:#}", e);
            warn!("file watching disabled — falling back to polling only");
            return;
        }
    };

    // Track which repos we're watching and per-repo debounce timers.
    let mut watched_repos: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut pending: HashMap<String, (Instant, String)> = HashMap::new();

    // Initial setup: watch all currently registered repos.
    {
        let repos = daemon.repos.read().await;
        for repo_id in repos.keys() {
            if repo_id == "__shutdown__" {
                continue;
            }
            if let Err(e) = add_repo_watches(&mut watcher, repo_id, &mut watched_repos) {
                warn!("🔍  failed to watch {}: {:#}", repo_id, e);
            }
        }
    }

    let mut poll_interval = tokio::time::interval(Duration::from_secs(5));
    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Receive filesystem events.
            Some((path, _event)) = event_rx.recv() => {
                // Ignore .lock files — git creates these transiently during
                // ref updates; we'll react to the actual ref file change.
                if path.extension().is_some_and(|ext| ext == "lock") {
                    continue;
                }

                // Ignore refs/remotes/ changes when a sync recently ran or
                // is currently running — these are our own fetch/push
                // updating the remote tracking ref.
                if is_remote_ref(&path) {
                    let dominated = {
                        let repos_guard = daemon.repos.read().await;
                        resolve_repo_id_for_path(&path, &watched_repos)
                            .and_then(|rid| repos_guard.get(&rid))
                            .map(|tr| match tr.last_sync {
                                // last_sync is None when a sync is in-flight
                                None => true,
                                Some(last) => last.elapsed() < debounce * 5,
                            })
                            .unwrap_or(false)
                    };
                    if dominated {
                        continue;
                    }
                }

                if let Some(repo_id) = resolve_repo_id_for_path(&path, &watched_repos) {
                    let reason = describe_change(&path);
                    pending
                        .entry(repo_id)
                        .and_modify(|entry| {
                            entry.0 = Instant::now();
                            entry.1 = reason.clone();
                        })
                        .or_insert_with(|| (Instant::now(), reason));
                }
            }

            // Periodically check for new repos and fire debounced events.
            _ = poll_interval.tick() => {
                // Check for newly registered repos that aren't watched yet.
                let repos = daemon.repos.read().await;
                for repo_id in repos.keys() {
                    if repo_id == "__shutdown__" {
                        continue;
                    }
                    if !watched_repos.contains_key(repo_id) {
                        if let Err(e) = add_repo_watches(&mut watcher, repo_id, &mut watched_repos) {
                            warn!("🔍  failed to watch {}: {:#}", repo_id, e);
                        }
                    }
                }
                // Remove watches for repos that are no longer registered.
                let to_remove: Vec<String> = watched_repos
                    .keys()
                    .filter(|id| !repos.contains_key(*id))
                    .cloned()
                    .collect();
                drop(repos);
                for repo_id in to_remove {
                    remove_repo_watches(&mut watcher, &repo_id, &mut watched_repos);
                }
            }

            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("🔍  file watcher shutting down");
                    return;
                }
            }
        }

        // Process debounced events — fire any that have aged past the debounce window.
        let now = Instant::now();
        let mut fired = Vec::new();
        for (repo_id, (last_event, trigger_path)) in &pending {
            if now.duration_since(*last_event) >= debounce {
                fired.push((repo_id.clone(), trigger_path.clone()));
            }
        }
        for (repo_id, reason) in fired {
            pending.remove(&repo_id);
            info!("🔍  rescanning {} ({})", repo_id, reason);
            let mut repos = daemon.repos.write().await;
            if let Some(tr) = repos.get_mut(&repo_id) {
                tr.last_sync = None;
                tr.sync_reason = Some(reason.clone());
            }
            drop(repos);
            daemon.sync_notify.notify_one();
        }
    }
}

/// Add filesystem watches for a repo's git directory.
fn add_repo_watches(
    watcher: &mut RecommendedWatcher,
    repo_id: &str,
    watched: &mut HashMap<String, Vec<PathBuf>>,
) -> Result<()> {
    let git_dir = PathBuf::from(repo_id);
    if !git_dir.exists() {
        anyhow::bail!("git dir does not exist: {}", git_dir.display());
    }

    let mut paths = Vec::new();

    for subdir in WATCH_SUBDIRS {
        let p = git_dir.join(subdir);
        if p.exists() {
            watcher.watch(&p, RecursiveMode::Recursive)?;
            paths.push(p);
        }
    }
    for file in WATCH_FILES {
        let p = git_dir.join(file);
        if p.exists() {
            watcher.watch(&p, RecursiveMode::NonRecursive)?;
            paths.push(p);
        }
    }

    info!("👁️  watching {}", git_dir.display());
    watched.insert(repo_id.to_string(), paths);
    Ok(())
}

/// Remove filesystem watches for a repo.
fn remove_repo_watches(
    watcher: &mut RecommendedWatcher,
    repo_id: &str,
    watched: &mut HashMap<String, Vec<PathBuf>>,
) {
    if let Some(paths) = watched.remove(repo_id) {
        for p in &paths {
            let _ = watcher.unwatch(p);
        }
        info!("👁️  unwatched {}", repo_id);
    }
}

/// Derive a human-readable reason from a changed path.
///
/// Examples: "ref update (main)", "HEAD changed", "remote ref (origin/main)"
fn describe_change(path: &Path) -> String {
    let components: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();

    // refs/heads/<branch>
    if let Some(pos) = components.iter().position(|&c| c == "heads") {
        let branch = components[pos + 1..].join("/");
        if !branch.is_empty() {
            return format!("ref update ({})", branch);
        }
    }

    // refs/remotes/<remote>/<branch>
    if let Some(pos) = components.iter().position(|&c| c == "remotes") {
        let rest = components[pos + 1..].join("/");
        if !rest.is_empty() {
            return format!("remote ref ({})", rest);
        }
    }

    // HEAD
    if components.last() == Some(&"HEAD") {
        return "HEAD changed".to_string();
    }

    format!("file changed ({})", path.display())
}

/// Check if a path is under refs/remotes/.
fn is_remote_ref(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "remotes")
        && path.components().any(|c| c.as_os_str() == "refs")
}

/// Given a changed path, figure out which repo_id it belongs to.
fn resolve_repo_id_for_path(
    path: &Path,
    watched: &HashMap<String, Vec<PathBuf>>,
) -> Option<String> {
    for (repo_id, paths) in watched {
        for watched_path in paths {
            if path.starts_with(watched_path) || path == watched_path {
                return Some(repo_id.clone());
            }
        }
    }
    None
}

