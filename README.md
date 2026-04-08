# gitsitter

A background daemon that keeps your local branches in sync with their tracking remotes.

## How it works

One common annoyance with Git is starting work on a branch only to discover it's stale. Git doesn't automatically sync with the remote — your local branches and your view of what's on the remote can be out of date without you knowing.

```sh
$ git checkout feature/search # not up-to-date with remote
# ... make commits ...
$ git push
# surprise: you have to pull and resolve conflicts
```

```sh
$ git checkout feature/search
$ git rebase origin/main # not up-to-date with remote
# ... slog through conflicts ...
# surprise: your local view of origin/main was stale because you didn't fetch
# now you have to merge or redo the rebase
```

gitsitter helps prevent these situations by fetching in the background and keeping your local branches up to date automatically when it's safe, and surfacing problems early when it's not.

After install, gitsitter adds a system daemon and a small shell hook. When you `cd` into a Git repo, the hook registers it with the daemon:

```sh
$ cd ~/work/app
gitsitter: Registered repo 📦 ~/work/app, syncing tracking branches on origin
```

From there, the daemon keeps syncing in the background. At a high level, it applies three rules:

- **Remote is ahead** → fast-forward your local branch
- **You're ahead** → push to remote
- **Diverged** → rebase onto remote and push

This also applies to branches you're not currently on:

```sh
$ git checkout bugfix/login-timeout
# meanwhile, someone pushes to feature/search on the remote
# gitsitter fetches and fast-forwards your local feature/search in the background

$ git checkout feature/search
# already up to date — no stale surprises
```

The important qualifier is "when it's safe". If gitsitter isn't confident it should write to a remote branch, it stops and tells you instead of guessing.

For example, suppose your local branch is ahead, but the last commit on the remote looks like someone else's work. gitsitter won't auto-push — that could disrupt some else's workflow.

This protects shared and review branches where auto-pushing would be surprising. gitsitter decides whether it can auto-push by checking who last committed on the remote. If that looks like you (out of the box, by email), it treats the branch as safe to push. If not, it leaves it alone and warns you via your shell prompt.

```sh
$ cd ~/work/app
gitsitter: 📦 ~/work/app feature/search has unpushed changes (last remote commit by someone else)
gitsitter: Run `gitsitter resolve` to resolve issues
```

When the daemon can't sync a branch automatically — due to merge conflicts, ambiguous ownership, etc — it flags it. Run `gitsitter resolve` to walk through each issue interactively.

## Integrations

### GitHub

Enable GitHub integration to relax the committer-email check. When enabled, gitsitter uses `gh` to verify your identity against your GitHub verified emails and checks PR authorship — so branches are synced even when commit emails don't match exactly.

With Nix: `githubIntegration.enable = true`
Without Nix: ensure `gh` is authenticated and on the daemon's PATH (it's auto-discovered at install time). When adding this later, rerun `gitsitter install`.

### Resolve agent

When a rebase produces conflicts, gitsitter can invoke an AI agent to resolve them automatically. Configure which tool to use (default and only for now: `claude`):

With Nix: `resolveAgent.enable = true`
Without Nix: set `resolve_agent = "claude"` in your config.


## Getting started

### With Nix (recommended)

The flake exports a Home Manager module that manages the binary, config, shell integration, and daemon service declaratively:

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
            shellIntegration.zsh = true;
            shellIntegration.fish = true;
            shellIntegration.bash = true;
            githubIntegration.enable = true;
            resolveAgent = {
              enable = true;
              tool = "claude";
            };
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

### Without Nix

```sh
curl -fsSL https://raw.githubusercontent.com/mathijshenquet/gitsitter/main/install.sh | sh
```

This downloads a pre-built binary to `~/.local/bin`, installs shell hooks and the background daemon service (launchd on macOS, systemd on Linux). Repos are auto-registered when you `cd` into them.

To update, run `gitsitter self-update`. The daemon will also notify you when a new version is available.

<details>
<summary>Other install methods</summary>

```sh
# From source (Linux, macOS, or Windows)
cargo install --git ssh://git@github.com/mathijshenquet/gitsitter
gitsitter install

# Specific version or custom path (Linux/macOS only)
curl -fsSL https://raw.githubusercontent.com/mathijshenquet/gitsitter/main/install.sh | sh -s -- --version v0.2.0
curl -fsSL https://raw.githubusercontent.com/mathijshenquet/gitsitter/main/install.sh | sh -s -- --path /usr/local/bin
```

</details>

Running `gitsitter install` again updates everything in place.

gitsitter can be configured by the file: `~/.config/gitsitter/config.toml`

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

### Trusted hosts

The daemon never contacts untrusted hosts. Built-in trusted: `github.com`, `gitlab.com`, `codeberg.org`, `bitbucket.org`, `sr.ht`. Add your own in `[trusted_hosts]` or via `gitsitter trust <host>`.

### In-repo config

Teams can commit `.gitsitter.toml` to share a `refresh_interval` override. User config always takes precedence.

## Usage

```sh
gitsitter                    # show status for current repo
gitsitter resolve            # interactively resolve sync issues
gitsitter resolve --global   # resolve issues across all repos
```

<details>
<summary>All commands</summary>

```sh
gitsitter status --global    # show status for all tracked repos
gitsitter sync               # trigger immediate sync for current repo
gitsitter sync --all         # trigger immediate sync for all repos
gitsitter config             # show global config
gitsitter enable             # enable syncing (single-remote repos)
gitsitter enable <remote>    # enable a specific remote
gitsitter enable --all       # enable all remotes
gitsitter disable            # disable syncing
gitsitter disable <remote>   # disable a specific remote
gitsitter disable --all      # disable all remotes
gitsitter trust <host>       # trust a remote host
gitsitter untrust <host>     # untrust a remote host
gitsitter log                # show daemon log
gitsitter daemon status      # check if daemon is running
gitsitter daemon start       # start daemon
gitsitter daemon stop        # graceful shutdown
gitsitter self-update        # update to latest release
gitsitter install            # install shell hooks and daemon service
gitsitter install shell      # install only shell hooks
gitsitter install daemon     # install only daemon service
gitsitter uninstall          # remove shell hooks and daemon service
```

</details>

## Building

Requires Rust 2024 edition. A Nix flake is provided:

```sh
nix develop          # enter dev shell
nix build            # build the package
cargo build --release
cargo test
```

<details>
<summary>Architecture</summary>

- **Client-server:** CLI connects to daemon over local IPC (Unix socket / Windows named pipe), JSON protocol
- **Hybrid git:** `git2` for fast reads (merge analysis, status, authorship), `git` CLI for network/writes (fetch, push, rebase)
- **File watching:** `notify` crate watches `.git/refs/` and `.git/HEAD` for near-instant reaction to local commits
- **State:** in-memory only, no database
- **Repo identity:** keyed by canonicalized common git dir (worktree-safe)

### Daemon sync loop

Per repo, per refresh interval:
1. Check repo exists, skip if missing or disabled
2. Skip if git operation in progress (rebase, merge, etc.)
3. Discover worktrees, build branch occupancy map
4. Fetch all trusted, non-disabled remotes
5. For each tracked branch: analyze merge status
   - Fast-forward possible → ff-merge (checked out) or update-ref (not checked out)
   - Local ahead + your branch → push
   - Local ahead + not your branch → flag as issue (unless local tip merges the remote tip → push)
   - Diverged + your branch → rebase and push
   - Diverged + not your branch → flag as issue

### File layout

```
~/.config/gitsitter/config.toml          # user configuration
~/.local/state/gitsitter/daemon.log      # daemon log
~/.local/state/gitsitter/daemon.pid      # PID file
$XDG_RUNTIME_DIR/gitsitter.sock          # Unix domain socket (Linux/macOS)
\\.\pipe\gitsitter-<user>                 # named pipe (Windows)
```

</details>

## Supported platforms

- **Linux** — systemd user service, inotify file watching
- **macOS** — launchd plist, FSEvents file watching
- **Windows** (experimental) — Windows Service via `sc.exe`, named-pipe IPC, PowerShell/pwsh shell hooks. Builds and passes CI, but must be installed from source (`cargo install`). The install script, pre-built release binaries, and `self-update` do not support Windows yet.

## License

MIT
