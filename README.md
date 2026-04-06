# gitsitter

A git utility that keeps your local branches in sync with their tracking remotes — automatically, silently, and safely. Single Rust binary that acts as both CLI and background daemon.

- **Remote ahead** → fast-forward merge into local
- **Local ahead + you own the branch** → push to remote
- **Local ahead + not your branch** → notify the user (unless local tip is a merge of the remote tip)

- **Diverged + you own the remote tip** → auto rebase and push
- **Diverged + not your remote tip** → notify the user

## Philosophy

- **Zero configuration.** Behavior is derived from git's own tracking branches and commit authorship. No modes, no rules, no globs.
- **Silent by default.** Invisible when everything is working.
- **Notify on problems.** Diverged branches, unpushed commits on branches you don't own, failed pushes — surfaced to the user via shell prompt.
- **Safe by default.** Uses `--force-with-lease` only when you own the remote tip (rebase/amend workflow). Never overwrites someone else's work.

### Ownership Rule

gitsitter decides whether to auto-push based on a simple heuristic: **if the tip commit of the remote tracking branch was authored by you, you own the branch.** This means:

- Your feature branches get pushed automatically.
- Someone else's branch that you checked out won't get pushed, even if you commit on it.
- If someone else pushes to your branch, gitsitter stops auto-pushing until you push manually (reclaiming ownership).
- **Exception:** if your local tip is a merge commit that includes the remote tip as a parent, gitsitter pushes anyway — merging someone else's work is an explicit integration, not an overwrite.

## Quick Start

```sh
# Build
cargo build --release

# Install shell hooks
gitsitter install shell

# Install the daemon service
gitsitter install daemon

# Or just run the daemon directly
gitsitter daemon run
```

Once the shell hook is installed, repos are auto-registered when you `cd` into them. The daemon fetches, pulls, and pushes in the background.

Platform notes:
- Linux: `install daemon` writes a systemd user service.
- macOS: `install daemon` writes a launchd plist.
- Windows: `install daemon` creates a Windows service. This typically requires an elevated shell.

## Usage

```sh
gitsitter                          # show status for current repo
gitsitter status --global          # show status for all tracked repos
gitsitter sync                     # trigger immediate sync for current repo
gitsitter sync --all               # trigger immediate sync for all repos
gitsitter config                   # show global config (highlights current repo)
gitsitter resolve                  # interactively resolve sync issues
gitsitter resolve --global         # resolve issues across all repos
gitsitter enable                   # enable syncing (single-remote repos)
gitsitter enable <remote>          # enable a specific remote
gitsitter enable --all             # enable all remotes
gitsitter disable                  # disable syncing (single-remote repos)
gitsitter disable <remote>         # disable a specific remote
gitsitter disable --all            # disable all remotes
gitsitter trust <host>             # trust a remote host
gitsitter untrust <host>           # untrust a remote host
gitsitter log                      # show daemon log for current repo
gitsitter log --global             # show global daemon log
gitsitter daemon status            # check if daemon is running
gitsitter daemon start             # start daemon (service manager or detached)
gitsitter daemon stop              # graceful shutdown
```

## Configuration

Config file: `~/.config/gitsitter/config.toml`
On Windows: `%APPDATA%\gitsitter\config.toml`.

If that file does not exist yet, gitsitter creates it on first run from the shipped default config template.

```toml
[global]
refresh_interval = "60s"
colors = true
emoji = true
notification_cooldown = "5m"

[trusted_hosts]
"git.corp.internal" = true

# Per-repo overrides (optional)
[repos."/home/user/projects/my-app"]
refresh_interval = "30s"
# disabled = true          # disable the whole repo
# disabled = ["upstream"]  # disable specific remotes
```

That's it. No sync modes, no branch rules, no remote globs. Behavior is derived from git state and commit authorship.

### Trusted Hosts

The daemon never contacts untrusted hosts. Built-in trusted: `github.com`, `gitlab.com`, `codeberg.org`, `bitbucket.org`, `sr.ht`. Add your own in `[trusted_hosts]` or via `gitsitter trust <host>`.

### In-Repo Config

Teams can commit `.gitsitter.toml` to share a `refresh_interval` override. User config always takes precedence.

## Architecture

- **Repo identity:** keyed by canonicalized common git dir (worktree-safe)
- **Client-server:** CLI connects to daemon over local IPC, JSON protocol
  - Unix: Unix domain socket
  - Windows: named pipe
- **Hybrid git:** `git2` for fast reads (merge analysis, status, authorship), `git` CLI for network/writes (fetch, push, merge)
- **File watching:** `notify` crate watches `.git/refs/` and `.git/HEAD` for near-instant reaction to local commits
- **State:** in-memory only, no database
- **Config:** TOML (`~/.config/gitsitter/config.toml`) for human-editable settings

### Daemon Sync Loop

Per repo, per refresh interval:
1. Check repo exists, skip if missing or disabled
2. Check for in-progress git operations (rebase, merge, etc.), skip if found
3. Discover worktrees, build branch occupancy map
4. Fetch all trusted, non-disabled remotes
5. For each tracked branch: analyze merge status
   - Fast-forward possible → ff-merge (checked out) or update-ref (not checked out)
   - Local ahead + owned → push
   - Local ahead + not owned → record as actionable issue (unless local tip merges the remote tip → push)
   - Diverged + owned → force-push with lease (safe rebase workflow)
   - Diverged + not owned → record as actionable issue

### Shell Notifications

The shell hooks call `gitsitter _prompt` on every prompt. When there are unresolved issues (unowned branches with local commits, diverged branches), warnings are printed directly in the terminal:

```
gitsitter: 📦 ~/project main → origin/main has unpushed changes (last remote commit by someone else)
gitsitter: Run `gitsitter resolve` to resolve issues
```

`gitsitter resolve` walks through each issue interactively, offering to force-push (take ownership) or create a new branch from your commits.

### Branch Updates

Non-checked-out branches are updated via `git update-ref` (no checkout needed) — when you `git checkout feature-x`, it's already up to date.

## File Layout

```
~/.config/gitsitter/config.toml         # user configuration
~/.local/state/gitsitter/daemon.log     # daemon log
~/.local/state/gitsitter/daemon.pid     # PID file
$XDG_RUNTIME_DIR/gitsitter.sock         # Unix domain socket
```

Windows defaults:

```
%APPDATA%\gitsitter\config.toml         # user configuration
%LOCALAPPDATA%\gitsitter\daemon.log     # daemon log
%LOCALAPPDATA%\gitsitter\daemon.pid     # PID file
\\.\pipe\gitsitter-<user>               # named pipe endpoint
```

## Building

Requires Rust 2024 edition. A Nix flake is provided for reproducible builds:

```sh
# With nix
nix develop          # enter dev shell
nix build            # build the package

# With cargo
cargo build --release
cargo test
```

### Home Manager

The flake exports a Home Manager module at `homeManagerModules.default`.
Use this when you want Nix to manage:

- installing the `gitsitter` binary
- writing `~/.config/gitsitter/config.toml`
- creating the user systemd service (Linux) or launchd agent (macOS)
- adding shell integration for bash, zsh, or fish

Example:

```nix
{
  inputs.gitsitter.url = "git+ssh://git@github.com/mathijshenquet/gitsitter";

  outputs = { nixpkgs, home-manager, gitsitter, ... }: {
    homeConfigurations."my-user" = home-manager.lib.homeManagerConfiguration {
      pkgs = import nixpkgs { system = "x86_64-linux"; };
      modules = [
        gitsitter.homeManagerModules.default
        ({ ... }: {
          services.gitsitter = {
            enable = true;
            shellIntegration.fish = true;
            settings = {
              trusted_hosts."git.corp.internal" = true;
            };
          };
        })
      ];
    };
  };
}
```

When using the Home Manager module, prefer that over `gitsitter install shell` and
`gitsitter install daemon`, since those commands modify files imperatively outside
Nix.

## Supported Platforms

- **Linux** — systemd user service, inotify file watching
- **macOS** — launchd plist, FSEvents file watching
- **Windows** — Windows service support, named-pipe IPC, PowerShell shell hook

Known gap:
- Windows end-to-end daemon integration tests are not implemented yet. Unit and integration coverage for the core code paths does run on Windows.

## License

TBD
