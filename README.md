# gitsitter

A background daemon that keeps your local branches in sync with their tracking remotes.

## How it works

Once running, gitsitter watches your repos and syncs branches in the background:

- **Remote is ahead** → fast-forward merge into your local branch
- **You're ahead** → push to remote
- **Diverged** → rebase onto remote and push

gitsitter decides whether it can push to remotes by checking who last committed to the remote tracking branch. If it thinks the committer is you (e.g. by email), then it assumes it can push to the branch. If it was someone else, gitsitter leaves it alone and warns you via your shell prompt.

When the daemon can't keep branches in sync automatically -- due to merge conflicts, remotes which look like they do not belong to you, etc -- it flags it. Run `gitsitter resolve` to walk through each issue interactively to resolve them.

Non-checked-out branches are updated in the background via `git update-ref`, so when you `git checkout feature-x` it's already up to date.

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
cargo install --git ssh://git@github.com/mathijshenquet/gitsitter
gitsitter install
```

This installs shell hooks and the background daemon service (launchd on macOS, systemd on Linux). Repos are auto-registered when you `cd` into them.

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
$XDG_RUNTIME_DIR/gitsitter.sock          # Unix domain socket
```

</details>

## Supported platforms

- **Linux** — systemd user service, inotify file watching
- **macOS** — launchd plist, FSEvents file watching

## License

MIT
