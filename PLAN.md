
# gitsitter

A git utility that makes you forget the distinction between `BRANCH` and `origin/BRANCH`. Your local branches stay in sync with their tracking remotes — automatically, silently, and safely.

Rust CLI + daemon. The CLI controls and configures the daemon. The daemon watches registered git repos in the background, fetches from tracking remotes, and fast-forward merges in both directions:
- **Remote ahead → ff-merge into local** (pull)
- **Local ahead → push to remote** (push)
- **Diverged → do nothing**, never force-push, notify the user

## Philosophy

- **Silent by default.** The daemon should be invisible when everything is working.
- **Notify on problems.** Diverged branches, failed pushes, auth errors — surface these to the user.
- **Never destructive.** No force-push, no rebase, no reset. If ff is not possible, stop and tell the user.
- **Good git hygiene is assumed.** If you're on a tracked branch, treat commits as final. Use local branches for WIP/amend/squash workflows.

---

## Configuration

### Scopes (hierarchical override)

Configuration is resolved in order of specificity — more specific scopes override less specific ones:

1. **Global** (user-level) — default for all repos
2. **In-repo file** (`.gitsitter.toml` in repo root) — committable, shareable defaults for the repo
3. **User per-repo** (in `~/.config/gitsitter/config.toml`) — personal overrides, takes precedence over in-repo file
4. **Branch** (by branch name pattern, allow *, eg `feature/*`) — overrides repo for matching branches

The in-repo file lets teams share sensible defaults (e.g. "never auto-push main"). The user's personal config always wins over the in-repo file, so you can override team defaults locally.

### Sync modes (per scope)

| Mode | Fetch | Pull (ff-merge in) | Push |
|------|-------|---------------------|------|
| `none` | ❌ | ❌ | ❌ |
| `fetch` | ✅ | ❌ | ❌ |
| `pull` | ✅ | ✅ | ❌ |
| `push` | ✅ | ❌ | ✅ |
| `push+pull` | ✅ | ✅ | ✅ |

- `none` vs `fetch` distinction only exists at the repo level. At branch level, `none` means "don't sync this branch" while inheriting the repo's fetch behavior.
- If any branch has a sync mode other than `none`, the repo implicitly has at least `fetch`.
- **Default: `push+pull`**

### Repo-level special states

- **`disabled`** — stops all fetch/push/pull activity for this repo. The daemon ignores it entirely, but settings are retained.

### Global settings

| Setting | Scope | Default | Description |
|---------|-------|---------|-------------|
| `refresh_interval` | global, repo | `60s` | How often to check for changes |
| `auto_add` | global | `true` | Automatically register repos on `cd` |
| `colors` | global | `true` | Enable colored output |
| `emoji` | global | `true` | Enable emoji in output |
| `notification_cooldown` | global | `5m` | Minimum time between shell hook notifications per repo |

### In-repo config file (`.gitsitter.toml` or `.config/gitsitter.toml`)

If both exists: warning and use the one in `.config/gitsitter.toml`.

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
auto_add = true
colors = true
emoji = true
notification_cooldown = "5m"
mode = "push+pull"

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
- Shows current config with inheritance chain (global → repo → branch)
- Navigate settings with arrow keys
- Inline editing for values
- Changes are saved on exit, applied immediately by the daemon

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
gitsitter log --follow/-f              # follow mode (like tail -f)
gitsitter log --since "1h"             # filter by time
```

Example output:
```
[14:32:01] 📥 ~/projects/my-app  main: pulled 3 commits from origin/main
[14:32:05] 📤 ~/projects/my-app  feature-x: pushed 1 commit to origin/feature-x
[14:33:01] ⚠️  ~/projects/api-server  main: diverged from origin/main, ff not possible
[14:34:01] 📥 ~/vendor/some-lib  main: pulled 1 commit from origin/main
```

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
gitsitter uninstall [*]              # similarto above 
```

Interactive TUI when called without arguments — shows what will be installed and asks for confirmation.

### `gitsitter daemon`

Direct daemon (systemd) control (mostly for debugging / advanced use).

```
gitsitter daemon start                 # start the daemon
gitsitter daemon stop                  # stop the daemon
gitsitter daemon restart               # restart the daemon
gitsitter daemon status                # show daemon status (pid, uptime, repos watched)
```

---

## Shell hooks

### Prompt hook (post-command / pre-prompt)

Fires on every prompt display. Performs a cheap check:

1. Are we in a registered git repo?
2. Has it been longer than `notification_cooldown` since last notification for this repo?
3. Are there any diverged branches?

If all three: print a one-line warning. Example:

```
⚠️  gitsitter: feature-x has diverged from origin/feature-x (ff not possible)
```

The notification is rate-limited per repo (default: once per 5 minutes) to avoid noise. The check itself should be near-instant — read a small state file the daemon maintains, no git operations.

### Registration hook

if `auto_add` is enabled: call `gitsitter register` to ensure the repo is tracked. This is idempotent and fast.

### Supported shells

- bash
- zsh
- fish

---

## Daemon

### Architecture

- Single-user systemd user service (`systemd --user`)
- Long-running Rust process
- Watches registered repos on a configurable interval
- Maintains a state file per repo for cheap status checks by the CLI/shell hooks

### Sync loop (per repo, per refresh interval)

1. **Check repo exists** — if path is gone, mark repo as `missing`, log it, skip
2. **Check repo state** — if mid-rebase, mid-merge, mid-cherry-pick, or `.git/index.lock` exists: skip this cycle
3. **Fetch** from tracking remote(s)
4. **For each tracked branch:**
   a. Determine local and remote HEAD
   b. If equal: nothing to do
   c. If remote is ahead and ff-possible:
      - **Checked-out branch:** ff-merge (updates worktree)
      - **Non-checked-out branch:** `git update-ref` to move the ref forward (no checkout needed)
   d. If local is ahead: push (never force-push)
   e. If diverged: mark as diverged, log warning, update state file
5. **Write state file** with per-branch sync status and timestamps

### File watching

Use the `notify` crate to watch `.git/refs/heads/` and `.git/HEAD` for changes and index.lock. This allows near-instant reaction to local commits (for pushing) rather than waiting for the next polling interval. The polling interval remains as a fallback and for fetching remote changes. Once things change, depending on the thing, run a sync.

### Repo disappearance

When a repo path no longer exists:
- Mark as `missing` in state
- Log a warning
- Do not remove config (repo may have moved or drive may be unmounted)
- After configurable period (e.g. 7 days), notify user via shell hook that repo has been missing
- `gitsitter status --global` shows missing repos clearly

### State files

Per-repo state files in `~/.local/state/gitsitter/` (or equivalent XDG path):

```
~/.local/state/gitsitter/repos/<hash>/state.toml
```

Contains: branch sync status, last fetch/push/pull time, divergence info, last notification time. This is what the shell hook reads for cheap status checks.

### Logging

Append-only log file at `~/.local/state/gitsitter/daemon.log`.

Log levels: `info` (syncs, fetches, pushes), `warn` (divergence, missing repos, auth failures), `error` (crashes, unrecoverable).

Log rotation: configurable max size, default 10MB, keep 3 rotated files.

### Error handling

- **Auth failures:** log warning, skip repo for this cycle, notify user via shell hook
- **Network unavailable:** skip all fetches/pushes, retry next cycle silently
- **Lock contention (`.git/index.lock`):** skip repo for this cycle, no warning (user is probably mid-operation)
- **Corrupt repo state:** log error, disable repo, notify user

---

## Installation & file layout

```
~/.config/gitsitter/
  config.toml                    # user configuration

~/.local/state/gitsitter/
  daemon.log                     # daemon activity log
  daemon.pid                     # daemon PID file
  repos/
    <sha256-of-path>/
      state.toml                 # per-repo state (branch status, timestamps)

~/.config/systemd/user/
  gitsitter.service              # systemd user service file
```

Shell hook scripts are appended to the appropriate shell config file (`.bashrc`, `.zshrc`, `config.fish`).

---

## Crate dependencies (likely)

| Crate | Purpose |
|-------|---------|
| `git2` | Git operations (fetch, merge, push, status) |
| `notify` | Filesystem watching for `.git/` changes |
| `clap` | CLI argument parsing |
| `ratatui` | Terminal UI for interactive modes |
| `toml` / `serde` | Config parsing |
| `tracing` | Structured logging |
| `dirs` | XDG directory resolution |
| `colored` | Terminal colors (respects `colors` config) |

---

## Edge cases & design decisions

- **Detached HEAD:** If a user detaches HEAD (e.g. cloned repo just for inspection), no branch is checked out, so push+pull has nothing to act on. This naturally opts out of sync.
- **Non-checked-out branches:** Updated via `git update-ref` — no checkout needed. This is a core feature: when you `git checkout feature-x`, it's already up to date with remote. No more `git pull` after every checkout.
- **Non-tracking branches:** Branches without an upstream are ignored by the daemon. Only branches with a configured tracking remote are synced.
- **Multiple remotes:** Sync follows the branch's configured upstream remote. No attempt to sync with multiple remotes.
- **Shallow clones:** Should work for fetch/pull. Push from a shallow clone may fail — log and skip.
- **Worktrees:** Each worktree checkout is treated independently. The daemon watches the main `.git` dir.
- **Bare repos:** Not supported (no use case for auto-sync).
