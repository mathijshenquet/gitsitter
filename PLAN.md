
# gitsitter

A git utility that makes you forget the distinction between `BRANCH` and `origin/BRANCH`. Your local branches stay in sync with their tracking remotes — automatically, silently, and safely.

Single Rust binary — acts as both CLI and daemon. The CLI controls and configures the daemon. The daemon watches registered git repos in the background, fetches from tracking remotes, and fast-forward merges in both directions:
- **Remote ahead → ff-merge into local** (pull)
- **Local ahead → push to remote** (push)
- **Diverged → do nothing**, never force-push, notify the user

## Philosophy

- **Silent by default.** The daemon should be invisible when everything is working.
- **Notify on problems.** Diverged branches, failed pushes, auth errors — surface these to the user.
- **Never destructive.** No force-push, no rebase, no reset. If ff is not possible, stop and tell the user.
- **Good git hygiene is assumed.** If you're on a tracked branch, treat commits as final. Use local branches for WIP/amend/squash workflows.

---

## Architecture

### Repo identity: common git dir

The daemon keys repositories by **canonicalized common git dir**, not by working tree path. This is critical for worktree support — multiple working trees share one logical repository.

- **`repo_id`** = canonicalized path to the common git dir (e.g. `/home/user/projects/my-app/.git`)
- **`worktrees`** = list of `{ path, current_head, clean/dirty, last_seen }` per repo
- **`display_path`** = preferred human-readable path for the repo (shortest worktree path, or the main working tree)

When a user `cd`s into a linked worktree, the shell hook resolves it to the same `repo_id` as the main working tree. One repo, one sync policy, one branch occupancy map across all worktrees. No duplicate registrations.

### Single binary, client-server model

`gitsitter` is one binary that acts as both daemon and CLI client.

- `gitsitter daemon run` — run the daemon in the foreground (used by service managers and for debugging)
- `gitsitter daemon start` — ask systemd/launchd to start the service, or spawn a detached fallback process
- All other commands — connect to the daemon via socket, send a request, print the response

### Communication: Unix domain socket / Windows named pipe

The daemon listens on a socket. The CLI connects, sends a JSON request, gets a JSON response. Simple length-prefixed `serde_json` messages — no protocol framework needed.

| Platform | Transport | Path |
|----------|-----------|------|
| Linux/macOS | Unix domain socket | `$XDG_RUNTIME_DIR/gitsitter.sock` or `/tmp/gitsitter-$UID.sock` |
| Windows | Named pipe | `\\.\pipe\gitsitter` |

Platform abstraction is a thin wrapper — everything else (git2, config, TUI) is cross-platform.

### Daemon is source of truth

- **On startup:** daemon reads config from TOML, loads last known state from SQLite into memory.
- **During operation:** all sync results update in-memory state. State is persisted to SQLite periodically and on clean shutdown.
- **CLI queries:** go through the socket → daemon replies from memory. Near-instant.
- **Config changes via CLI:** go through the socket → daemon updates in-memory config to TOML on disk.

### Daemon-down fallback

When the CLI can't connect to the socket:
- **Config commands:** CLI writes directly to the TOML file on disk. Daemon picks up changes on next start.
- **Status commands:** CLI reads from SQLite directly, with a warning: "daemon not running, showing cached state".
- **Shell hooks:** if socket connect fails (with short timeout ~20ms), silently skip. No notification is better than a hung terminal.

### State storage: SQLite

State (sync timestamps, branch status, divergence info, notification cooldowns) is stored in SQLite at `~/.local/state/gitsitter/state.db`.

Why SQLite over TOML state files:
- Atomic writes without temp-file-and-rename per repo
- Queryable (e.g. "all diverged branches across all repos")
- Single file, no directory of per-repo state files to manage
- The daemon writes frequently — SQLite handles concurrent read/write safely

The database is initialized with `PRAGMA journal_mode = WAL` and `PRAGMA synchronous = NORMAL` for safe concurrent read/write without locking.

Config stays in TOML — it's human-editable and version-controllable.

---

## Configuration

### Scopes (hierarchical override)

Configuration is resolved in order of specificity — more specific scopes override less specific ones:

1. **Global** (user-level) — default for all repos
2. **In-repo file** (`.gitsitter.toml` in repo root) — committable, shareable defaults for the repo
3. **User per-repo** (in `~/.config/gitsitter/config.toml`) — personal overrides, takes precedence over in-repo file
4. **Branch** (by branch name pattern, allow *, eg `feature/*`) — overrides repo for matching branches

The in-repo file lets teams share sensible defaults (e.g. "never auto-push main"). The user's personal config always wins over the in-repo file, so you can override team defaults locally.

### Sync modes

#### Repo-level modes

| Mode | Fetch | Pull (ff-merge in) | Push |
|------|-------|---------------------|------|
| `none` | ❌ | ❌ | ❌ |
| `fetch` | ✅ | ❌ | ❌ |
| `pull` | ✅ | ✅ | ❌ |
| `push` | ✅ | ❌ | ✅ |
| `push+pull` | ✅ | ✅ | ✅ |

#### Branch-level modes

Branches do not control fetch (fetch is repo-wide). Branch modes are:

| Mode | Pull | Push |
|------|------|------|
| `inherit` | from repo | from repo |
| `none` | ❌ | ❌ |
| `pull` | ✅ | ❌ |
| `push` | ❌ | ✅ |
| `push+pull` | ✅ | ✅ |

`inherit` is the implicit default when no branch rule matches.

- If any branch has a sync mode other than `none`, the repo implicitly has at least `fetch`.
- **Default: `pull`** — push is opt-in. Use remote globs (see below) to enable push for repos owned by you or your org.

### Remote-based trust model

All repos are **auto-registered** on `cd` — no opt-in needed. The sync mode is determined by matching the repo's remote URL(s) against remote globs:

- **Well-known hosts** (GitHub, GitLab, Codeberg, Bitbucket, SourceHut) ship as built-in `pull` defaults
- **Unknown remotes** default to `none` — no network activity. The shell hook shows a one-time notification: "repo X has unknown remote, run `gitsitter config` to enable sync"
- **`gitsitter status`** shows unsynced repos clearly with the reason (e.g. "remote not recognized")

This eliminates the need for a separate trust/enable ceremony or an `auto_add` setting. No background network operations happen against unfamiliar remotes.

### Remote globs

Remote URL globs let you set sync modes based on who owns the remote, rather than configuring per-repo:

```toml
[remotes]
# Built-in defaults (shipped with gitsitter, can be overridden):
# "*github.com*" = "pull"
# "*gitlab.com*" = "pull"
# "*codeberg.org*" = "pull"
# "*bitbucket.org*" = "pull"
# "*sr.ht*" = "pull"

# User config — evaluated before built-ins:
"git@github.com:myuser/*" = "push+pull"     # my repos: full sync
"git@github.com:myorg/*" = "push+pull"       # org repos: full sync
```

Remote globs are evaluated in order — first match wins. User-defined globs take priority over built-in defaults. They act as defaults that can be overridden by per-repo or per-branch config.

### Config resolution algorithm

**Repo effective mode** (evaluated top to bottom, first match wins):

1. `disabled=true` on user per-repo config → repo disabled
2. User per-repo `mode`
3. In-repo `.gitsitter.toml` `mode`
4. First matching user remote glob
5. First matching built-in remote glob
6. Fallback: `none`

**Branch effective mode** (evaluated top to bottom, first match wins):

1. Exact user per-repo branch rule
2. Longest matching user per-repo branch glob
3. Exact in-repo branch rule
4. Longest matching in-repo branch glob
5. Exact global branch rule
6. Longest matching global branch glob
7. Repo effective mode (inherit)

Within any layer: exact name beats glob, longer glob beats shorter, declaration order breaks ties.

### Repo-level special states

- **`disabled`** — stops all fetch/push/pull activity for this repo. The daemon ignores it entirely, but settings are retained.

### Global settings

| Setting | Scope | Default | Description |
|---------|-------|---------|-------------|
| `refresh_interval` | global, repo | `60s` | How often to check for changes |
| `colors` | global | `true` | Enable colored output |
| `emoji` | global | `true` | Enable emoji in output |
| `notification_cooldown` | global | `5m` | Minimum time between shell hook notifications per repo |
| `git_path` | global | `null` (auto-detect) | Path to git binary. If unset, uses `git` from `$PATH` |

### In-repo config file (`.gitsitter.toml`)

Located at the repo root. One path, no ambiguity.

```toml
mode = "pull"                    # repo-level default: pull only
refresh_interval = "30s"

[branches]
main = "pull"                    # never auto-push main
"release/*" = "pull"
"feature/*" = "push+pull"
```

The user's personal `~/.config/gitsitter/config.toml` overrides anything in `.gitsitter.toml`. This means a team can set conservative defaults while individual devs can opt into more aggressive sync.

### User config file

TOML format, stored in `~/.config/gitsitter/config.toml`:

```toml
[global]
refresh_interval = "60s"
colors = true
emoji = true
notification_cooldown = "5m"

[remotes]
"git@github.com:myuser/*" = "push+pull"    # my repos: full sync
"git@github.com:myorg/*" = "push+pull"     # org repos: full sync

[branches]
"temp/*" = "none" # ignore temp branches

[repos."/home/user/projects/my-app"]
mode = "push+pull"
refresh_interval = "30s"

[repos."/home/user/projects/my-app".branches]
main = "pull"          # don't auto-push to main
"feature/*" = "push+pull"
"tmp/*" = "none"       # ignore temp branches

[repos."/home/user/vendor/some-lib"]
mode = "pull"          # read-only, never push

[repos."/home/user/old-project"]
disabled = true
```

---

## CLI

### Invocation style

All commands support both CLI flags and interactive TUI. When flags are omitted, the command drops into an interactive interface where applicable. This way power users can script with flags, while interactive use doesn't require memorizing arguments.

### `gitsitter` / `gitsitter status`

Context-aware status display.

**Inside a git repo:**
```
📦 ~/projects/my-app  (push+pull, synced 30s ago)

  main        ← origin/main        ✅ synced (pulled 2m ago)
  feature-x   ← origin/feature-x   ⚠️  diverged (ff not possible)
  hotfix      ← origin/hotfix      ✅ synced (pushed 45s ago)
  tmp/scratch                       ⏸️  untracked
```

**Outside a git repo (or with `--global/-g`):**
```
📊 gitsitter — 12 repos tracked, daemon running

  ~/projects/my-app        push+pull   ✅ 10 synced       30s ago
  ~/projects/api-server    push+pull   ⚠️  1/9 diverged      1m ago
  ~/vendor/some-lib        pull        ✅ 5 synced       5m ago
  ~/old-project            disabled    ⏸️  —               —
  ~/sketchy/repo           none        ⚠️  unknown remote   —
```

The global view is an interactive TUI list — navigate with arrow keys, press Enter to drill into a repo, `d` to disable, `e` to enable, `c` to configure.

### `gitsitter config`

Without arguments: opens interactive TUI for configuring the current scope.

```
gitsitter config                       # TUI: configure current repo (if in repo) or global
gitsitter config --global/-g           # TUI: configure global settings
gitsitter config --repo/-r <mode>      # set sync mode for current repo
gitsitter config --branch/-b <mode>    # set sync mode for current branch
gitsitter config --branch/-b <name> <mode>  # set sync mode for a specific branch
```

**Interactive TUI (`gitsitter config`):**
- Shows current config with inheritance chain (global → in-repo → user-repo → branch)
- Navigate settings with arrow keys
- Inline editing for values
- Changes are saved on exit, sent to daemon via socket for immediate application

### `gitsitter enable` / `gitsitter add`

Enable or register a repo. Without arguments, acts on the current repo.

```
gitsitter enable                       # enable current repo
gitsitter enable /path/to/repo         # enable specific repo
```

### `gitsitter disable` / `gitsitter remove` / `gitsitter rm`

Disable a repo (stops all daemon activity, preserves config).

```
gitsitter disable                      # disable current repo
gitsitter disable /path/to/repo        # disable specific repo
gitsitter disable --purge              # disable and remove config, warning to not use (since it will nuke settings and might auto add)
```

### `gitsitter log`

Show daemon activity log.

```
gitsitter log                          # tail log for current repo
gitsitter log --global/-g              # tail global daemon log
gitsitter log --follow/-f              # stream live from daemon via socket
gitsitter log --since "1h"             # filter by time
```

Example output:
```
[14:32:01] 📥 ~/projects/my-app  main: pulled 3 commits from origin/main
[14:32:05] 📤 ~/projects/my-app  feature-x: pushed 1 commit to origin/feature-x
[14:33:01] ⚠️  ~/projects/api-server  main: diverged from origin/main, ff not possible
[14:34:01] 📥 ~/vendor/some-lib  main: pulled 1 commit from origin/main
```

`gitsitter log --follow` streams directly from the daemon over the socket — no log file tailing.

### `gitsitter sync`

Trigger an immediate sync, bypassing the refresh interval timer.

```
gitsitter sync                         # sync current repo now
gitsitter sync --all                   # sync all repos now
```

Sends a message to the daemon: "ignore the timer, run a sync cycle for this repo right now."

### `gitsitter register`

Adds a repo to gitsitter with default/implied settings. Generally not called by a human — invoked by shell hooks and implicitly by other commands.

```
gitsitter register                     # register current repo
gitsitter register /path/to/repo       # register specific repo
```

### `gitsitter install`

Install the daemon and shell hooks. Detects current shell automatically.

```
gitsitter install                    # TUI, ask to install all or specify parts, auto-detect shell
gitsitter install shell fish         # explicit shell selection
gitsitter install shell              # autodetect
gitsitter install daemon             # only install systemd service
gitsitter install hooks              # only install shell hooks
```

```
gitsitter uninstall                    # TUI: select what to uninstall
gitsitter uninstall shell              # remove shell hooks
gitsitter uninstall daemon             # remove systemd/launchd service
gitsitter uninstall hooks              # remove shell hooks only
```

Interactive TUI when called without arguments — shows what will be installed and asks for confirmation.

### `gitsitter daemon`

Direct daemon control (mostly for debugging / advanced use).

```
gitsitter daemon run                   # run daemon in foreground (for service managers / debugging)
gitsitter daemon start                 # start via service manager, or spawn detached fallback
gitsitter daemon stop                  # stop the daemon (via socket, graceful shutdown)
gitsitter daemon restart               # restart the daemon
gitsitter daemon status                # show daemon status (pid, uptime, repos watched)
```

---

## Shell hooks

### Prompt hook (post-command / pre-prompt)

Fires on every prompt display. Connects to daemon socket with ~20ms timeout:

1. Are we in a registered git repo?
2. Has it been longer than `notification_cooldown` since last notification for this repo?
3. Are there any diverged branches or pending warnings?

If all three: print a one-line warning. Example:

```
⚠️  gitsitter: feature-x has diverged from origin/feature-x (ff not possible)
```

The notification is rate-limited per repo (default: once per 5 minutes) to avoid noise. If the socket connect fails, silently skip — never hang the terminal.

### Registration hook

On every prompt, calls `gitsitter register` to ensure the repo is tracked. This is idempotent and fast. For repos with unrecognized remotes, a one-time notification is shown suggesting `gitsitter config`.

### Supported shells

- bash
- zsh
- fish

---

## Daemon

### Sync loop (per repo, per refresh interval)

1. **Check repo exists** — if path is gone, mark repo as `missing`, log it, skip
2. **Check repo state** — if any in-progress git operation is detected, skip this cycle. Check for: `.git/index.lock`, `.git/rebase-merge/`, `.git/rebase-apply/`, `.git/MERGE_HEAD`, `.git/CHERRY_PICK_HEAD`, `.git/BISECT_LOG`
3. **Discover worktrees** — enumerate linked worktrees via `git2` to build branch→worktree occupancy map
4. **Fetch** from tracking remote(s)
5. **For each tracked branch** (only `refs/heads/*` — ignore `refs/stash`, `refs/bisect/*`, `refs/notes/*`):
   a. Determine local and remote HEAD
   b. If upstream ref is gone (deleted remotely): mark as `upstream_gone`, warn once, stop syncing this branch. Never delete the local branch.
   c. If equal: nothing to do
   d. If remote is ahead and ff-possible:
      - **Checked-out branch (in any worktree):** check that worktree is clean first. If dirty, mark as "pending ff (worktree dirty)", skip. If clean, ff-merge (updates worktree and index).
      - **Non-checked-out branch:** `git update-ref` with expected-old-OID to move the ref forward (no checkout needed). This is a core feature — when you `git checkout feature-x`, it's already up to date.
   e. If local is ahead: push (never force-push). Respect backoff state (per-ref for push rejections, per-remote for auth/network).
   f. If diverged: mark as diverged, log warning, update state
6. **Persist state** to SQLite

### File watching

Use the `notify` crate to watch `.git/refs/heads/` and `.git/HEAD` for changes and index.lock. This allows near-instant reaction to local commits (for pushing) rather than waiting for the next polling interval. The polling interval remains as a fallback and for fetching remote changes. Once things change, depending on the thing, run a sync.

Events must be debounced (2-3 seconds after last event) before triggering a sync cycle, since git operations can produce thousands of filesystem events (e.g. during rebase or large merges).

### Git strategy: hybrid git2 + git CLI

Two layers for interacting with git:

- **`git2` (libgit2)** — for fast, in-process read-only operations: merge analysis (`is_ancestor`), worktree status checks, ref inspection, branch enumeration. No auth needed, no fork/exec overhead.
- **`git` CLI** — for all network and write operations: `fetch`, `push`, `merge --ff-only`, `update-ref`. This gets SSH config, credential helpers, GPG signing, and proxy settings for free — no reimplementation needed.

The `git_path` config option lets users point to a specific git binary. If unset, the daemon uses `git` from `$PATH`. On startup, the daemon logs the detected git version.

All daemon-spawned git CLI commands set `GIT_TERMINAL_PROMPT=0` and have stdin redirected from `/dev/null` to prevent hanging on passphrase prompts or other TTY input requests. Auth, SSH config, credential helpers — all handled by the git CLI transparently.

### Repo disappearance

When a repo path no longer exists:
- Mark as `missing` in state
- Log a warning
- Do not remove config (repo may have moved or drive may be unmounted)
- After configurable period (e.g. 7 days), notify user via shell hook that repo has been missing
- `gitsitter status --global` shows missing repos clearly

### Logging

Structured logging via `tracing`. Logs written to `~/.local/state/gitsitter/daemon.log`.

Log levels: `info` (syncs, fetches, pushes), `warn` (divergence, missing repos, auth failures), `error` (crashes, unrecoverable).

Log rotation: configurable max size, default 10MB, keep 3 rotated files.

`gitsitter log --follow` streams log entries from the daemon over the socket in real time.

### Error handling

- **Auth failures:** log warning, skip repo for this cycle, notify user via shell hook. **Per-remote** exponential backoff — double the retry interval on each consecutive failure (max 1h). Reset on success.
- **Protected branch / server-side rejection:** categorize the push failure. **Per-ref** exponential backoff (max 1h) — one protected branch does not suppress healthy pushes to other refs on the same remote. Notify user.
- **Network unavailable:** skip all fetches/pushes, retry next cycle silently. **Per-remote** exponential backoff on retries (fetch can stay at normal interval).
- **Lock contention (`.git/index.lock`):** skip repo for this cycle, no warning (user is probably mid-operation)
- **Dirty worktree on checked-out branch:** fetch and update-ref other branches, but skip ff-merge on checked-out branch. Mark as "pending ff (worktree dirty)". Retry next cycle.
- **Corrupt repo state:** log error, disable repo, notify user
- **Slow/failing git hooks:** Daemon-initiated `push` and `merge --ff-only` commands use a configurable timeout (default 30s, per-repo). If the command times out (e.g. a `pre-push` hook running a test suite), treat the branch the same as diverged — mark as "push blocked (hook timeout)" or "pull blocked (hook timeout)" in state, and surface via shell hook notifications. This preserves hook semantics (formatters still run) while preventing the daemon from hanging indefinitely.

---

## Platform support

### Daemon management

| Platform | Service manager | Fallback |
|----------|----------------|----------|
| Linux | systemd user service | Background process + PID file |
| macOS | launchd (plist) | Background process + PID file |
| Windows | Windows Service / Task Scheduler | Background process |

### File watching

The `notify` crate handles platform differences automatically:
- Linux: inotify
- macOS: FSEvents
- Windows: ReadDirectoryChangesW

---

## Installation & file layout

```
~/.config/gitsitter/
  config.toml                    # user configuration

~/.local/state/gitsitter/
  state.db                       # SQLite state database
  daemon.log                     # daemon activity log
  daemon.pid                     # daemon PID file

$XDG_RUNTIME_DIR/ (or /tmp/)
  gitsitter.sock                 # Unix domain socket (Linux/macOS)

~/.config/systemd/user/
  gitsitter.service              # systemd user service file (Linux)

~/Library/LaunchAgents/
  com.gitsitter.daemon.plist     # launchd plist (macOS)
```

Shell hook scripts are appended to the appropriate shell config file (`.bashrc`, `.zshrc`, `config.fish`).

---

## Crate dependencies (likely)

| Crate | Purpose |
|-------|---------|
| `git2` | Local repo reading (merge analysis, status, ref inspection) |
| `notify` | Filesystem watching for `.git/` changes |
| `clap` | CLI argument parsing |
| `ratatui` | Terminal UI for interactive modes |
| `toml` / `serde` / `serde_json` | Config parsing + socket protocol |
| `rusqlite` | State storage |
| `tokio` | Async runtime, Unix socket / named pipe |
| `tracing` + `tracing-appender` | Structured logging with rotation |
| `dirs` | XDG directory resolution |

---

## Distribution

### Nix flake

The project includes a `flake.nix` for reproducible builds and easy installation:

- `nix run github:user/gitsitter` — run directly
- `nix profile install github:user/gitsitter` — install to user profile
- Dev shell with all build dependencies (`rust`, `pkg-config`, `openssl`, `libgit2`, `sqlite`)
- Outputs: package, overlay, NixOS/home-manager module (configures systemd user service automatically)

### Other distribution

- Cargo: `cargo install gitsitter`
- Pre-built binaries: GitHub releases (Linux x86_64/aarch64, macOS x86_64/aarch64, Windows x86_64)
- AUR (Arch Linux)

---

## Edge cases & design decisions

- **Detached HEAD:** If a user detaches HEAD (e.g. cloned repo just for inspection), no branch is checked out, so push+pull has nothing to act on. This naturally opts out of sync.
- **Non-checked-out branches:** Updated via `git update-ref` — no checkout needed. This is a core feature: when you `git checkout feature-x`, it's already up to date with remote. No more `git pull` after every checkout.
- **Dirty worktree:** "Dirty" means any staged or unstaged changes to tracked files. Untracked files do not block ff-merge. If the checked-out branch has tracked-file changes and remote is ahead, the daemon skips the ff-merge and marks the branch as "pending ff (worktree dirty)". It retries next cycle. Non-checked-out branches are still updated normally.
- **Remote-deleted upstream:** If a branch's upstream ref was deleted remotely, mark as `upstream_gone`. Never delete the local branch. Warn once via shell hook. Stop syncing that branch until the upstream is restored or the user changes tracking config.
- **Non-tracking branches:** Branches without an upstream are ignored by the daemon. Only branches with a configured tracking remote are synced.
- **Multiple remotes:** Sync follows the branch's configured upstream remote. No attempt to sync with multiple remotes.
- **Shallow clones:** Should work for fetch/pull. Push from a shallow clone may fail — log and skip.
- **Worktrees:** The daemon discovers all linked worktrees (`git worktree list`) and tracks branch occupancy across them. A branch checked out in *any* worktree is treated as a checked-out branch — it is never advanced via `update-ref` silently. Instead, it follows the checked-out-branch path: check worktree cleanliness, then ff-merge (which updates the worktree and index), or skip if dirty. Non-checked-out branches (not occupied by any worktree) are still advanced via `update-ref` — this remains a core feature. Worktree discovery is refreshed each sync cycle.
- **update-ref safety:** All `update-ref` calls use expected-old-OID semantics (`git update-ref <ref> <new> <old>`) to avoid races. If the ref moved since the daemon last read it, the update fails safely and retries next cycle.
- **Bare repos:** Not supported (no use case for auto-sync).
