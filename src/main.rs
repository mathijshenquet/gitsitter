use clap::{Parser, Subcommand};

use gitsitter::cli;
use gitsitter::paths::Paths;

#[derive(Parser)]
#[command(name = "gitsitter", about = "Keep local branches in sync with remotes")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Show status (default when no command given)
    Status {
        #[arg(short, long)]
        global: bool,
    },
    /// Configure gitsitter
    Config {
        #[arg(short, long)]
        global: bool,
        #[arg(long)]
        explain: bool,
    },
    /// Enable a repo or a specific remote
    Enable {
        /// Path to repo, or remote name if --remote is set
        target: Option<String>,
        /// Treat target as a remote name in the current repo
        #[arg(long)]
        remote: bool,
    },
    /// Disable a repo or a specific remote
    Disable {
        /// Path to repo, or remote name if --remote is set
        target: Option<String>,
        /// Treat target as a remote name in the current repo
        #[arg(long)]
        remote: bool,
        #[arg(long)]
        purge: bool,
    },
    /// Trust a remote host (allows syncing with remotes on this host)
    Trust {
        host: String,
    },
    /// Untrust a remote host
    Untrust {
        host: String,
    },
    /// Show daemon log
    Log {
        #[arg(short, long)]
        global: bool,
        #[arg(short, long)]
        follow: bool,
        #[arg(long)]
        since: Option<String>,
    },
    /// Trigger immediate sync
    Sync {
        #[arg(long)]
        all: bool,
    },
    /// Register a repo (usually called by shell hooks)
    Register {
        path: Option<String>,
    },
    /// Interactively resolve sync issues (diverged/unowned branches)
    Resolve {
        /// Resolve issues for all repos (default: current repo only)
        #[arg(short, long)]
        global: bool,
    },
    /// Install daemon and shell hooks
    Install {
        component: Option<String>,
        shell_name: Option<String>,
    },
    /// Uninstall daemon and shell hooks
    Uninstall {
        component: Option<String>,
    },
    /// Internal: prompt hook for shell integration
    #[command(hide = true)]
    #[clap(name = "_prompt")]
    Prompt,
    /// Daemon control
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    Run,
    #[command(hide = true)]
    Service,
    Start,
    Stop,
    Restart,
    Status,
}

#[tokio::main]
async fn main() {
    let args = Cli::parse();
    let paths = Paths::resolve();

    let result = match args.command {
        None => cli::handle_status(&paths, false).await,
        Some(Commands::Status { global }) => cli::handle_status(&paths, global).await,
        Some(Commands::Config {
            global,
            explain,
        }) => cli::handle_config(&paths, global, explain).await,
        Some(Commands::Enable { target, remote }) => {
            if remote {
                cli::handle_remote_enable(&paths, target.as_deref()).await
            } else {
                cli::handle_enable(&paths, target).await
            }
        }
        Some(Commands::Disable { target, remote, purge }) => {
            if remote {
                cli::handle_remote_disable(&paths, target.as_deref()).await
            } else {
                cli::handle_disable(&paths, target, purge).await
            }
        }
        Some(Commands::Trust { host }) => cli::handle_trust(&paths, &host).await,
        Some(Commands::Untrust { host }) => cli::handle_untrust(&paths, &host).await,
        Some(Commands::Log {
            global,
            follow,
            since,
        }) => cli::handle_log(&paths, global, follow, since).await,
        Some(Commands::Sync { all }) => cli::handle_sync(&paths, all).await,
        Some(Commands::Resolve { global }) => cli::handle_resolve(&paths, global).await,
        Some(Commands::Register { path }) => cli::handle_register(&paths, path).await,
        Some(Commands::Install { component, shell_name }) => {
            cli::handle_install(component, shell_name).await
        }
        Some(Commands::Uninstall { component }) => {
            cli::handle_uninstall(component).await
        }
        Some(Commands::Prompt) => cli::handle_prompt(&paths).await,
        Some(Commands::Daemon { action }) => match action {
            DaemonAction::Run => cli::handle_daemon_run(&paths).await,
            DaemonAction::Service => cli::handle_daemon_service().await,
            DaemonAction::Start => cli::handle_daemon_start(&paths).await,
            DaemonAction::Stop => cli::handle_daemon_stop(&paths).await,
            DaemonAction::Restart => cli::handle_daemon_restart(&paths).await,
            DaemonAction::Status => cli::handle_daemon_status(&paths).await,
        },
    };

    if let Err(e) = result {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}
