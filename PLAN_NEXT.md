# gitsitter — Next Steps

## Current State (as of 2026-03-23)

~5400 lines of Rust across 13 source files. 52 tests passing. Core daemon loop, CLI, config resolution, file watcher, shell hooks, SQLite state, and Unix socket transport are all implemented and functional.

### What's Implemented

| Area | Status | Notes |
|------|--------|-------|
| CLI (all subcommands) | Done | status, config, enable/disable, log, sync, register, install/uninstall, daemon control, prompt hook |
| Config system | Done | TOML parsing, hierarchical resolution (global → in-repo → user-repo → branch), glob matching, host trust |
| Daemon | Done | Socket listener, sync loop, signal handling, shutdown, PID file |
| Sync loop | Done | Fetch, ff-merge (checked-out + update-ref), push, divergence detection, backoff |
| Git operations | Done | Hybrid git2 + CLI: merge analysis, worktree discovery, dirty checks, fetch/push/ff-merge/update-ref |
| File watcher | Done | notify-based, watches refs/heads, refs/remotes, HEAD; debounce; suppresses self-triggered events |
| Shell hooks | Done | bash/zsh/fish generation, install/uninstall with markers |
| State storage | Done | SQLite with WAL, repos/branches/worktrees/notification cooldowns |
| Transport | Done | Length-prefixed JSON over Unix socket |
| Path resolution | Done | XDG-compliant, env overrides for testing |
| Backoff | Done | Per-remote fetch backoff, per-ref push backoff, exponential with max 1h |
| Error categorization | Done | Push: rejected/auth/network/hook-timeout; Fetch: timeout/failure |
| Worktree support | Done | Multi-worktree discovery, branch occupancy map, dirty checks per worktree |
| Systemd integration | Done | Service file generation, start/stop via systemctl |

---

## Missing / Not Yet Implemented

### High Priority

1. **Log streaming (`gitsitter log --follow`)** — The daemon's `handle_log` currently reads the log file and returns last 100 lines as a single response. The `follow` flag is accepted but ignored. Needs: keep the socket open, stream `LogEntry` responses, send `LogEnd` on disconnect.

2. **Notification cooldowns in prompt hook** — The `handle_prompt` command checks branch status but doesn't use the `should_notify` / `record_notification` DB methods. Divergence warnings will repeat on every prompt instead of being rate-limited per the `notification_cooldown` setting.

3. **Config TUI** — `tui.rs` is empty. `gitsitter config` without flags just prints text. Plan calls for ratatui-based interactive config editing. Same for `gitsitter status --global` interactive TUI list.

4. **launchd support (macOS)** — `gitsitter install daemon` only generates a systemd unit. macOS needs a `com.gitsitter.daemon.plist` in `~/Library/LaunchAgents/`.

5. **`--since` filtering for `gitsitter log`** — The fallback path attempts to parse timestamps from log lines, but the daemon path ignores `since` entirely. Needs proper timestamp filtering both in daemon response and fallback.

### Medium Priority

6. **Stale branch cleanup in DB** — When branches are deleted locally, the DB retains stale entries. Should prune branches from the DB that no longer exist in the repo during each sync cycle.

7. **Log rotation** — Plan calls for configurable max size (10MB default, 3 rotated files). Currently logs grow unbounded when running in detached mode (`daemon start`). Should use `tracing-appender` with rotation.

8. **Repo disappearance notifications** — Missing repos are marked in the DB and status output, but the "after 7 days, notify user via shell hook" behavior isn't implemented.

9. **Config file watching** — The daemon loads config at startup and reloads on `ConfigUpdate` socket request. Plan mentions the daemon should pick up TOML changes automatically. Could use the file watcher or a simple mtime check.

10. **`gitsitter install` interactive TUI** — Currently requires subcommands (`shell`, `daemon`). Plan calls for an interactive installer when called with no arguments.

11. **Shutdown mechanism** — Uses a `__shutdown__` sentinel key in the repos HashMap, polled every 500ms. Should use a proper channel or atomic flag instead.

### Lower Priority

12. **Windows support** — Transport uses Unix sockets, paths use `libc::getuid()`. Would need named pipes and Windows-specific path handling. The plan mentions Windows but the current implementation is Unix-only.

13. **Multiple remotes** — Currently hardcoded to "origin". Plan says "sync follows the branch's configured upstream remote" — should extract the actual remote name from each branch's tracking config.

14. **Shallow clone handling** — No special handling. Plan says "push from shallow clone may fail — log and skip."

15. **`tracing-appender`** — Currently uses `tracing_subscriber::fmt` to stderr. The plan calls for structured logging to a file with rotation via `tracing-appender`.

16. **`gitsitter register` from prompt hook** — The shell hook runs `gitsitter register &` in the background, which spawns a process per prompt. This works but is slightly heavyweight. Could batch via a single socket message that's cheaper.

---

## Improvements / Tech Debt

### Code Quality

- **Daemon lock contention** — `daemon.db` is behind a `Mutex`, `daemon.repos` behind `RwLock`. The sync loop acquires and drops these locks repeatedly within a single repo sync (sometimes 3-4 times per branch). Consider: acquire locks once at the top of `sync_repo`, or batch DB writes.

- **Repetitive branch state updates** — The match arms in `sync_repo` for each `MergeAnalysis` variant each construct a `BranchState` and call `upsert_branch`. This creates ~200 lines of near-identical code. Extract a helper.

- **Shutdown sentinel hack** — Replace `__shutdown__` HashMap key with a proper `AtomicBool` or `watch` channel.

- **Mode Display formatting** — `format!("{:?}", mode).to_lowercase()` produces `pushpull` instead of `push+pull`. Should implement `Display` for `RepoSyncMode`.

### Robustness

- **Daemon start race** — `handle_daemon_start` spawns a detached process and immediately returns. No verification that the daemon actually started successfully or bound the socket.

- **Socket cleanup on crash** — If the daemon crashes without cleanup, the stale socket file blocks the next start. The daemon removes it on startup, but could also check if a PID file exists and whether that PID is alive.

- **Fetch path** — Uses the first worktree path for fetch, which may not be the main worktree. Should use the main working directory (from `repo.workdir()`).

### Testing

- **Integration test coverage** — There's one end-to-end daemon integration test. Could use more: multi-branch sync, divergence detection, push flow, backoff behavior, config resolution with in-repo files.

- **No tests for config resolution** — `resolve_repo_mode` and `resolve_branch_mode` are complex but untested beyond the glob matching tests.

### UX

- **`gitsitter config --explain` readability** — The resolution chain output is functional but doesn't match the nicely formatted plan example with tree lines and "← applied" markers.

- **Status output when not registered** — `gitsitter status` in an unregistered repo shows "repo not registered" but doesn't auto-register. The prompt hook does auto-register, so there's a gap if someone runs `gitsitter status` before the hook fires.

- **Error messages for untrusted hosts** — When a repo's remote is on an untrusted host, the status just shows `none` mode. Could be more helpful: "host X not trusted, run `gitsitter config --global` to add it".
