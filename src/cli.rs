//! CLI command handlers.
//!
//! Each function corresponds to a CLI subcommand. They connect to the daemon
//! via Unix socket, send a request, and print the response. When the daemon is
//! down, some commands fall back to reading directly from SQLite/TOML.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::config::{self, RepoSyncMode, UserConfig};
use crate::git_ops;
use crate::paths;
use crate::state::StateDb;
use crate::transport::{
    self, connect_to_daemon, Request, Response,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
    let parsed = DateTime::parse_from_rfc3339(timestamp)
        .or_else(|_| {
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
    let repo_id = git_ops::discover_repo_id(&cwd)
        .context("not inside a git repository")?;
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
    let canonical = p.canonicalize()
        .with_context(|| format!("path does not exist: {}", p.display()))?;
    Ok(canonical.to_string_lossy().to_string())
}

/// Send a request and receive a single response over a connected stream.
async fn roundtrip<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    req: &Request,
) -> Result<Response> {
    transport::send_request(stream, req).await?;
    transport::recv_response(stream).await
}

/// Status icon for a branch sync status string.
fn status_icon(status: &str) -> &'static str {
    match status {
        "synced" | "up_to_date" => "\u{2705}", // green check
        "local_ahead" => "\u{2B06}\u{FE0F}",   // up arrow
        "fast_forward" | "remote_ahead" => "\u{2B07}\u{FE0F}", // down arrow
        "diverged" => "\u{26A0}\u{FE0F}",      // warning
        "error" => "\u{274C}",                  // red X
        _ => "\u{2753}",                        // question mark
    }
}

/// Human-readable label for a branch sync status string.
fn status_label(status: &str) -> &'static str {
    match status {
        "synced" | "up_to_date" => "synced",
        "local_ahead" => "local ahead",
        "fast_forward" | "remote_ahead" => "remote ahead",
        "diverged" => "diverged (ff not possible)",
        "error" => "error",
        "unknown" => "unknown",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn handle_status(global: bool) -> Result<()> {
    // Try daemon first
    match connect_to_daemon().await {
        Ok(mut stream) => {
            let repo_path = if global {
                None
            } else {
                Some(resolve_cwd_repo_path()?)
            };
            let req = Request::Status {
                repo_path: repo_path.clone(),
                global,
            };
            let resp = roundtrip(&mut stream, &req).await?;
            match resp {
                Response::Status { data } => {
                    print_repo_status(&data);
                }
                Response::GlobalStatus { repos } => {
                    if repos.is_empty() {
                        println!("No repos being watched.");
                    } else {
                        for r in &repos {
                            let dp = display_path(&r.display_path);
                            let sync_info = match &r.last_sync {
                                Some(ts) => format!("synced {}", format_relative_time(ts)),
                                None => "never synced".to_string(),
                            };
                            println!(
                                "\u{1F4E6} {}  ({}, {})",
                                dp, r.mode, sync_info
                            );
                            println!("    {}", r.status_summary);
                            println!();
                        }
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
            // Daemon not running -- fall back to SQLite
            eprintln!("warning: daemon not running, showing cached state");
            eprintln!();
            fallback_status(global)?;
        }
    }
    Ok(())
}

fn print_repo_status(data: &transport::StatusData) {
    let dp = display_path(&data.display_path);
    let sync_info = match &data.last_sync {
        Some(ts) => format!("synced {}", format_relative_time(ts)),
        None => "never synced".to_string(),
    };
    println!(
        "\u{1F4E6} {}  ({}, {})",
        dp, data.mode, sync_info
    );
    println!();
    for b in &data.branches {
        let upstream = match &b.upstream {
            Some(u) => format!("\u{2190} {}", u),
            None => "(no upstream)".to_string(),
        };
        let icon = status_icon(&b.status);
        let label = status_label(&b.status);
        let action = match &b.last_action {
            Some(a) => format!(" ({})", a),
            None => String::new(),
        };
        println!(
            "  {:<16} {:<28} {} {}{}",
            b.name, upstream, icon, label, action
        );
    }
}

fn fallback_status(global: bool) -> Result<()> {
    let db = StateDb::open()?;
    let config = config::UserConfig::load()?;

    if global {
        let repos = crate::queries::build_global_status(&db, &config)?;
        if repos.is_empty() {
            println!("No repos registered.");
            return Ok(());
        }
        for r in &repos {
            let dp = display_path(&r.display_path);
            let sync_info = match &r.last_sync {
                Some(ts) => format!("synced {}", format_relative_time(ts)),
                None => "never synced".to_string(),
            };
            println!(
                "\u{1F4E6} {}  ({}, {})",
                dp, r.mode, sync_info
            );
            println!("    {}", r.status_summary);
            println!();
        }
    } else {
        let repo_path = resolve_cwd_repo_path()?;
        match crate::queries::build_repo_status(&db, &config, &repo_path) {
            Ok(data) => print_repo_status(&data),
            Err(_) => {
                println!("This repo is not registered with gitsitter.");
                println!("Run `gitsitter enable` or `gitsitter register` to add it.");
            }
        }
    }
    Ok(())
}

pub async fn handle_config(
    global: bool,
    repo: Option<String>,
    branch: Option<String>,
    explain: bool,
) -> Result<()> {
    if explain {
        return handle_config_explain().await;
    }

    // If setting repo or branch mode, try daemon first, fall back to direct TOML edit
    if repo.is_some() || branch.is_some() {
        let repo_path = resolve_cwd_repo_path()?;

        // Try daemon
        if let Ok(mut stream) = connect_to_daemon().await {
            // Send the config update, daemon will re-read TOML
            // But first write the TOML change locally
            write_config_change(&repo_path, repo.as_deref(), branch.as_deref())?;
            let req = Request::ConfigUpdate {
                repo_path: Some(repo_path),
            };
            let resp = roundtrip(&mut stream, &req).await?;
            match resp {
                Response::Ok { message } => println!("{}", message),
                Response::Error { message } => eprintln!("error: {}", message),
                _ => eprintln!("unexpected response"),
            }
        } else {
            // Direct TOML edit
            write_config_change(&repo_path, repo.as_deref(), branch.as_deref())?;
            println!("Config updated (daemon not running, change will take effect on next start).");
        }
        return Ok(());
    }

    // No flags: just print current config summary
    if global {
        print_global_config()?;
    } else {
        print_repo_config()?;
    }
    Ok(())
}

fn write_config_change(
    repo_path: &str,
    repo_mode: Option<&str>,
    branch_mode: Option<&str>,
) -> Result<()> {
    let mut cfg = UserConfig::load()?;

    if let Some(mode_str) = repo_mode {
        let mode: RepoSyncMode = serde_json::from_value(serde_json::Value::String(mode_str.to_string()))
            .with_context(|| format!("invalid repo sync mode: {}", mode_str))?;
        let entry = cfg.repos.entry(repo_path.to_string()).or_default();
        entry.mode = Some(mode);
    }

    if let Some(mode_str) = branch_mode {
        let branch_mode: config::BranchSyncMode =
            serde_json::from_value(serde_json::Value::String(mode_str.to_string()))
                .with_context(|| format!("invalid branch sync mode: {}", mode_str))?;
        // Parse "branch_name=mode" or just set default
        // For now, we expect --branch to be a mode that applies to the current branch
        let cwd = std::env::current_dir()?;
        let repo_id_path = git_ops::discover_repo_id(&cwd)?;
        let repo = git2::Repository::open(&repo_id_path)?;
        let head = repo.head()?;
        let branch_name = head.shorthand().unwrap_or("HEAD").to_string();

        let entry = cfg.repos.entry(repo_path.to_string()).or_default();
        // Check if branch already in list
        let mut found = false;
        for (name, mode) in &mut entry.branches {
            if name == &branch_name {
                *mode = branch_mode;
                found = true;
                break;
            }
        }
        if !found {
            entry.branches.push((branch_name.clone(), branch_mode));
        }
        println!("Set branch '{}' to {:?}", branch_name, branch_mode);
    }

    cfg.save()?;
    Ok(())
}

fn print_global_config() -> Result<()> {
    let cfg = UserConfig::load()?;
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
        for (host, trusted) in &cfg.trusted_hosts {
            println!(
                "  {}: {}",
                host,
                if *trusted { "trusted" } else { "untrusted" }
            );
        }
        println!();
    }
    if !cfg.repos.is_empty() {
        println!("Per-repo overrides:");
        for (path, repo_cfg) in &cfg.repos {
            let dp = display_path(path);
            println!("  {}:", dp);
            if let Some(mode) = repo_cfg.mode {
                println!("    mode: {:?}", mode);
            }
            if repo_cfg.disabled == Some(true) {
                println!("    disabled: true");
            }
            if let Some(ri) = repo_cfg.refresh_interval {
                println!("    refresh_interval: {:?}", ri);
            }
            if !repo_cfg.branches.is_empty() {
                println!("    branches:");
                for (pat, mode) in &repo_cfg.branches {
                    println!("      {}: {:?}", pat, mode);
                }
            }
        }
    }
    Ok(())
}

fn print_repo_config() -> Result<()> {
    let cfg = UserConfig::load()?;
    let repo_path = match resolve_cwd_repo_path() {
        Ok(p) => p,
        Err(_) => {
            println!("Not inside a git repository. Use --global to see global config.");
            return Ok(());
        }
    };
    let dp = display_path(&repo_path);
    println!("Config for {}:", dp);

    if let Some(repo_cfg) = cfg.repos.get(&repo_path) {
        if let Some(mode) = repo_cfg.mode {
            println!("  mode: {:?}", mode);
        } else {
            println!("  mode: (inherited default)");
        }
        if repo_cfg.disabled == Some(true) {
            println!("  disabled: true");
        }
        if let Some(ri) = repo_cfg.refresh_interval {
            println!("  refresh_interval: {:?}", ri);
        }
        if !repo_cfg.branches.is_empty() {
            println!("  branches:");
            for (pat, mode) in &repo_cfg.branches {
                println!("    {}: {:?}", pat, mode);
            }
        }
    } else {
        println!("  (no per-repo overrides, using defaults)");
    }
    Ok(())
}

async fn handle_config_explain() -> Result<()> {
    let cfg = UserConfig::load()?;
    let repo_path = resolve_cwd_repo_path()?;
    let dp = display_path(&repo_path);

    let repo_id_path = PathBuf::from(&repo_path);
    let remote_url = git_ops::get_remote_url(&repo_id_path)?
        .unwrap_or_default();
    let in_repo = crate::queries::load_in_repo_config(&repo_id_path)?;

    let repo_mode = cfg.resolve_repo_mode(
        &remote_url,
        &repo_path,
        in_repo.as_ref(),
    );

    println!("Config resolution for {}:", dp);
    println!();
    println!("  Remote URL: {}", if remote_url.is_empty() { "(none)" } else { &remote_url });
    println!();

    // Show resolution chain for repo mode
    println!("  Repo sync mode: {:?}", repo_mode);
    println!("  Resolution chain:");

    // Check host trust
    if let Some(host) = config::extract_host(&remote_url) {
        let trusted = cfg.is_host_trusted(&host);
        println!("    1. Host trust ({host}): {}", if trusted { "trusted" } else { "UNTRUSTED -> None" });
        if !trusted {
            return Ok(());
        }
    }

    // Check per-repo
    if let Some(repo_cfg) = cfg.repos.get(&repo_path) {
        if repo_cfg.disabled == Some(true) {
            println!("    2. User config per-repo: disabled");
            return Ok(());
        }
        if let Some(mode) = repo_cfg.mode {
            println!("    2. User config per-repo: {:?} <-- winner", mode);
        } else {
            println!("    2. User config per-repo: (not set)");
        }
    } else {
        println!("    2. User config per-repo: (not set)");
    }

    // Check in-repo
    match &in_repo {
        Some(irc) => {
            if let Some(mode) = irc.mode {
                println!("    3. .gitsitter.toml: {:?}", mode);
            } else {
                println!("    3. .gitsitter.toml: (not set)");
            }
        }
        None => {
            println!("    3. .gitsitter.toml: (not present)");
        }
    }

    // Check defaults.remotes
    let mut matched_default = false;
    for (pattern, mode) in &cfg.defaults.remotes {
        if config::matches_remote_glob(&remote_url, pattern) {
            println!("    4. defaults.remotes[\"{}\"]: {:?}", pattern, mode);
            matched_default = true;
            break;
        }
    }
    if !matched_default {
        println!("    4. defaults.remotes: (no match)");
    }

    println!("    5. Fallback: Pull");

    // Show branch resolution for current branch
    println!();
    let cwd = std::env::current_dir()?;
    let repo = git2::Repository::discover(&cwd)?;
    if let Ok(head) = repo.head() {
        if let Some(branch_name) = head.shorthand() {
            let branch_mode = cfg.resolve_branch_mode(
                &repo_path,
                branch_name,
                in_repo.as_ref(),
                repo_mode,
            );
            println!("  Branch '{}' mode: {:?}", branch_name, branch_mode);
        }
    }

    Ok(())
}

pub async fn handle_enable(path: Option<String>) -> Result<()> {
    let repo_path = resolve_path_or_cwd(path.as_deref())?;

    // Verify it's a git repo
    if !git_ops::is_valid_repo(Path::new(&repo_path)) {
        bail!("not a git repository: {}", repo_path);
    }

    // Resolve to repo_id (common git dir)
    let repo_id = git_ops::discover_repo_id(Path::new(&repo_path))?;
    let repo_id_str = repo_id.to_string_lossy().to_string();

    match connect_to_daemon().await {
        Ok(mut stream) => {
            let req = Request::Enable {
                repo_path: repo_id_str,
            };
            let resp = roundtrip(&mut stream, &req).await?;
            match resp {
                Response::Ok { message } => println!("{}", message),
                Response::Error { message } => eprintln!("error: {}", message),
                _ => eprintln!("unexpected response"),
            }
        }
        Err(_) => {
            // Direct TOML edit: remove disabled flag if present
            let mut cfg = UserConfig::load()?;
            if let Some(repo_cfg) = cfg.repos.get_mut(&repo_id_str) {
                repo_cfg.disabled = Some(false);
            }
            cfg.save()?;

            let dp = display_path(&repo_id_str);
            println!("Enabled {} (daemon not running, change saved to config)", dp);
        }
    }
    Ok(())
}

pub async fn handle_disable(path: Option<String>, purge: bool) -> Result<()> {
    let repo_path = resolve_path_or_cwd(path.as_deref())?;
    let repo_id = git_ops::discover_repo_id(Path::new(&repo_path))?;
    let repo_id_str = repo_id.to_string_lossy().to_string();

    match connect_to_daemon().await {
        Ok(mut stream) => {
            let req = Request::Disable {
                repo_path: repo_id_str.clone(),
                purge,
            };
            let resp = roundtrip(&mut stream, &req).await?;
            match resp {
                Response::Ok { message } => println!("{}", message),
                Response::Error { message } => eprintln!("error: {}", message),
                _ => eprintln!("unexpected response"),
            }
        }
        Err(_) => {
            let mut cfg = UserConfig::load()?;
            let entry = cfg.repos.entry(repo_id_str.clone()).or_default();
            entry.disabled = Some(true);
            cfg.save()?;

            if purge {
                // Remove from state DB too
                if let Ok(db) = StateDb::open() {
                    let _ = db.remove_repo(&repo_id_str);
                }
            }

            let dp = display_path(&repo_id_str);
            println!("Disabled {} (daemon not running, change saved to config)", dp);
        }
    }
    Ok(())
}

pub async fn handle_log(global: bool, follow: bool, since: Option<String>) -> Result<()> {
    match connect_to_daemon().await {
        Ok(mut stream) => {
            let repo_path = if global {
                None
            } else {
                resolve_cwd_repo_path().ok()
            };
            let req = Request::Log {
                repo_path,
                global,
                follow,
                since,
            };
            transport::send_request(&mut stream, &req).await?;

            // Read log entries until LogEnd
            loop {
                let resp = transport::recv_response(&mut stream).await?;
                match resp {
                    Response::LogEntry { entry } => {
                        println!("{}", entry);
                    }
                    Response::LogEnd => {
                        break;
                    }
                    Response::Error { message } => {
                        eprintln!("error: {}", message);
                        break;
                    }
                    _ => {
                        break;
                    }
                }
            }
        }
        Err(_) => {
            // Fallback: read daemon.log directly
            eprintln!("warning: daemon not running, reading log file directly");
            eprintln!();
            let log_path = paths::daemon_log();
            if log_path.exists() {
                let content = std::fs::read_to_string(&log_path)
                    .context("failed to read daemon log")?;
                // If --since is set, filter lines
                if let Some(ref since_str) = since {
                    if let Ok(since_dt) = DateTime::parse_from_rfc3339(since_str) {
                        for line in content.lines() {
                            // Try to parse a leading timestamp from each line
                            if let Some(ts_end) = line.find(' ') {
                                if let Ok(line_dt) = DateTime::parse_from_rfc3339(&line[..ts_end]) {
                                    if line_dt >= since_dt {
                                        println!("{}", line);
                                    }
                                    continue;
                                }
                            }
                            // If can't parse timestamp, print the line anyway
                            println!("{}", line);
                        }
                    } else {
                        // Can't parse --since, just print everything
                        print!("{}", content);
                    }
                } else {
                    print!("{}", content);
                }
            } else {
                println!("No log file found at {}", log_path.display());
            }
        }
    }
    Ok(())
}

pub async fn handle_sync(all: bool) -> Result<()> {
    match connect_to_daemon().await {
        Ok(mut stream) => {
            let repo_path = if all {
                None
            } else {
                Some(resolve_cwd_repo_path()?)
            };
            let req = Request::Sync { repo_path, all };
            let resp = roundtrip(&mut stream, &req).await?;
            match resp {
                Response::Ok { message } => println!("{}", message),
                Response::Error { message } => eprintln!("error: {}", message),
                _ => eprintln!("unexpected response"),
            }
        }
        Err(_) => {
            bail!("daemon not running. Start it with `gitsitter daemon start`");
        }
    }
    Ok(())
}

pub async fn handle_register(path: Option<String>) -> Result<()> {
    let repo_path = resolve_path_or_cwd(path.as_deref())?;

    // Verify it's a git repo and resolve to repo_id
    let repo_id = git_ops::discover_repo_id(Path::new(&repo_path))
        .context("not a git repository")?;
    let repo_id_str = repo_id.to_string_lossy().to_string();

    match connect_to_daemon().await {
        Ok(mut stream) => {
            let req = Request::Register {
                repo_path: repo_id_str,
            };
            let resp = roundtrip(&mut stream, &req).await?;
            match resp {
                Response::Ok { message } => println!("{}", message),
                Response::Error { message } => eprintln!("error: {}", message),
                _ => eprintln!("unexpected response"),
            }
        }
        Err(_) => {
            bail!("daemon not running. Start it with `gitsitter daemon start`");
        }
    }
    Ok(())
}

pub async fn handle_daemon_status() -> Result<()> {
    match connect_to_daemon().await {
        Ok(mut stream) => {
            let req = Request::DaemonStatus;
            let resp = roundtrip(&mut stream, &req).await?;
            match resp {
                Response::DaemonStatus {
                    pid,
                    uptime_secs,
                    repos_watched,
                } => {
                    println!("gitsitter daemon is running");
                    println!("  PID:           {}", pid);
                    println!("  Uptime:        {}", format_uptime(uptime_secs));
                    println!("  Repos watched: {}", repos_watched);
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
            println!("gitsitter daemon is not running");
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

#[cfg(windows)]
async fn sc_command(args: &[&str]) -> Result<std::process::Output> {
    tokio::process::Command::new("sc.exe")
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to run sc.exe {}", args.join(" ")))
}

pub async fn handle_daemon_start() -> Result<()> {
    // Check if already running
    if transport::is_daemon_running() {
        println!("gitsitter daemon is already running");
        return Ok(());
    }

    // Try platform service manager first
    #[cfg(target_os = "macos")]
    {
        let plist_path = dirs::home_dir()
            .map(|h| h.join("Library/LaunchAgents/com.gitsitter.daemon.plist"));
        if let Some(ref p) = plist_path {
            if p.exists() {
                let domain_target = format!("gui/{}", unsafe { libc::getuid() });
                let result = tokio::process::Command::new("launchctl")
                    .args(["bootstrap", &domain_target, &p.display().to_string()])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .status()
                    .await;
                if let Ok(status) = result {
                    if status.success() {
                        println!("gitsitter daemon started via launchd");
                        return Ok(());
                    }
                }
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
                println!("gitsitter daemon started via systemd");
                return Ok(());
            }
            _ => {
                // systemd not available or unit not installed -- spawn directly
            }
        }
    }

    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "linux")))]
    {
        eprintln!("warning: no supported Unix service manager configured on this platform; starting detached daemon directly");
    }

    #[cfg(windows)]
    {
        let service_result = sc_command(&["start", crate::service::SERVICE_NAME]).await;
        match service_result {
            Ok(output) if output.status.success() => {
                println!("gitsitter daemon started via Windows Service Control Manager");
                return Ok(());
            }
            Ok(_) | Err(_) => {
                // Service not installed or SCM unavailable -- spawn directly.
            }
        }
    }

    // Spawn self as a detached background process
    let exe = std::env::current_exe().context("failed to determine own executable path")?;
    paths::ensure_dirs()?;

    let log_file = std::fs::File::create(paths::daemon_log())
        .context("failed to create daemon log file")?;
    let log_stderr = log_file.try_clone()?;

    let child = std::process::Command::new(&exe)
        .args(["daemon", "run"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_stderr))
        .spawn()
        .context("failed to spawn daemon process")?;

    println!("gitsitter daemon started (PID: {})", child.id());
    Ok(())
}

pub async fn handle_daemon_stop() -> Result<()> {
    #[cfg(windows)]
    {
        let service_result = sc_command(&["stop", crate::service::SERVICE_NAME]).await;
        if let Ok(output) = service_result {
            if output.status.success() {
                println!("gitsitter daemon stop requested via Windows Service Control Manager");
                return Ok(());
            }
        }
    }

    match connect_to_daemon().await {
        Ok(mut stream) => {
            let req = Request::Shutdown;
            let resp = roundtrip(&mut stream, &req).await;
            match resp {
                Ok(Response::Ok { message }) => println!("{}", message),
                Ok(Response::Error { message }) => eprintln!("error: {}", message),
                // Connection may close before we get a response during shutdown
                Err(_) => println!("gitsitter daemon stopped"),
                _ => println!("gitsitter daemon stopped"),
            }
        }
        Err(_) => {
            println!("gitsitter daemon is not running");
        }
    }
    Ok(())
}

pub async fn handle_daemon_restart() -> Result<()> {
    // Stop if running
    if transport::is_daemon_running() {
        if let Ok(mut stream) = connect_to_daemon().await {
            let req = Request::Shutdown;
            let _ = roundtrip(&mut stream, &req).await;
            // Give it a moment to shut down
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
    // Start
    handle_daemon_start().await
}

pub async fn handle_daemon_run() -> Result<()> {
    crate::daemon::run_daemon().await
}

pub async fn handle_daemon_service() -> Result<()> {
    crate::service::run_service_dispatcher()
}

pub async fn handle_install(component: Option<String>, shell_name: Option<String>) -> Result<()> {
    let comp = component.as_deref().unwrap_or("all");
    match comp {
        "shell" | "hooks" | "all" => {
            let sh = match &shell_name {
                Some(s) => s.clone(),
                None => crate::shell::detect_shell()
                    .context("could not detect shell, specify with: gitsitter install shell <name>")?,
            };
            crate::shell::install_hook(&sh)?;
            println!("Shell hook installed for {sh}");
        }
        "daemon" => {
            let exe = std::env::current_exe()?;
            #[cfg(target_os = "macos")]
            {
                // Generate launchd plist
                let launch_agents = dirs::home_dir()
                    .context("cannot determine home directory")?
                    .join("Library/LaunchAgents");
                std::fs::create_dir_all(&launch_agents)?;
                let log_dir = dirs::home_dir()
                    .context("cannot determine home directory")?
                    .join("Library/Logs/gitsitter");
                std::fs::create_dir_all(&log_dir)?;
                let plist = include_str!("embed/com.gitsitter.daemon.plist")
                    .replace("@@EXEC_PATH@@", &exe.display().to_string())
                    .replace("@@LOG_DIR@@", &log_dir.display().to_string());
                let plist_path = launch_agents.join("com.gitsitter.daemon.plist");
                std::fs::write(&plist_path, plist)?;
                println!("launchd plist written to {}", plist_path.display());
                println!("Run: launchctl bootstrap gui/$(id -u) {}", plist_path.display());
            }

            #[cfg(target_os = "linux")]
            {
                // Generate systemd unit file
                let unit_dir = dirs::home_dir()
                    .context("cannot determine home directory")?
                    .join(".config/systemd/user");
                std::fs::create_dir_all(&unit_dir)?;
                let unit = include_str!("embed/gitsitter.service")
                    .replace("@@EXEC_PATH@@", &exe.display().to_string());
                let unit_path = unit_dir.join("gitsitter.service");
                std::fs::write(&unit_path, unit)?;
                println!("Systemd user service written to {}", unit_path.display());
                println!("Run: systemctl --user daemon-reload && systemctl --user enable --now gitsitter");
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
        }
        _ => bail!("unknown component: {}. Use 'shell', 'hooks', 'daemon', or 'all'", comp),
    }

    if comp == "all" {
        // Also offer daemon install hint
        println!();
        #[cfg(target_os = "macos")]
        println!("To also install the launchd service, run: gitsitter install daemon");
        #[cfg(target_os = "linux")]
        println!("To also install the systemd service, run: gitsitter install daemon");
        #[cfg(all(unix, not(target_os = "macos"), not(target_os = "linux")))]
        println!("Daemon service installation is not implemented for this Unix platform");
        #[cfg(windows)]
        println!("To also install the Windows service, run: gitsitter install daemon");
    }
    Ok(())
}

pub async fn handle_uninstall(component: Option<String>) -> Result<()> {
    let comp = component.as_deref().unwrap_or("all");
    match comp {
        "shell" | "hooks" | "all" => {
            let sh = crate::shell::detect_shell()
                .unwrap_or_else(|| "bash".to_string());
            crate::shell::uninstall_hook(&sh)?;
            println!("Shell hook removed for {sh}");
        }
        "daemon" => {
            #[cfg(target_os = "macos")]
            {
                let plist_path = dirs::home_dir()
                    .context("cannot determine home directory")?
                    .join("Library/LaunchAgents/com.gitsitter.daemon.plist");
                if plist_path.exists() {
                    println!("Run first: launchctl bootout gui/$(id -u) {}", plist_path.display());
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
                    std::fs::remove_file(&unit_path)?;
                    println!("Systemd service file removed");
                    println!("Run: systemctl --user daemon-reload");
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
        }
        _ => bail!("unknown component: {}. Use 'shell', 'hooks', 'daemon', or 'all'", comp),
    }
    Ok(())
}

/// Hidden command used by shell hooks to check for notifications.
pub async fn handle_prompt() -> Result<()> {
    // Quick check: try to connect to daemon with short timeout
    let connect = tokio::time::timeout(
        std::time::Duration::from_millis(20),
        connect_to_daemon(),
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
        // Load cooldown config and state DB for rate-limiting notifications.
        let cooldown = config::UserConfig::load()
            .map(|c| c.global.notification_cooldown)
            .unwrap_or(std::time::Duration::from_secs(300));
        let db = StateDb::open().ok();

        for b in &data.branches {
            let notification_type = match b.status.as_str() {
                "diverged" | "upstream_gone" | "push_blocked" => b.status.as_str(),
                _ => continue,
            };

            // Build a per-branch cooldown key: "status:branch_name"
            let cooldown_key = format!("{}:{}", notification_type, b.name);

            if let Some(ref db) = db {
                if let Ok(false) = db.should_notify(&data.repo_id, &cooldown_key, cooldown) {
                    continue;
                }
            }

            match notification_type {
                "diverged" => {
                    let upstream = b.upstream.as_deref().unwrap_or("upstream");
                    println!(
                        "\u{26A0}\u{FE0F}  gitsitter: {} has diverged from {} (ff not possible)",
                        b.name, upstream
                    );
                }
                "upstream_gone" => {
                    println!(
                        "\u{26A0}\u{FE0F}  gitsitter: {}'s upstream has been deleted",
                        b.name
                    );
                }
                "push_blocked" => {
                    println!(
                        "\u{26A0}\u{FE0F}  gitsitter: push blocked for {}",
                        b.name
                    );
                }
                _ => unreachable!(),
            }

            if let Some(ref db) = db {
                let _ = db.record_notification(&data.repo_id, &cooldown_key);
            }
        }
    }
    Ok(())
}
