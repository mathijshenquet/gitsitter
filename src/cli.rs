//! CLI command handlers.
//!
//! Each function corresponds to a CLI subcommand. They connect to the daemon
//! via Unix socket, send a request, and print the response.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use comfy_table::{presets, Cell, Color, ContentArrangement, Table};

use crate::cli_ui::{self, DisplayOpts};
use crate::config::{self, UserConfig};
use crate::git_ops;
use crate::paths::Paths;
use crate::transport::{
    self, DaemonStream, Request, Response,
};

/// Load display options from user config (best-effort, defaults if config fails).
fn load_display_opts(paths: &Paths) -> DisplayOpts {
    match UserConfig::load(&paths.config_file) {
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
async fn roundtrip<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    req: &Request,
) -> Result<Response> {
    transport::send_request(stream, req).await?;
    transport::recv_response(stream).await
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

    let repo_path = resolve_cwd_repo_path()?;

    if !daemon_running {
        println!();
        let dp = display_path(&repo_path);
        println!("{}", cli_ui::repo_header(&dp, opts));
        println!();
        println!("   No sync data available, ensure the daemon is running");
        println!();
        println!("  {}", cli_ui::mode_line(opts));
        println!("  {}", cli_ui::change_hint());
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
    Ok(())
}

async fn handle_status_global(paths: &Paths, opts: DisplayOpts, daemon_running: bool) -> Result<()> {
    if !daemon_running {
        let cfg = UserConfig::load(&paths.config_file)?;
        if cfg.repos.is_empty() {
            println!("No repos registered.");
        } else {
            println!();
            println!("  Watched repositories (daemon not running)");
            println!();
            let mut table = global_status_table();
            for (path, repo_cfg) in &cfg.repos {
                let dp = display_path(path);
                let disabled = repo_cfg.disabled.as_ref().map_or(false, |d| d.is_repo_disabled());
                let sync = if disabled { "never synced, disabled" } else { "never synced" };
                table.add_row(global_status_row(&dp, "auto", sync, "", opts));
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
                println!("  Watched repositories ({} total)", repos.len());
                println!();
                let mut table = global_status_table();
                for r in &repos {
                    let dp = display_path(&r.display_path);
                    let sync_info = match &r.last_sync {
                        Some(ts) => format!("synced {}", format_relative_time(ts)),
                        None => "never synced".to_string(),
                    };
                    table.add_row(global_status_row(&dp, &r.mode, &sync_info, &r.status_summary, opts));
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
        .set_header(vec!["Repo", "Mode", "Last Sync", "Status"]);
    table
}

fn global_status_row(path: &str, mode: &str, sync: &str, status: &str, opts: DisplayOpts) -> Vec<Cell> {
    if opts.colors {
        vec![
            Cell::new(path).fg(Color::Blue),
            Cell::new(mode).fg(Color::Blue),
            Cell::new(sync),
            Cell::new(status),
        ]
    } else {
        vec![
            Cell::new(path),
            Cell::new(mode),
            Cell::new(sync),
            Cell::new(status),
        ]
    }
}

/// Print warnings about untrusted remotes for a repo (used by enable/register).
fn print_untrusted_remote_warnings(repo_id: &Path, cfg: &UserConfig, opts: DisplayOpts) {
    let remote_urls = git_ops::get_all_remote_urls(repo_id).unwrap_or_default();
    let repo_id_str = repo_id.to_string_lossy();
    for (name, url) in &remote_urls {
        if !cfg.is_remote_trusted(url) {
            if let Some(host) = config::extract_host(url) {
                println!("  {} remote '{}' ({}) is not trusted — won't sync",
                    cli_ui::warning_icon(opts), name, host);
                println!("  Add with: gitsitter trust {}", host);
            }
        }
        if cfg.is_remote_disabled(&repo_id_str, name) {
            println!("  {} remote '{}' is disabled",
                cli_ui::warning_icon(opts), name);
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
        let upstream = match &b.upstream {
            Some(u) => format!("\u{2190} {}", u),
            None => "(no upstream)".to_string(),
        };
        let icon = cli_ui::branch_status_icon(&b.status, opts);
        let label = cli_ui::branch_status_styled(&b.status, opts);
        let action = match &b.last_action {
            Some(a) => format!(", {}", a),
            None => String::new(),
        };
        println!(
            "  {:<32} {}  {}{}",
            format!("{} {}", b.name, upstream), icon, label, action
        );
    }
    println!();
    println!("  {}", cli_ui::mode_line(opts));
    // Show remote warnings
    if !data.untrusted_remotes.is_empty() {
        for r in &data.untrusted_remotes {
            println!("  {} remote '{}' is not trusted — branches tracking it won't sync",
                cli_ui::warning_icon(opts), r);
        }
        for host in &data.untrusted_hosts {
            println!("  Add with: gitsitter trust {}", host);
        }
        println!();
    }
    if !data.disabled_remotes.is_empty() {
        for r in &data.disabled_remotes {
            println!("  {} remote '{}' is disabled",
                cli_ui::warning_icon(opts), r);
        }
        println!();
    }
    println!("  {}", cli_ui::change_hint());
    println!();
}

pub async fn handle_config(paths: &Paths,
    global: bool,
    explain: bool,
) -> Result<()> {
    if explain {
        return handle_config_explain(paths).await;
    }

    if global {
        print_global_config(paths)?;
    } else {
        print_repo_config(paths)?;
    }
    Ok(())
}

fn print_global_config(paths: &Paths) -> Result<()> {
    let cfg = UserConfig::load(&paths.config_file)?;
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
            if let Some(d) = &repo_cfg.disabled {
                match d {
                    config::Disabled::All(true) => println!("    disabled: true"),
                    config::Disabled::Remotes(remotes) => println!("    disabled remotes: {:?}", remotes),
                    _ => {}
                }
            }
            if let Some(ri) = repo_cfg.refresh_interval {
                println!("    refresh_interval: {:?}", ri);
            }
        }
    }
    Ok(())
}

fn print_repo_config(paths: &Paths) -> Result<()> {
    let opts = load_display_opts(paths);
    let cfg = UserConfig::load(&paths.config_file)?;
    let repo_path = match resolve_cwd_repo_path() {
        Ok(p) => p,
        Err(_) => {
            println!("Not inside a git repository. Use --global to see global config.");
            return Ok(());
        }
    };
    let dp = display_path(&repo_path);
    println!("Config for {}", cli_ui::repo_header(&dp, opts));
    println!();

    println!("  {}", cli_ui::mode_line(opts));
    println!();

    let refresh = cfg.effective_refresh_interval(
        &repo_path,
        config::InRepoConfig::load(&git_ops::get_display_path(&PathBuf::from(&repo_path))?)?.as_ref(),
    );
    println!("  Refresh interval: {}s", refresh.as_secs());
    Ok(())
}

async fn handle_config_explain(paths: &Paths) -> Result<()> {
    let cfg = UserConfig::load(&paths.config_file)?;
    let repo_path = resolve_cwd_repo_path()?;
    let dp = display_path(&repo_path);
    let repo_id_path = PathBuf::from(&repo_path);

    println!("Config diagnostics for {}:", dp);
    println!();

    // Repo disabled?
    if cfg.is_repo_disabled(&repo_path) {
        println!("  Status: DISABLED");
        println!("  Enable with: gitsitter enable");
        return Ok(());
    }
    println!("  Status: enabled");
    println!();

    // Show remote trust status
    let remote_urls = git_ops::get_all_remote_urls(&repo_id_path)?;
    if remote_urls.is_empty() {
        println!("  No remotes configured.");
    } else {
        println!("  Remotes:");
        for (name, url) in &remote_urls {
            let trusted = cfg.is_remote_trusted(url);
            let disabled = cfg.is_remote_disabled(&repo_path, name);
            let status = if disabled {
                "disabled"
            } else if trusted {
                "trusted"
            } else {
                "UNTRUSTED"
            };
            println!("    {} ({}) — {}", name, url, status);
        }
    }
    println!();

    // Refresh interval
    let in_repo = config::InRepoConfig::load(&git_ops::get_display_path(&repo_id_path)?)?;
    let refresh = cfg.effective_refresh_interval(&repo_path, in_repo.as_ref());
    println!("  Refresh interval: {}s", refresh.as_secs());
    println!();

    // Show current branch ownership
    let cwd = std::env::current_dir()?;
    let repo = git2::Repository::discover(&cwd)?;
    if let Ok(head) = repo.head() {
        if let Some(branch_name) = head.shorthand() {
            let owned = git_ops::is_branch_owned_by_user(&repo_id_path, branch_name)
                .unwrap_or(false);
            println!("  Branch '{}': auto-push {}", branch_name,
                if owned { "YES (you own it)" } else { "NO (not your branch)" });
        }
    }

    Ok(())
}

pub async fn handle_enable(paths: &Paths, path: Option<String>) -> Result<()> {
    let opts = load_display_opts(paths);
    let repo_path = resolve_path_or_cwd(path.as_deref())?;

    // Verify it's a git repo
    if !git_ops::is_valid_repo(Path::new(&repo_path)) {
        bail!("not a git repository: {}", repo_path);
    }

    // Resolve to repo_id (common git dir)
    let repo_id = git_ops::discover_repo_id(Path::new(&repo_path))?;
    let repo_id_str = repo_id.to_string_lossy().to_string();

    UserConfig::modify(&paths.config_file, |cfg| {
        let entry = cfg.repos.entry(repo_id_str.clone()).or_default();
        entry.disabled = Some(config::Disabled::All(false));
    })?;
    notify_daemon_reload(paths).await;

    let dp = display_path(&repo_id_str);
    let icon = cli_ui::success_icon(opts);
    println!("{} Enabled {}", icon, cli_ui::repo_header(&dp, opts));

    let cfg = UserConfig::load(&paths.config_file)?;
    let daemon_running = transport::is_daemon_running(&paths.socket_path);
    cli_ui::print_repo_info_block(daemon_running, opts);
    print_untrusted_remote_warnings(&repo_id, &cfg, opts);

    Ok(())
}

pub async fn handle_disable(paths: &Paths, path: Option<String>, purge: bool) -> Result<()> {
    let opts = load_display_opts(paths);
    let repo_path = resolve_path_or_cwd(path.as_deref())?;
    let repo_id = git_ops::discover_repo_id(Path::new(&repo_path))?;
    let repo_id_str = repo_id.to_string_lossy().to_string();

    UserConfig::modify(&paths.config_file, {
        let repo_id = repo_id_str.clone();
        move |cfg| {
            if purge {
                cfg.repos.remove(&repo_id);
            } else {
                let entry = cfg.repos.entry(repo_id).or_default();
                entry.disabled = Some(config::Disabled::All(true));
            }
        }
    })?;
    notify_daemon_reload(paths).await;

    let dp = display_path(&repo_id_str);
    let icon = cli_ui::pause_icon(opts);
    println!("{} Disabled {}", icon, cli_ui::repo_header(&dp, opts));
    Ok(())
}

pub async fn handle_remote_enable(paths: &Paths, name: Option<&str>) -> Result<()> {
    let opts = load_display_opts(paths);
    let remote_name = name.context("remote name is required: gitsitter enable --remote <name>")?;
    let repo_path = resolve_cwd_repo_path()?;

    // Verify the remote exists in git
    let repo_id_path = PathBuf::from(&repo_path);
    let remote_urls = git_ops::get_all_remote_urls(&repo_id_path)?;
    if !remote_urls.contains_key(remote_name) {
        bail!("remote '{}' not found in this repo", remote_name);
    }

    // Check if the whole repo is disabled — per-remote enable doesn't make sense then
    let cfg = UserConfig::load(&paths.config_file)?;
    if cfg.is_repo_disabled(&repo_path) {
        bail!("repo is fully disabled — run `gitsitter enable` first");
    }

    UserConfig::modify(&paths.config_file, {
        let repo_path = repo_path.clone();
        let remote_name = remote_name.to_string();
        move |cfg| {
            let entry = cfg.repos.entry(repo_path).or_default();
            match &mut entry.disabled {
                Some(config::Disabled::Remotes(list)) => {
                    list.retain(|r| r != &remote_name);
                    if list.is_empty() {
                        entry.disabled = Some(config::Disabled::All(false));
                    }
                }
                _ => {} // repo is fully enabled — nothing to do
            }
        }
    })?;
    notify_daemon_reload(paths).await;

    let icon = cli_ui::success_icon(opts);
    println!("{} Enabled remote '{}'", icon, remote_name);
    Ok(())
}

pub async fn handle_remote_disable(paths: &Paths, name: Option<&str>) -> Result<()> {
    let opts = load_display_opts(paths);
    let remote_name = name.context("remote name is required: gitsitter disable --remote <name>")?;
    let repo_path = resolve_cwd_repo_path()?;

    // Verify the remote exists in git
    let repo_id_path = PathBuf::from(&repo_path);
    let remote_urls = git_ops::get_all_remote_urls(&repo_id_path)?;
    if !remote_urls.contains_key(remote_name) {
        bail!("remote '{}' not found in this repo", remote_name);
    }

    // Check if the whole repo is already disabled
    let cfg = UserConfig::load(&paths.config_file)?;
    if cfg.is_repo_disabled(&repo_path) {
        bail!("repo is already fully disabled");
    }

    UserConfig::modify(&paths.config_file, {
        let repo_path = repo_path.clone();
        let remote_name = remote_name.to_string();
        move |cfg| {
            let entry = cfg.repos.entry(repo_path).or_default();
            match &mut entry.disabled {
                Some(config::Disabled::Remotes(list)) => {
                    if !list.contains(&remote_name) {
                        list.push(remote_name);
                    }
                }
                _ => {
                    entry.disabled = Some(config::Disabled::Remotes(vec![remote_name]));
                }
            }
        }
    })?;
    notify_daemon_reload(paths).await;

    let icon = cli_ui::pause_icon(opts);
    println!("{} Disabled remote '{}'", icon, remote_name);
    Ok(())
}

pub async fn handle_trust(paths: &Paths, host: &str) -> Result<()> {
    let opts = load_display_opts(paths);
    UserConfig::modify(&paths.config_file, {
        let host = host.to_string();
        move |cfg| {
            cfg.trusted_hosts.insert(host, true);
        }
    })?;
    notify_daemon_reload(paths).await;

    let icon = cli_ui::success_icon(opts);
    println!("{} Trusted host '{}'", icon, host);
    Ok(())
}

pub async fn handle_untrust(paths: &Paths, host: &str) -> Result<()> {
    let opts = load_display_opts(paths);
    UserConfig::modify(&paths.config_file, {
        let host = host.to_string();
        move |cfg| {
            cfg.trusted_hosts.remove(&host);
        }
    })?;
    notify_daemon_reload(paths).await;

    let icon = cli_ui::pause_icon(opts);
    println!("{} Untrusted host '{}'", icon, host);
    Ok(())
}

pub async fn handle_log(paths: &Paths, _global: bool, _follow: bool, since: Option<String>) -> Result<()> {
    let log_path = paths.daemon_log.clone();
    if !log_path.exists() {
        println!("No log file found at {}", log_path.display());
        return Ok(());
    }
    let content = std::fs::read_to_string(&log_path)
        .context("failed to read daemon log")?;
    if let Some(ref since_str) = since {
        if let Ok(since_dt) = DateTime::parse_from_rfc3339(since_str) {
            for line in content.lines() {
                if let Some(ts_end) = line.find(' ') {
                    if let Ok(line_dt) = DateTime::parse_from_rfc3339(&line[..ts_end]) {
                        if line_dt >= since_dt {
                            println!("{}", line);
                        }
                        continue;
                    }
                }
                println!("{}", line);
            }
        } else {
            print!("{}", content);
        }
    } else {
        print!("{}", content);
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
    let repo_id = git_ops::discover_repo_id(Path::new(&repo_path))
        .context("not a git repository")?;
    let repo_id_str = repo_id.to_string_lossy().to_string();

    UserConfig::modify(&paths.config_file, {
        let repo_id = repo_id_str.clone();
        move |cfg| {
            cfg.repos.entry(repo_id).or_default();
        }
    })?;
    notify_daemon_reload(paths).await;

    let dp = display_path(&repo_id_str);
    let icon = cli_ui::celebrate_icon(opts);
    println!("{} Registered {}", icon, cli_ui::repo_header(&dp, opts));

    let cfg = UserConfig::load(&paths.config_file)?;
    let daemon_running = transport::is_daemon_running(&paths.socket_path);
    cli_ui::print_repo_info_block(daemon_running, opts);
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
                } => {
                    let icon = cli_ui::success_icon(opts);
                    println!("{} Daemon is running", icon);
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
                        if wait_for_daemon_ready(paths, std::time::Duration::from_secs(3)).await {
                            println!("\u{2713} Daemon started via launchd");
                            return Ok(());
                        }
                        bail!("launchd accepted the job, but the daemon socket did not appear");
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
        eprintln!("warning: no supported Unix service manager configured on this platform; starting detached daemon directly");
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

    let log_file = std::fs::File::create(paths.daemon_log.clone())
        .context("failed to create daemon log file")?;
    let log_stderr = log_file.try_clone()?;

    let child = std::process::Command::new(&exe)
        .args(["daemon", "run"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_stderr))
        .spawn()
        .context("failed to spawn daemon process")?;

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
        if let Ok(output) = service_result {
            if output.status.success() {
                println!("\u{2713} Daemon stopped");
                return Ok(());
            }
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
    if transport::is_daemon_running(&paths.socket_path) {
        if let Ok(mut stream) = transport::connect_to_daemon(&paths.socket_path).await {
            let req = Request::Shutdown;
            let _ = roundtrip(&mut stream, &req).await;
            // Give it a moment to shut down
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
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
        for b in &data.branches {
            let notification_type = match b.status.as_str() {
                "diverged"
                | "upstream_gone"
                | "push_rejected"
                | "auth_failed"
                | "network_error"
                | "push_blocked_hook_timeout" => b.status.as_str(),
                _ => continue,
            };

            // We can't check daemon-side cooldowns from the CLI process,
            // so we use a simple heuristic: always print (the daemon's sync
            // loop only sets these states when they're new/changed).
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
                "push_rejected" => {
                    println!(
                        "\u{26A0}\u{FE0F}  gitsitter: push rejected for {}",
                        b.name
                    );
                }
                "auth_failed" => {
                    println!(
                        "\u{26A0}\u{FE0F}  gitsitter: auth failed while syncing {}",
                        b.name
                    );
                }
                "network_error" => {
                    println!(
                        "\u{26A0}\u{FE0F}  gitsitter: network error while syncing {}",
                        b.name
                    );
                }
                "push_blocked_hook_timeout" => {
                    println!(
                        "\u{26A0}\u{FE0F}  gitsitter: push blocked by hook timeout for {}",
                        b.name
                    );
                }
                _ => unreachable!(),
            }
        }
    }
    Ok(())
}
