use clap::{Parser, Subcommand};

use gitsitter::cli;

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
        #[arg(short, long)]
        repo: Option<String>,
        #[arg(short, long)]
        branch: Option<String>,
        #[arg(long)]
        explain: bool,
    },
    /// Enable a repo
    Enable {
        path: Option<String>,
    },
    /// Disable a repo
    Disable {
        path: Option<String>,
        #[arg(long)]
        purge: bool,
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
    Start,
    Stop,
    Restart,
    Status,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        None => cli::handle_status(false).await,
        Some(Commands::Status { global }) => cli::handle_status(global).await,
        Some(Commands::Config {
            global,
            repo,
            branch,
            explain,
        }) => cli::handle_config(global, repo, branch, explain).await,
        Some(Commands::Enable { path }) => cli::handle_enable(path).await,
        Some(Commands::Disable { path, purge }) => cli::handle_disable(path, purge).await,
        Some(Commands::Log {
            global,
            follow,
            since,
        }) => cli::handle_log(global, follow, since).await,
        Some(Commands::Sync { all }) => cli::handle_sync(all).await,
        Some(Commands::Register { path }) => cli::handle_register(path).await,
        Some(Commands::Install { component, shell_name }) => {
            cli::handle_install(component, shell_name).await
        }
        Some(Commands::Uninstall { component }) => {
            cli::handle_uninstall(component).await
        }
        Some(Commands::Prompt) => cli::handle_prompt().await,
        Some(Commands::Daemon { action }) => match action {
            DaemonAction::Run => cli::handle_daemon_run().await,
            DaemonAction::Start => cli::handle_daemon_start().await,
            DaemonAction::Stop => cli::handle_daemon_stop().await,
            DaemonAction::Restart => cli::handle_daemon_restart().await,
            DaemonAction::Status => cli::handle_daemon_status().await,
        },
    };

    if let Err(e) = result {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}
