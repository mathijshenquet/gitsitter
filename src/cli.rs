//! CLI command handlers.
//!
//! Each function corresponds to a CLI subcommand. They connect to the daemon
//! via Unix socket, send a request, and print the response.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use comfy_table::{Cell, Color, ContentArrangement, Table, presets};

use crate::cli_ui::{self, DisplayOpts};
use crate::config::{self, UserConfig};
use crate::git_ops;
use crate::paths::Paths;
use crate::transport::{self, DaemonStream, Request, Response};

/// Load display options from user config (best-effort, defaults if config fails).
fn load_display_opts(paths: &Paths) -> DisplayOpts {
    match UserConfig::load(paths) {
        Ok(cfg) => DisplayOpts {
            emoji: cfg.global.emoji,
            colors: cfg.global.colors,
        },
        Err(_) => DisplayOpts {
            emoji: true,
            colors: true,
        },
    }
}

/// Connect to the daemon, returning a consistent error if it's not running.
async fn require_daemon(paths: &Paths) -> Result<DaemonStream> {
    transport::connect_to_daemon(&paths.socket_path)
        .await
        .context("daemon not running. Start it with `gitsitter daemon start`")
}

/// Best-effort notify the daemon to reload config.
async fn notify_daemon_reload(paths: &Paths) {
    if let Ok(mut stream) = transport::connect_to_daemon(&paths.socket_path).await {
        let _ = roundtrip(&mut stream, &Request::ReloadConfig).await;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Print a one-liner if a newer version is available (from cached state file).
fn print_update_hint() {
    if std::env::var_os("GITSITTER_NO_UPDATE_CHECK").is_some() {
        return;
    }
    if let Some(v) = crate::self_update::cached_update_available() {
        eprintln!(
            "\ngitsitter {v} available (current: v{}), run `gitsitter self-update`",
            crate::self_update::current_version()
        );
    }
}

/// Replace the user's home directory prefix with `~` for display.
fn display_path(p: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if p.starts_with(home_str.as_ref()) {
            return format!("~{}", &p[home_str.len()..]);
        }
    }
    p.to_string()
}

/// Convert an ISO 8601 / RFC 3339 timestamp string to a human-readable relative
/// time like "30s ago", "2m ago", "1h ago", "3d ago".
fn format_relative_time(timestamp: &str) -> String {
    let parsed = DateTime::parse_from_rfc3339(timestamp).or_else(|_| {
        chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%d %H:%M:%S")
            .map(|naive| naive.and_utc().fixed_offset())
    });
    let dt = match parsed {
        Ok(dt) => dt,
        Err(_) => return timestamp.to_string(),
    };
    let now = Utc::now();
    let delta = now.signed_duration_since(dt);
    let secs = delta.num_seconds();
    if secs < 0 {
        return "just now".to_string();
    }
    if secs < 60 {
        return format!("{}s ago", secs);
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{}m ago", mins);
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{}h ago", hours);
    }
    let days = hours / 24;
    format!("{}d ago", days)
}

/// Resolve the repo path for the current directory.
fn resolve_cwd_repo_path() -> Result<String> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let repo_id = git_ops::discover_repo_id(&cwd).context("not inside a git repository")?;
    Ok(repo_id.to_string_lossy().to_string())
}

/// Resolve a user-provided path or fall back to cwd.
fn resolve_path_or_cwd(path: Option<&str>) -> Result<String> {
    let p = match path {
        Some(s) => {
            let pb = PathBuf::from(s);
            if pb.is_absolute() {
                pb
            } else {
                std::env::current_dir()?.join(pb)
            }
        }
        None => std::env::current_dir()?,
    };
    let canonical = p
        .canonicalize()
        .with_context(|| format!("path does not exist: {}", p.display()))?;
    Ok(canonical.to_string_lossy().to_string())
}

/// Send a request and receive a single response over a connected stream.
async fn roundtrip<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    req: &Request,
) -> Result<Response> {
    transport::send_request(stream, req).await?;
    transport::recv_response(stream).await
}

/// Resolve the remote target for enable/disable commands.
///
/// Returns `Some(name)` for a specific remote, `None` for whole-repo.
/// With no arguments and exactly one remote, targets that remote.
/// With no arguments and multiple remotes, errors with a hint to use --all or name a remote.
fn resolve_remote_target<'a>(
    remote: Option<&'a str>,
    all: bool,
    remote_urls: &HashMap<String, String>,
    verb: &str,
) -> Result<Option<&'a str>> {
    if all {
        return Ok(None); // whole repo
    }
    if let Some(name) = remote {
        if !remote_urls.contains_key(name) {
            bail!("remote '{}' not found in this repo", name);
        }
        return Ok(Some(name));
    }
    // No argument: if exactly one remote, target it; otherwise error
    if remote_urls.len() == 1 {
        return Ok(None); // single remote — whole-repo enable/disable is unambiguous
    }
    let names: Vec<&str> = remote_urls.keys().map(|s| s.as_str()).collect();
    bail!(
        "multiple remotes found: {}. Specify a remote name or use --all:\n  gitsitter {} <remote>\n  gitsitter {} --all",
        names.join(", "),
        verb,
        verb,
    );
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn handle_status(paths: &Paths, global: bool) -> Result<()> {
    let opts = load_display_opts(paths);
    let daemon_running = transport::is_daemon_running(&paths.socket_path);

    if !daemon_running {
        println!("{}", cli_ui::daemon_warning(opts));
        println!();
    }

    if global {
        return handle_status_global(paths, opts, daemon_running).await;
    }

    let repo_path = match resolve_cwd_repo_path() {
        Ok(p) => p,
        Err(_) => return handle_status_global(paths, opts, daemon_running).await,
    };

    if !daemon_running {
        println!();
        let dp = display_path(&repo_path);
        println!("{}", cli_ui::repo_header(&dp, opts));
        println!();
        println!("   No sync data available, ensure the daemon is running");
        println!();
        return Ok(());
    }

    let mut stream = require_daemon(paths).await?;
    let req = Request::Status {
        repo_path: Some(repo_path),
        global: false,
    };
    let resp = roundtrip(&mut stream, &req).await?;
    match resp {
        Response::Status { data } => {
            print_repo_status(&data, opts);
        }
        Response::Error { message } => {
            eprintln!("error: {}", message);
        }
        _ => {
            eprintln!("unexpected response from daemon");
        }
    }
    print_update_hint();
    Ok(())
}

async fn handle_status_global(
    paths: &Paths,
    opts: DisplayOpts,
    daemon_running: bool,
) -> Result<()> {
    if !daemon_running {
        let cfg = UserConfig::load(paths)?;
        if cfg.repos.is_empty() {
            println!("No repos registered.");
        } else {
            println!();
            println!("Watched repositories (daemon not running)");
            println!();
            let mut table = global_status_table();
            for (path, repo_cfg) in &cfg.repos {
                let dp = display_path(path);
                let disabled = repo_cfg
                    .disabled
                    .as_ref()
                    .is_some_and(|d| d.is_repo_disabled());
                let sync = if disabled {
                    "never synced, disabled"
                } else {
                    "never synced"
                };
                table.add_row(global_status_row(&dp, sync, "", opts));
            }
            println!("{table}");
            println!();
        }
        return Ok(());
    }

    let mut stream = require_daemon(paths).await?;
    let req = Request::Status {
        repo_path: None,
        global: true,
    };
    let resp = roundtrip(&mut stream, &req).await?;
    match resp {
        Response::GlobalStatus { repos } => {
            if repos.is_empty() {
                println!("No repos being watched.");
            } else {
                println!();
                println!("Watched repositories ({} total)", repos.len());
                println!();
                let mut table = global_status_table();
                for r in &repos {
                    let dp = display_path(&r.display_path);
                    let sync_info = match &r.last_sync {
                        Some(ts) => format!("synced {}", format_relative_time(ts)),
                        None => "never synced".to_string(),
                    };
                    table.add_row(global_status_row(&dp, &sync_info, &r.status_summary, opts));
                }
                println!("{table}");
                println!();
            }
        }
        Response::Error { message } => {
            eprintln!("error: {}", message);
        }
        _ => {
            eprintln!("unexpected response from daemon");
        }
    }
    Ok(())
}

fn global_status_table() -> Table {
    let mut table = Table::new();
    table
        .load_preset(presets::NOTHING)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["Repo", "Last Sync", "Status"]);
    table
}

fn global_status_row(path: &str, sync: &str, status: &str, opts: DisplayOpts) -> Vec<Cell> {
    if opts.colors {
        vec![
            Cell::new(path).fg(Color::Blue),
            Cell::new(sync),
            Cell::new(status),
        ]
    } else {
        vec![Cell::new(path), Cell::new(sync), Cell::new(status)]
    }
}

/// Print warnings about untrusted remotes for a repo (used by enable/register).
fn print_untrusted_remote_warnings(repo_id: &Path, cfg: &UserConfig, opts: DisplayOpts) {
    let remote_urls = git_ops::get_all_remote_urls(repo_id).unwrap_or_default();
    let repo_id_str = repo_id.to_string_lossy();
    for (name, url) in &remote_urls {
        if !cfg.is_remote_trusted(url)
            && let Some(host) = config::extract_host(url)
        {
            println!(
                "  {} remote '{}' ({}) is not trusted — won't sync",
                cli_ui::warning_icon(opts),
                name,
                host
            );
            println!("  Add with: gitsitter trust {}", host);
        }
        if cfg.is_remote_disabled(&repo_id_str, name) {
            println!(
                "  {} remote '{}' is disabled",
                cli_ui::warning_icon(opts),
                name
            );
        }
    }
}

fn print_repo_status(data: &transport::StatusData, opts: DisplayOpts) {
    println!();

    let dp = display_path(&data.display_path);
    let sync_info = match &data.last_sync {
        Some(ts) => format!("synced {}", format_relative_time(ts)),
        None => "never synced".to_string(),
    };
    println!("{}  {}", cli_ui::repo_header(&dp, opts), sync_info);
    println!();
    for b in &data.branches {
        let upstream = b.upstream.as_deref().unwrap_or("(no upstream)");
        let pair = cli_ui::sync_pair(&b.name, upstream);
        let icon = cli_ui::branch_status_icon(&b.status, opts);
        let label = cli_ui::branch_status_styled(&b.status, opts);
        let action = match &b.last_action {
            Some(a) => format!(", {}", a),
            None => String::new(),
        };
        println!("  {:<32} {}  {}{}", pair, icon, label, action);
    }

    // Show remotes section if any exist
    if !data.remote_urls.is_empty() {
        println!();
        println!("  Remotes:");
        for (name, url) in &data.remote_urls {
            let trust = if data.untrusted_remotes.contains(name) {
                let host = crate::config::extract_host(url).unwrap_or_else(|| url.to_string());
                format!(
                    "  {} untrusted \u{2014} run 'gitsitter trust {}'",
                    cli_ui::warning_icon(opts),
                    host
                )
            } else if data.disabled_remotes.contains(name) {
                format!("  {} disabled", cli_ui::warning_icon(opts))
            } else {
                String::new()
            };
            println!("    {}  {}{}", name, url, trust);
        }
    }
    println!();
}

pub async fn handle_config(paths: &Paths) -> Result<()> {
    let cfg = UserConfig::load(paths)?;

    println!("Global settings:");
    println!("  refresh_interval: {:?}", cfg.global.refresh_interval);
    println!("  colors: {}", cfg.global.colors);
    println!("  emoji: {}", cfg.global.emoji);
    println!(
        "  notification_cooldown: {:?}",
        cfg.global.notification_cooldown
    );
    if let Some(ref gp) = cfg.global.git_path {
        println!("  git_path: {}", gp);
    }
    println!();

    if !cfg.trusted_hosts.is_empty() {
        println!("Trusted hosts:");
        let mut hosts: Vec<&str> = cfg.trusted_hosts.iter().map(|s| s.as_str()).collect();
        hosts.sort();
        for host in hosts {
            println!("  {}", host);
        }
        println!();
    }

    if !cfg.repos.is_empty() {
        // If inside a repo, highlight the current one
        let current_repo = resolve_cwd_repo_path().ok();
        println!("Per-repo overrides:");
        for (path, repo_cfg) in &cfg.repos {
            let dp = display_path(path);
            let marker = if current_repo.as_deref() == Some(path.as_str()) {
                " ←"
            } else {
                ""
            };
            println!("  {}:{}", dp, marker);
            if let Some(d) = &repo_cfg.disabled {
                match d {
                    config::Disabled::All(true) => println!("    disabled: true"),
                    config::Disabled::Remotes(remotes) => {
                        println!("    disabled remotes: {:?}", remotes)
                    }
                    _ => {}
                }
            }
            if let Some(ri) = repo_cfg.refresh_interval {
                println!("    refresh_interval: {:?}", ri);
            }
        }
    }

    println!();
    println!("Config file: {}", paths.config_file.display());
    Ok(())
}

pub async fn handle_enable(paths: &Paths, remote: Option<String>, all: bool) -> Result<()> {
    let opts = load_display_opts(paths);
    let repo_path = resolve_cwd_repo_path()?;
    let repo_id = git_ops::discover_repo_id(Path::new(&repo_path))?;
    let repo_id_str = repo_id.to_string_lossy().to_string();
    let remote_urls = git_ops::get_all_remote_urls(&repo_id)?;

    // Determine what to enable
    let target_remote = resolve_remote_target(remote.as_deref(), all, &remote_urls, "enable")?;

    match target_remote {
        Some(name) => {
            // Enable a specific remote
            let cfg = UserConfig::load(paths)?;
            if cfg.is_repo_disabled(&repo_id_str) {
                bail!("repo is fully disabled — run `gitsitter enable --all` first");
            }
            let remote_name = name.to_string();
            UserConfig::update_repo(paths, &repo_id_str, move |entry| {
                if let Some(config::Disabled::Remotes(list)) = &mut entry.disabled {
                    list.retain(|r| r != &remote_name);
                    if list.is_empty() {
                        entry.disabled = Some(config::Disabled::All(false));
                    }
                }
            })?;
            notify_daemon_reload(paths).await;
            let icon = cli_ui::success_icon(opts);
            println!("{} Enabled remote '{}'", icon, name);
        }
        None => {
            // Enable whole repo
            UserConfig::update_repo(paths, &repo_id_str, |entry| {
                entry.disabled = Some(config::Disabled::All(false));
            })?;
            notify_daemon_reload(paths).await;
            let dp = display_path(&repo_id_str);
            let icon = cli_ui::success_icon(opts);
            println!("{} Enabled {}", icon, cli_ui::repo_header(&dp, opts));

            let cfg = UserConfig::load(paths)?;
            let daemon_running = transport::is_daemon_running(&paths.socket_path);
            cli_ui::print_daemon_warning(daemon_running, opts);
            print_untrusted_remote_warnings(&repo_id, &cfg, opts);
        }
    }
    Ok(())
}

pub async fn handle_resolve(paths: &Paths, global: bool) -> Result<()> {
    let opts = load_display_opts(paths);
    let mut stream = require_daemon(paths).await?;

    // Get status (current repo or global)
    let req = if global {
        Request::Status {
            repo_path: None,
            global: true,
        }
    } else {
        let repo_path = resolve_cwd_repo_path()?;
        Request::Status {
            repo_path: Some(repo_path),
            global: false,
        }
    };
    let resp = roundtrip(&mut stream, &req).await?;

    // Collect issues from one or many repos
    let status_list: Vec<transport::StatusData> = match resp {
        Response::Status { data } => vec![data],
        Response::GlobalStatus { repos } => {
            // For global, we need to fetch per-repo status for branch details
            let mut all = Vec::new();
            for repo in &repos {
                let req = Request::Status {
                    repo_path: Some(repo.display_path.clone()),
                    global: false,
                };
                if let Ok(resp) = roundtrip(&mut stream, &req).await
                    && let Response::Status { data } = resp
                {
                    all.push(data);
                }
            }
            all
        }
        _ => {
            println!("No sync data available.");
            return Ok(());
        }
    };

    let mut any_issues = false;

    for data in &status_list {
        let dp = display_path(&data.display_path);
        let repo_id_path = PathBuf::from(&data.repo_id);

        for b in &data.branches {
            let upstream = b.upstream.as_deref().unwrap_or("upstream");
            let action = match b.status.as_str() {
                "local_ahead" => "unpushed commits (last remote commit by someone else)",
                "diverged" => "diverged (last remote commit by someone else)",
                "diverged_yours" => "diverged (auto-rebase failed, resolve manually)",
                "pending_dirty" => "dirty worktree — commit or stash to sync",
                "merge_conflict" => {
                    "merge conflict — resolve manually or run `gitsitter auto-resolve`"
                }
                _ => continue,
            };

            any_issues = true;
            println!();
            println!(
                "{}: {} — {}",
                cli_ui::repo_header(&dp, opts),
                cli_ui::sync_pair(&b.name, upstream),
                action
            );

            match b.status.as_str() {
                "local_ahead" => {
                    println!("  [1] Push to remote");
                    println!("  [2] Create a new branch from your commits");
                    println!("  [3] Skip");
                    match prompt_choice(3) {
                        1 => {
                            let branch_remote = upstream.split('/').next().unwrap_or("origin");
                            let remote_ref =
                                upstream.split_once('/').map(|x| x.1).unwrap_or(&b.name);
                            match git_ops::git_push(
                                &repo_id_path,
                                branch_remote,
                                &b.name,
                                remote_ref,
                                None,
                                30,
                            )
                            .await
                            {
                                Ok(git_ops::PushResult::Success) => {
                                    println!("  {} Pushed {}", cli_ui::success_icon(opts), b.name);
                                }
                                _ => {
                                    println!("  Push failed. Resolve manually.");
                                }
                            }
                        }
                        2 => {
                            // Create new branch
                            let new_name = prompt_string("  Branch name: ");
                            if !new_name.is_empty() {
                                // Create branch at current HEAD, then reset original to upstream
                                let result = std::process::Command::new("git")
                                    .arg("-C")
                                    .arg(&data.repo_id)
                                    .args(["branch", &new_name])
                                    .output();
                                match result {
                                    Ok(o) if o.status.success() => {
                                        println!(
                                            "  {} Created branch {}",
                                            cli_ui::success_icon(opts),
                                            new_name
                                        );
                                        // Reset original branch to upstream
                                        if let Some(remote_oid) = &b.upstream {
                                            let _ = std::process::Command::new("git")
                                                .arg("-C")
                                                .arg(&data.repo_id)
                                                .args([
                                                    "update-ref",
                                                    &format!("refs/heads/{}", b.name),
                                                ])
                                                .arg(
                                                    remote_oid
                                                        .split('/')
                                                        .next_back()
                                                        .unwrap_or("HEAD"),
                                                )
                                                .output();
                                        }
                                    }
                                    _ => {
                                        println!("  Failed to create branch.");
                                    }
                                }
                            }
                        }
                        _ => {
                            println!("  Skipped.");
                        }
                    }
                }
                "diverged" | "diverged_yours" => {
                    println!("  [1] Rebase onto remote and push");
                    println!("  [2] Create a new branch from your commits");
                    println!("  [3] Skip");
                    match prompt_choice(3) {
                        1 => {
                            let branch_remote = upstream.split('/').next().unwrap_or("origin");
                            let remote_ref =
                                upstream.split_once('/').map(|x| x.1).unwrap_or(&b.name);
                            let upstream_ref = format!("{}/{}", branch_remote, remote_ref);
                            match git_ops::git_rebase(&repo_id_path, &upstream_ref, None, 30).await
                            {
                                Ok(true) => {
                                    // Rebase succeeded, now push
                                    match git_ops::git_push(
                                        &repo_id_path,
                                        branch_remote,
                                        &b.name,
                                        remote_ref,
                                        None,
                                        30,
                                    )
                                    .await
                                    {
                                        Ok(git_ops::PushResult::Success) => {
                                            println!(
                                                "  {} Rebased and pushed {}",
                                                cli_ui::success_icon(opts),
                                                b.name
                                            );
                                        }
                                        _ => {
                                            println!(
                                                "  Rebase succeeded but push failed. Resolve manually."
                                            );
                                        }
                                    }
                                }
                                Ok(false) => {
                                    let _ =
                                        git_ops::git_rebase_abort(&repo_id_path, None, 30).await;
                                    println!(
                                        "  Rebase had conflicts and was aborted. Resolve manually."
                                    );
                                }
                                Err(e) => {
                                    let _ =
                                        git_ops::git_rebase_abort(&repo_id_path, None, 30).await;
                                    println!("  Rebase failed: {:#}. Resolve manually.", e);
                                }
                            }
                        }
                        2 => {
                            let new_name = prompt_string("  Branch name: ");
                            if !new_name.is_empty() {
                                let result = std::process::Command::new("git")
                                    .arg("-C")
                                    .arg(&data.repo_id)
                                    .args(["branch", &new_name])
                                    .output();
                                match result {
                                    Ok(o) if o.status.success() => {
                                        println!(
                                            "  {} Created branch {}",
                                            cli_ui::success_icon(opts),
                                            new_name
                                        );
                                    }
                                    _ => {
                                        println!("  Failed to create branch.");
                                    }
                                }
                            }
                        }
                        _ => {
                            println!("  Skipped.");
                        }
                    }
                }
                "pending_dirty" => {
                    println!("  [1] Stash, sync, pop stash");
                    println!("  [2] Skip (will auto-sync when worktree is clean)");
                    match prompt_choice(2) {
                        1 => {
                            let branch_remote = upstream.split('/').next().unwrap_or("origin");
                            // Stash
                            match git_ops::git_stash(&repo_id_path, None, 30).await {
                                Ok(true) => {
                                    println!("  Stashed working changes.");
                                }
                                Ok(false) => {
                                    println!("  Nothing to stash — worktree already clean.");
                                    // Proceed anyway, daemon will pick up on next cycle
                                    continue;
                                }
                                Err(e) => {
                                    println!("  Stash failed: {:#}. Resolve manually.", e);
                                    continue;
                                }
                            }

                            // Determine what sync operation is needed
                            let upstream_ref = format!("{}/{}", branch_remote, b.name);
                            let sync_ok = match git_ops::analyze_merge(&repo_id_path, &b.name) {
                                Ok(git_ops::MergeAnalysis::FastForward) => {
                                    git_ops::git_ff_merge(&repo_id_path, &upstream_ref, None, 30)
                                        .await
                                        .is_ok()
                                }
                                Ok(git_ops::MergeAnalysis::Diverged) => {
                                    match git_ops::git_rebase(
                                        &repo_id_path,
                                        &upstream_ref,
                                        None,
                                        30,
                                    )
                                    .await
                                    {
                                        Ok(true) => true,
                                        _ => {
                                            let _ =
                                                git_ops::git_rebase_abort(&repo_id_path, None, 30)
                                                    .await;
                                            false
                                        }
                                    }
                                }
                                _ => {
                                    println!("  Branch is already up to date after stash.");
                                    true
                                }
                            };

                            // Pop stash
                            match git_ops::git_stash_pop(&repo_id_path, None, 30).await {
                                Ok(true) => {
                                    if sync_ok {
                                        println!(
                                            "  {} Synced and restored working changes",
                                            cli_ui::success_icon(opts)
                                        );
                                    } else {
                                        println!(
                                            "  Sync failed but working changes restored. Resolve manually."
                                        );
                                    }
                                }
                                Ok(false) => {
                                    if sync_ok {
                                        println!(
                                            "  Synced, but stash pop has conflicts — resolve with `git stash pop` or `git stash drop`"
                                        );
                                    } else {
                                        println!(
                                            "  Sync failed and stash pop has conflicts. Resolve manually."
                                        );
                                    }
                                }
                                Err(e) => {
                                    println!(
                                        "  Stash pop failed: {:#}. Your changes are in `git stash list`.",
                                        e
                                    );
                                }
                            }
                        }
                        _ => {
                            println!("  Skipped.");
                        }
                    }
                }
                "merge_conflict" => {
                    let cfg = UserConfig::load(paths).ok();
                    let has_agent = cfg
                        .as_ref()
                        .and_then(|c| c.global.resolve_agent.as_ref())
                        .is_some();

                    if has_agent {
                        println!("  [1] Run resolve agent");
                        println!("  [2] Abort rebase");
                        println!("  [3] Skip (resolve manually)");
                        match prompt_choice(3) {
                            1 => {
                                run_resolve_agent_interactive(
                                    &repo_id_path,
                                    cfg.as_ref().unwrap(),
                                    opts,
                                )
                                .await;
                            }
                            2 => {
                                let _ = git_ops::git_rebase_abort(&repo_id_path, None, 30).await;
                                println!("  Rebase aborted.");
                            }
                            _ => {
                                println!("  Skipped.");
                            }
                        }
                    } else {
                        println!("  [1] Abort rebase");
                        println!("  [2] Skip (resolve manually)");
                        match prompt_choice(2) {
                            1 => {
                                let _ = git_ops::git_rebase_abort(&repo_id_path, None, 30).await;
                                println!("  Rebase aborted.");
                            }
                            _ => {
                                println!("  Skipped.");
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    if !any_issues {
        println!("No issues to resolve. All branches are in sync.");
    }

    // Trigger a sync after resolving
    if any_issues {
        let _ = notify_daemon_reload(paths).await;
    }

    Ok(())
}

/// Shared code path for running the resolve agent — used by both `resolve` and `auto-resolve`.
async fn run_resolve_agent_interactive(repo_path: &Path, config: &UserConfig, opts: DisplayOpts) {
    let agent = config.global.resolve_agent.as_deref().unwrap_or("claude");
    let agent_path = config.global.resolve_agent_path.as_deref();

    println!("  Running resolve agent '{}'...", agent);
    match git_ops::run_resolve_agent(repo_path, agent, agent_path, 180).await {
        Ok(result) if result.completed => {
            println!(
                "  {} Conflicts resolved by agent",
                cli_ui::success_icon(opts)
            );
        }
        Ok(result) => {
            println!("  Agent did not fully resolve conflicts.");
            if !result.agent_output.is_empty() {
                // Surface last few lines of agent output
                for line in result
                    .agent_output
                    .lines()
                    .rev()
                    .take(5)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                {
                    println!("  | {}", line);
                }
            }
            println!("  Finish resolving manually, then `git rebase --continue`.");
        }
        Err(e) => {
            println!("  Resolve agent failed: {:#}", e);
            println!("  Conflicts remain — resolve manually or `git rebase --abort`.");
        }
    }
}

pub async fn handle_auto_resolve(paths: &Paths, agent_override: Option<String>) -> Result<()> {
    let opts = load_display_opts(paths);
    let config = UserConfig::load(paths)?;

    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let repo_id = git_ops::discover_repo_id(&cwd)?;

    // Check if there's an in-progress rebase
    if !git_ops::is_operation_in_progress(&repo_id) {
        println!("No rebase conflicts in progress.");
        return Ok(());
    }

    // Determine which agent to use
    let effective_config = if let Some(ref agent) = agent_override {
        let mut cfg = config.clone();
        cfg.global.resolve_agent = Some(agent.clone());
        cfg
    } else {
        config
    };

    if effective_config.global.resolve_agent.is_none() {
        bail!("no resolve agent configured. Set resolve_agent in config or pass --agent");
    }

    // Use the worktree path (cwd), not the .git dir
    run_resolve_agent_interactive(&cwd, &effective_config, opts).await;

    // Notify daemon to re-evaluate
    let _ = notify_daemon_reload(paths).await;

    Ok(())
}

fn prompt_choice(max: usize) -> usize {
    use std::io::{self, Write};
    loop {
        print!("  > ");
        io::stdout().flush().ok();
        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            return max; // default to skip on error
        }
        if let Ok(n) = input.trim().parse::<usize>()
            && n >= 1
            && n <= max
        {
            return n;
        }
        println!("  Please enter a number between 1 and {}", max);
    }
}

fn prompt_string(prompt: &str) -> String {
    use std::io::{self, Write};
    print!("{}", prompt);
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    input.trim().to_string()
}

pub async fn handle_disable(
    paths: &Paths,
    remote: Option<String>,
    all: bool,
    purge: bool,
) -> Result<()> {
    let opts = load_display_opts(paths);
    let repo_path = resolve_cwd_repo_path()?;
    let repo_id = git_ops::discover_repo_id(Path::new(&repo_path))?;
    let repo_id_str = repo_id.to_string_lossy().to_string();
    let remote_urls = git_ops::get_all_remote_urls(&repo_id)?;

    let target_remote = resolve_remote_target(remote.as_deref(), all, &remote_urls, "disable")?;

    match target_remote {
        Some(name) => {
            // Disable a specific remote
            let cfg = UserConfig::load(paths)?;
            if cfg.is_repo_disabled(&repo_id_str) {
                bail!("repo is already fully disabled");
            }
            let remote_name = name.to_string();
            UserConfig::update_repo(paths, &repo_id_str, move |entry| {
                if let Some(config::Disabled::Remotes(list)) = &mut entry.disabled {
                    if !list.contains(&remote_name) {
                        list.push(remote_name);
                    }
                } else {
                    entry.disabled = Some(config::Disabled::Remotes(vec![remote_name]));
                }
            })?;
            notify_daemon_reload(paths).await;
            let icon = cli_ui::pause_icon(opts);
            println!("{} Disabled remote '{}'", icon, name);
        }
        None => {
            // Disable whole repo
            if purge {
                UserConfig::remove_repo(paths, &repo_id_str)?;
            } else {
                UserConfig::update_repo(paths, &repo_id_str, |entry| {
                    entry.disabled = Some(config::Disabled::All(true));
                })?;
            }
            notify_daemon_reload(paths).await;
            let dp = display_path(&repo_id_str);
            let icon = cli_ui::pause_icon(opts);
            println!("{} Disabled {}", icon, cli_ui::repo_header(&dp, opts));
        }
    }
    Ok(())
}

pub async fn handle_trust(paths: &Paths, host: &str) -> Result<()> {
    let opts = load_display_opts(paths);
    UserConfig::trust(paths, host)?;
    notify_daemon_reload(paths).await;

    let icon = cli_ui::success_icon(opts);
    println!("{} Trusted host '{}'", icon, host);
    Ok(())
}

pub async fn handle_untrust(paths: &Paths, host: &str) -> Result<()> {
    let opts = load_display_opts(paths);
    UserConfig::untrust(paths, host)?;
    notify_daemon_reload(paths).await;

    let icon = cli_ui::pause_icon(opts);
    println!("{} Untrusted host '{}'", icon, host);
    Ok(())
}

pub async fn handle_log(
    paths: &Paths,
    global: bool,
    follow: bool,
    path: Option<String>,
) -> Result<()> {
    let log_dir = paths
        .daemon_log
        .parent()
        .context("daemon_log has no parent")?;

    // Find all daemon.log files (current + rotated daily files)
    let mut log_files: Vec<_> = std::fs::read_dir(log_dir)
        .context("failed to read log directory")?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("daemon.log"))
        })
        .collect();

    if log_files.is_empty() {
        println!("No log files found in {}", log_dir.display());
        return Ok(());
    }

    log_files.sort();

    // Resolve repo filter: --global skips filtering, otherwise try to find .git
    let repo_filter = if global {
        None
    } else {
        let target = path
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        match git_ops::discover_repo_id(&target) {
            Ok(repo_id) => {
                let label = repo_id
                    .to_string_lossy()
                    .trim_end_matches(['/', '\\'])
                    .to_string();
                Some(label)
            }
            Err(_) => None, // not in a repo — fall back to global
        }
    };

    if follow {
        let latest = log_files.last().unwrap();
        if let Some(ref filter) = repo_filter {
            // tail -f with grep filtering
            let tail = std::process::Command::new("tail")
                .args(["-f", &latest.to_string_lossy()])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .context("failed to run tail")?;
            let status = std::process::Command::new("grep")
                .args(["--line-buffered", filter])
                .stdin(tail.stdout.unwrap())
                .status()
                .context("failed to run grep")?;
            if !status.success() {
                // grep exits 1 on no match / interrupt, that's fine
            }
        } else {
            let status = std::process::Command::new("tail")
                .args(["-f", &latest.to_string_lossy()])
                .status()
                .context("failed to run tail")?;
            if !status.success() {
                bail!("tail exited with {}", status);
            }
        }
    } else if let Some(ref filter) = repo_filter {
        // cat all log files, grep for repo, pipe to less
        let cat = std::process::Command::new("cat")
            .args(log_files.iter().map(|p| p.as_os_str()))
            .stdout(std::process::Stdio::piped())
            .spawn()
            .context("failed to run cat")?;
        let grep = std::process::Command::new("grep")
            .arg(filter)
            .stdin(cat.stdout.unwrap())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .context("failed to run grep")?;
        let status = std::process::Command::new("less")
            .args(["-Rf", "+G"])
            .stdin(grep.stdout.unwrap())
            .status()
            .context("failed to run less")?;
        if !status.success() && status.code() != Some(1) {
            bail!("less exited with {}", status);
        }
    } else {
        // global: pipe all log files into less
        let status = std::process::Command::new("less")
            .args(["-Rf", "+G"])
            .args(log_files.iter().map(|p| p.as_os_str()))
            .status()
            .context("failed to run less")?;
        if !status.success() && status.code() != Some(1) {
            bail!("less exited with {}", status);
        }
    }

    Ok(())
}

pub async fn handle_sync(paths: &Paths, all: bool) -> Result<()> {
    let opts = load_display_opts(paths);
    let mut stream = require_daemon(paths).await?;
    let repo_path = if all {
        None
    } else {
        Some(resolve_cwd_repo_path()?)
    };
    let display = repo_path.as_ref().map(|p| display_path(p));
    let req = Request::Sync { repo_path, all };
    let resp = roundtrip(&mut stream, &req).await?;
    match resp {
        Response::Ok { .. } => {
            let icon = cli_ui::success_icon(opts);
            if let Some(dp) = display {
                println!("{} Synced {}", icon, cli_ui::repo_header(&dp, opts));
            } else {
                println!("{} Synced all repos", icon);
            }
        }
        Response::Error { message } => eprintln!("error: {}", message),
        _ => eprintln!("unexpected response"),
    }
    Ok(())
}

pub async fn handle_register(paths: &Paths, path: Option<String>) -> Result<()> {
    let opts = load_display_opts(paths);
    let repo_path = resolve_path_or_cwd(path.as_deref())?;
    let repo_id =
        git_ops::discover_repo_id(Path::new(&repo_path)).context("not a git repository")?;
    let repo_id_str = repo_id.to_string_lossy().to_string();

    UserConfig::update_repo(paths, &repo_id_str, |_| {})?;
    notify_daemon_reload(paths).await;

    let dp = display_path(&repo_id_str);
    let icon = cli_ui::celebrate_icon(opts);
    println!("{} Registered {}", icon, cli_ui::repo_header(&dp, opts));

    let cfg = UserConfig::load(paths)?;
    let daemon_running = transport::is_daemon_running(&paths.socket_path);
    cli_ui::print_daemon_warning(daemon_running, opts);
    print_untrusted_remote_warnings(&repo_id, &cfg, opts);

    Ok(())
}

pub async fn handle_daemon_status(paths: &Paths) -> Result<()> {
    let opts = load_display_opts(paths);
    match transport::connect_to_daemon(&paths.socket_path).await {
        Ok(mut stream) => {
            let req = Request::DaemonStatus;
            let resp = roundtrip(&mut stream, &req).await?;
            match resp {
                Response::DaemonStatus {
                    pid,
                    uptime_secs,
                    repos_watched,
                    latest_version,
                } => {
                    let icon = cli_ui::success_icon(opts);
                    println!("{} Daemon is running", icon);
                    println!("  PID:           {}", pid);
                    println!("  Uptime:        {}", format_uptime(uptime_secs));
                    println!("  Repos watched: {}", repos_watched);
                    if let Some(v) = latest_version {
                        println!();
                        println!(
                            "  Update available: {v} (current: v{})",
                            crate::self_update::current_version()
                        );
                        println!("  Run 'gitsitter self-update' to update.");
                    }
                }
                Response::Error { message } => {
                    eprintln!("error: {}", message);
                }
                _ => {
                    eprintln!("unexpected response from daemon");
                }
            }
        }
        Err(_) => {
            println!("\u{00B7} Daemon is not running");
        }
    }
    Ok(())
}

fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        return format!("{}s", secs);
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{}m {}s", mins, secs % 60);
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{}h {}m", hours, mins % 60);
    }
    let days = hours / 24;
    format!("{}d {}h", days, hours % 24)
}

async fn wait_for_daemon_ready(paths: &Paths, timeout: std::time::Duration) -> bool {
    let start = std::time::Instant::now();
    let poll_interval = std::time::Duration::from_millis(50);

    while start.elapsed() < timeout {
        if transport::is_daemon_running(&paths.socket_path) {
            return true;
        }
        tokio::time::sleep(poll_interval).await;
    }

    false
}

#[cfg(windows)]
async fn sc_command(args: &[&str]) -> Result<std::process::Output> {
    tokio::process::Command::new("sc.exe")
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to run sc.exe {}", args.join(" ")))
}

pub async fn handle_daemon_start(paths: &Paths) -> Result<()> {
    // Check if already running
    if transport::is_daemon_running(&paths.socket_path) {
        println!("\u{00B7} Daemon is already running");
        return Ok(());
    }

    // Try platform service manager first
    #[cfg(target_os = "macos")]
    {
        let plist_path =
            dirs::home_dir().map(|h| h.join("Library/LaunchAgents/com.gitsitter.daemon.plist"));
        if let Some(ref p) = plist_path
            && p.exists()
        {
            let domain_target = format!("gui/{}", unsafe { libc::getuid() });
            let result = tokio::process::Command::new("launchctl")
                .args(["bootstrap", &domain_target, &p.display().to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .status()
                .await;
            if let Ok(status) = result
                && status.success()
            {
                if wait_for_daemon_ready(paths, std::time::Duration::from_secs(3)).await {
                    println!("\u{2713} Daemon started via launchd");
                    return Ok(());
                }
                bail!("launchd accepted the job, but the daemon socket did not appear");
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        let systemd_result = tokio::process::Command::new("systemctl")
            .args(["--user", "start", "gitsitter"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .status()
            .await;

        match systemd_result {
            Ok(status) if status.success() => {
                if wait_for_daemon_ready(paths, std::time::Duration::from_secs(3)).await {
                    println!("\u{2713} Daemon started via systemd");
                    return Ok(());
                }
                bail!("systemd accepted the job, but the daemon socket did not appear");
            }
            _ => {
                // systemd not available or unit not installed -- spawn directly
            }
        }
    }

    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "linux")))]
    {
        eprintln!(
            "warning: no supported Unix service manager configured on this platform; starting detached daemon directly"
        );
    }

    #[cfg(windows)]
    {
        let service_result = sc_command(&["start", crate::service::SERVICE_NAME]).await;
        match service_result {
            Ok(output) if output.status.success() => {
                println!("\u{2713} Daemon started via Windows Service Control Manager");
                return Ok(());
            }
            Ok(_) | Err(_) => {
                // Service not installed or SCM unavailable -- spawn directly.
            }
        }
    }

    // Spawn self as a detached background process
    let exe = std::env::current_exe().context("failed to determine own executable path")?;
    paths.ensure_dirs()?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["daemon", "run"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }
    let child = cmd.spawn().context("failed to spawn daemon process")?;

    if wait_for_daemon_ready(paths, std::time::Duration::from_secs(3)).await {
        println!("\u{2713} Daemon started, PID: {}", child.id());
        return Ok(());
    }

    bail!(
        "spawned daemon process (PID: {}), but the daemon socket did not appear; check {}",
        child.id(),
        paths.daemon_log.clone().display()
    )
}

pub async fn handle_daemon_stop(paths: &Paths) -> Result<()> {
    #[cfg(windows)]
    {
        let service_result = sc_command(&["stop", crate::service::SERVICE_NAME]).await;
        if let Ok(output) = service_result
            && output.status.success()
        {
            println!("\u{2713} Daemon stopped");
            return Ok(());
        }
    }

    match transport::connect_to_daemon(&paths.socket_path).await {
        Ok(mut stream) => {
            let req = Request::Shutdown;
            let resp = roundtrip(&mut stream, &req).await;
            match resp {
                Ok(Response::Ok { .. }) | Err(_) => println!("\u{2713} Daemon stopped"),
                Ok(Response::Error { message }) => eprintln!("error: {}", message),
                _ => println!("\u{2713} Daemon stopped"),
            }
        }
        Err(_) => {
            println!("\u{00B7} Daemon is not running");
        }
    }
    Ok(())
}

pub async fn handle_daemon_restart(paths: &Paths) -> Result<()> {
    // Stop if running
    if transport::is_daemon_running(&paths.socket_path)
        && let Ok(mut stream) = transport::connect_to_daemon(&paths.socket_path).await
    {
        let req = Request::Shutdown;
        let _ = roundtrip(&mut stream, &req).await;
        // Give it a moment to shut down
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    // Start
    handle_daemon_start(paths).await
}

pub async fn handle_daemon_run(paths: &Paths) -> Result<()> {
    crate::daemon::run_daemon(paths).await
}

pub async fn handle_daemon_service() -> Result<()> {
    crate::service::run_service_dispatcher()
}

/// Build a PATH string for the daemon service by discovering tool locations.
/// Keep in sync with nix/home-manager-module.nix (daemonPathPackages).
#[cfg(unix)]
fn build_daemon_path() -> String {
    let mut dirs = Vec::new();
    for tool in ["git", "ssh", "gh", "claude"] {
        if let Ok(output) = std::process::Command::new("which").arg(tool).output()
            && output.status.success()
        {
            let stdout = std::str::from_utf8(&output.stdout).unwrap_or("");
            let line = stdout.lines().next().unwrap_or("").trim();
            if let Some(dir) = std::path::Path::new(line).parent() {
                let d = dir.to_string_lossy().to_string();
                if !dirs.contains(&d) {
                    dirs.push(d);
                }
            }
        }
    }
    if dirs.is_empty() {
        return "/usr/bin:/usr/local/bin".to_string();
    }
    dirs.join(":")
}

async fn install_daemon() -> Result<()> {
    let exe = std::env::current_exe()?;
    #[cfg(unix)]
    let daemon_path = build_daemon_path();
    let state_dir = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .context("cannot determine state directory")?
        .join("gitsitter");
    std::fs::create_dir_all(&state_dir)?;

    #[cfg(target_os = "macos")]
    {
        let launch_agents = dirs::home_dir()
            .context("cannot determine home directory")?
            .join("Library/LaunchAgents");
        std::fs::create_dir_all(&launch_agents)?;
        let plist = include_str!("embed/com.gitsitter.daemon.plist")
            .replace("@@EXEC_PATH@@", &exe.display().to_string())
            .replace("@@LOG_DIR@@", &state_dir.display().to_string())
            .replace("@@DAEMON_PATH@@", &daemon_path);
        let plist_path = launch_agents.join("com.gitsitter.daemon.plist");
        std::fs::write(&plist_path, &plist)?;
        println!("launchd plist written to {}", plist_path.display());
        println!("Daemon PATH: {daemon_path}");
        // Bootstrap the service
        let uid = unsafe { libc::getuid() };
        let domain = format!("gui/{uid}");
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &domain, &plist_path.display().to_string()])
            .output();
        let output = std::process::Command::new("launchctl")
            .args(["bootstrap", &domain, &plist_path.display().to_string()])
            .output()?;
        if output.status.success() {
            println!("launchd service bootstrapped");
        } else {
            println!(
                "launchctl bootstrap failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            println!(
                "Run manually: launchctl bootstrap {domain} {}",
                plist_path.display()
            );
        }
    }

    #[cfg(target_os = "linux")]
    {
        let unit_dir = dirs::home_dir()
            .context("cannot determine home directory")?
            .join(".config/systemd/user");
        std::fs::create_dir_all(&unit_dir)?;
        let unit = include_str!("embed/gitsitter.service")
            .replace("@@EXEC_PATH@@", &exe.display().to_string())
            .replace("@@DAEMON_PATH@@", &daemon_path);
        let unit_path = unit_dir.join("gitsitter.service");
        std::fs::write(&unit_path, &unit)?;
        println!("Systemd user service written to {}", unit_path.display());
        println!("Daemon PATH: {daemon_path}");
        let output = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        if let Ok(o) = output
            && o.status.success()
        {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "enable", "--now", "gitsitter"])
                .output();
            println!("systemd service enabled and started");
        }
    }

    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "linux")))]
    {
        bail!("daemon service installation is not implemented for this Unix platform");
    }

    #[cfg(windows)]
    {
        let bin_path = format!("\"{}\" daemon service", exe.display());
        let output = sc_command(&[
            "create",
            crate::service::SERVICE_NAME,
            &format!("DisplayName= {}", crate::service::SERVICE_DISPLAY_NAME),
            &format!("binPath= {}", bin_path),
            "start= auto",
            "type= own",
        ])
        .await?;
        if !output.status.success() {
            bail!(
                "failed to create Windows service: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        println!(
            "Windows service '{}' installed",
            crate::service::SERVICE_DISPLAY_NAME
        );
        println!("Run: sc.exe start {}", crate::service::SERVICE_NAME);
    }

    Ok(())
}

pub async fn handle_install(component: Option<String>, shell_name: Option<String>) -> Result<()> {
    let comp = component.as_deref().unwrap_or("all");
    match comp {
        "shell" | "hooks" => {
            let sh = match &shell_name {
                Some(s) => s.clone(),
                None => crate::shell::detect_shell().context(
                    "could not detect shell, specify with: gitsitter install shell <name>",
                )?,
            };
            crate::shell::install_hook(&sh)?;
            println!("Shell hook installed for {sh}");
        }
        "daemon" => {
            install_daemon().await?;
        }
        "all" => {
            let sh = match &shell_name {
                Some(s) => s.clone(),
                None => crate::shell::detect_shell().context(
                    "could not detect shell, specify with: gitsitter install shell <name>",
                )?,
            };
            crate::shell::install_hook(&sh)?;
            println!("Shell hook installed for {sh}");
            println!();
            install_daemon().await?;
        }
        _ => bail!(
            "unknown component: {}. Use 'shell', 'daemon', or 'all'",
            comp
        ),
    }
    Ok(())
}

async fn uninstall_daemon() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let plist_path = dirs::home_dir()
            .context("cannot determine home directory")?
            .join("Library/LaunchAgents/com.gitsitter.daemon.plist");
        if plist_path.exists() {
            let uid = unsafe { libc::getuid() };
            let domain = format!("gui/{uid}");
            let output = std::process::Command::new("launchctl")
                .args(["bootout", &domain, &plist_path.display().to_string()])
                .output();
            match output {
                Ok(o) if o.status.success() => println!("launchd service stopped"),
                Ok(o) => println!(
                    "launchctl bootout: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                Err(e) => println!("failed to run launchctl: {e}"),
            }
            std::fs::remove_file(&plist_path)?;
            println!("launchd plist removed");
        } else {
            println!("No launchd plist found");
        }
    }

    #[cfg(target_os = "linux")]
    {
        let unit_path = dirs::home_dir()
            .context("cannot determine home directory")?
            .join(".config/systemd/user/gitsitter.service");
        if unit_path.exists() {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "disable", "--now", "gitsitter"])
                .output();
            println!("systemd service stopped and disabled");
            std::fs::remove_file(&unit_path)?;
            println!("Systemd service file removed");
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .output();
        } else {
            println!("No systemd service file found");
        }
    }

    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "linux")))]
    {
        bail!("daemon service uninstall is not implemented for this Unix platform");
    }

    #[cfg(windows)]
    {
        let output = sc_command(&["delete", crate::service::SERVICE_NAME]).await?;
        if output.status.success() {
            println!("Windows service removed");
        } else {
            println!(
                "Windows service removal failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }

    Ok(())
}

pub async fn handle_uninstall(component: Option<String>) -> Result<()> {
    let comp = component.as_deref().unwrap_or("all");
    match comp {
        "shell" | "hooks" => {
            let sh = crate::shell::detect_shell().unwrap_or_else(|| "bash".to_string());
            crate::shell::uninstall_hook(&sh)?;
            println!("Shell hook removed for {sh}");
        }
        "daemon" => {
            uninstall_daemon().await?;
        }
        "all" => {
            let sh = crate::shell::detect_shell().unwrap_or_else(|| "bash".to_string());
            crate::shell::uninstall_hook(&sh)?;
            println!("Shell hook removed for {sh}");
            println!();
            uninstall_daemon().await?;
        }
        _ => bail!(
            "unknown component: {}. Use 'shell', 'daemon', or 'all'",
            comp
        ),
    }
    Ok(())
}

/// Hidden command used by shell hooks to check for notifications.
pub async fn handle_prompt(paths: &Paths) -> Result<()> {
    // Quick check: try to connect to daemon with short timeout
    let connect = tokio::time::timeout(
        std::time::Duration::from_millis(20),
        transport::connect_to_daemon(&paths.socket_path),
    )
    .await;

    let mut stream = match connect {
        Ok(Ok(s)) => s,
        _ => return Ok(()), // silently skip if daemon not available
    };

    let repo_path = match resolve_cwd_repo_path() {
        Ok(p) => p,
        Err(_) => return Ok(()), // not in a git repo
    };

    // Use PromptCheck to register + get status in a single daemon call,
    // eliminating the need for a separate `gitsitter register` process.
    let req = Request::PromptCheck { repo_path };

    let resp = match tokio::time::timeout(
        std::time::Duration::from_millis(50),
        roundtrip(&mut stream, &req),
    )
    .await
    {
        Ok(Ok(r)) => r,
        _ => return Ok(()),
    };

    if let Response::Status { data } = resp {
        let dp = display_path(&data.display_path);

        if data.newly_registered {
            let remote_names: Vec<&str> = data.remote_urls.keys().map(|s| s.as_str()).collect();
            if remote_names.is_empty() {
                println!(
                    "gitsitter: Registered repo \u{1F4E6} {}, syncing tracking branches",
                    dp
                );
            } else {
                println!(
                    "gitsitter: Registered repo \u{1F4E6} {}, syncing tracking branches on {}",
                    dp,
                    remote_names.join(", ")
                );
            }
            for name in &data.untrusted_remotes {
                let url = data
                    .remote_urls
                    .get(name)
                    .map(|s| s.as_str())
                    .unwrap_or("?");
                let host = crate::config::extract_host(url).unwrap_or_else(|| url.to_string());
                println!(
                    "gitsitter: \u{26A0}\u{FE0F} remote '{}' at {} not trusted \u{2014} run 'gitsitter trust {}' to enable syncing",
                    name, url, host,
                );
            }
        }

        let mut issues = Vec::new();

        for b in &data.branches {
            let upstream = b.upstream.as_deref().unwrap_or("upstream");
            let pair = cli_ui::sync_pair(&b.name, upstream);
            match b.status.as_str() {
                "local_ahead" => {
                    issues.push(format!(
                        "gitsitter: \u{1F4E6} {} {} has unpushed changes (last remote commit by someone else)",
                        dp, pair
                    ));
                }
                "diverged" => {
                    issues.push(format!(
                        "gitsitter: \u{1F4E6} {} {} has diverged (last remote commit by someone else)",
                        dp, pair
                    ));
                }
                "diverged_yours" => {
                    issues.push(format!(
                        "gitsitter: \u{1F4E6} {} {} has diverged (auto-rebase failed, resolve manually)",
                        dp, pair
                    ));
                }
                "pending_dirty" => {
                    issues.push(format!(
                        "gitsitter: \u{270F}\u{FE0F} {} {} dirty worktree \u{2014} commit or stash to sync",
                        dp, pair
                    ));
                }
                "merge_conflict" => {
                    issues.push(format!(
                        "gitsitter: \u{1F527} {} {} has merge conflicts \u{2014} run `gitsitter auto-resolve` or resolve manually",
                        dp, pair
                    ));
                }
                "push_rejected" | "auth_failed" | "network_error" | "push_blocked_hook_timeout" => {
                    issues.push(format!(
                        "gitsitter: \u{1F4E6} {} {} sync error ({})",
                        dp,
                        pair,
                        b.status.replace('_', " ")
                    ));
                }
                _ => continue,
            }
        }

        if !issues.is_empty() {
            for issue in &issues {
                println!("{}", issue);
            }
            println!("gitsitter: Run `gitsitter resolve` to resolve issues");
        }
    }
    Ok(())
}
