# gitsitter

A git utility that keeps your local branches in sync with their tracking remotes — automatically, silently, and safely. Single Rust binary that acts as both CLI and background daemon.

- **Remote ahead** → fast-forward merge into local (pull)
- **Local ahead** → push to remote
- **Diverged** → do nothing, never force-push, notify the user

## Philosophy

- **Silent by default.** Invisible when everything is working.
- **Notify on problems.** Diverged branches, failed pushes, auth errors — surfaced to the user.
- **Never destructive.** No force-push, no rebase, no reset. If ff is not possible, stop and tell the user.

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

Once the shell hook is installed, repos are auto-registered when you `cd` into them. The daemon fetches, pulls, and pushes in the background based on your configuration.

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
gitsitter config                   # show config for current repo
gitsitter config --global          # show global config
gitsitter config --repo pull       # set current repo to pull-only
gitsitter config --branch push+pull  # set current branch mode
gitsitter config --explain         # show full config resolution chain
gitsitter enable                   # enable current repo
gitsitter disable                  # disable current repo
gitsitter log                      # show daemon log for current repo
gitsitter log --global             # show global daemon log
gitsitter daemon status            # check if daemon is running
gitsitter daemon start             # start daemon (service manager or detached)
gitsitter daemon stop              # graceful shutdown
```

## Configuration

Config file: `~/.config/gitsitter/config.toml`
On Windows: `%APPDATA%\gitsitter\config.toml`.

If that file does not exist yet, gitsitter creates it on first run from the
shipped default config template, including commented examples.

```toml
[global]
refresh_interval = "60s"
colors = true
emoji = true
notification_cooldown = "5m"

[trusted_hosts]
"git.corp.internal" = true

[[defaults.remotes]]
pattern = "git@github.com:myuser/*"
mode = "push+pull"

[[defaults.remotes]]
pattern = "git@github.com:myorg/*"
mode = "push+pull"

[[defaults.branches]]
pattern = "temp/*"
mode = "none"

[repos."/home/user/projects/my-app"]
mode = "push+pull"
refresh_interval = "30s"

[[repos."/home/user/projects/my-app".branches]]
pattern = "main"
mode = "pull"

[[repos."/home/user/projects/my-app".branches]]
pattern = "feature/*"
mode = "push+pull"
```

### Sync Modes

**Repo-level:** `none`, `fetch`, `pull`, `push`, `push+pull`

**Branch-level:** `inherit`, `none`, `pull`, `push`, `push+pull`

Default is `pull` (push is opt-in). Branches default to `inherit` from their repo.

### Config Resolution

Repo mode is resolved top-down (first match wins):
1. Remote host not trusted → `none`
2. User per-repo `disabled=true` → disabled
3. User per-repo mode
4. In-repo `.gitsitter.toml` mode
5. First matching `defaults.remotes` glob
6. Fallback: `pull`

### Trusted Hosts

The daemon never contacts untrusted hosts. Built-in trusted: `github.com`, `gitlab.com`, `codeberg.org`, `bitbucket.org`, `sr.ht`. Add your own in `[trusted_hosts]`.

### In-Repo Config

Teams can commit `.gitsitter.toml` to share defaults. User config always takes precedence. In-repo config cannot set trusted hosts or remote defaults (security boundary).

## Architecture

- **Repo identity:** keyed by canonicalized common git dir (worktree-safe)
- **Client-server:** CLI connects to daemon over local IPC, JSON protocol
  - Unix: Unix domain socket
  - Windows: named pipe
- **Hybrid git:** `git2` for fast reads (merge analysis, status), `git` CLI for network/writes (fetch, push, merge)
- **File watching:** `notify` crate watches `.git/refs/` and `.git/HEAD` for near-instant reaction to local commits
- **State:** SQLite (`~/.local/state/gitsitter/state.db`) for sync timestamps, branch status, worktree info
- **Config:** TOML (`~/.config/gitsitter/config.toml`) for human-editable settings

### Daemon Sync Loop

Per repo, per refresh interval:
1. Check repo exists, skip if missing
2. Check for in-progress git operations (rebase, merge, etc.), skip if found
3. Discover worktrees, build branch occupancy map
4. Fetch from origin
5. For each tracked branch: analyze merge status, then ff-merge / update-ref / push / mark diverged
6. Persist state to SQLite

Non-checked-out branches are updated via `git update-ref` (no checkout needed) — when you `git checkout feature-x`, it's already up to date.

## File Layout

```
~/.config/gitsitter/config.toml         # user configuration
~/.local/state/gitsitter/state.db       # SQLite state database
~/.local/state/gitsitter/daemon.log     # daemon log
~/.local/state/gitsitter/daemon.pid     # PID file
$XDG_RUNTIME_DIR/gitsitter.sock         # Unix domain socket
```

Windows defaults:

```
%APPDATA%\gitsitter\config.toml         # user configuration
%LOCALAPPDATA%\gitsitter\state.db       # SQLite state database
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

The flake also exports a Home Manager module at `homeManagerModules.default`.
Use this when you want Nix to manage:

- installing the `gitsitter` binary
- writing `~/.config/gitsitter/config.toml`
- creating the user `systemd` service
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
            settings = {
              global.refresh_interval = "60s";
              trusted_hosts."github.com" = true;
            };
          };
        })
      ];
    };
  };
}
```

The module merges your `services.gitsitter.settings` over a small default config:

```toml
[global]
refresh_interval = "60s"
colors = true
emoji = true
notification_cooldown = "5m"
```

That means you can override just the pieces you care about instead of repeating the
full file.

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
