# gitsitter — Next Steps

## Current State (as of 2026-03-23)

~5400 lines of Rust across 13 source files. 52 tests passing. Core daemon loop, CLI, config resolution, file watcher, shell hooks, SQLite state, and Unix socket transport are all implemented and functional.

### What's Implemented

| Area | Status | Notes |
|------|--------|-------|
| CLI (all subcommands) | Done | status, config, enable/disable, log, sync, register, install/uninstall, daemon control, prompt hook |
| Config system | Done | TOML parsing, hierarchical resolution (global -> in-repo -> user-repo -> branch), glob matching, host trust |
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
| Systemd/launchd integration | Done | Service file generation for both platforms, start/stop via systemctl or launchctl |

---

## TODO

### High Priority

1. ~~**Notification cooldowns in prompt hook**~~ — **Done.** `handle_prompt` now checks `should_notify` / `record_notification` with per-branch cooldown keys.

2. **Config TUI** — `tui.rs` is empty. `gitsitter config` without flags just prints text. Plan calls for ratatui-based interactive config editing. Same for `gitsitter status --global` interactive TUI list.

3. ~~**launchd support (macOS)**~~ — **Done.** `gitsitter install daemon` generates a launchd plist on macOS, systemd unit on Linux. `daemon start` tries `launchctl` on macOS before falling back to direct spawn.

4. ~~**Multiple remotes**~~ — **Done.** `RepoBranch` now reads `branch.<name>.remote` from git config (defaults to "origin"). Fetch collects unique remotes across all branches. Push uses the branch's configured remote.

5. ~~**Fold register into `_prompt` command**~~ — **Done.** New `PromptCheck` request combines register + status in one daemon call. Shell hooks (bash/zsh/fish) no longer spawn a separate `gitsitter register` process.

### Medium Priority

6. ~~**Shutdown mechanism cleanup**~~ — **Done.** Replaced `__shutdown__` sentinel with `shutdown_tx: watch::Sender<bool>` in `Daemon` struct. `handle_shutdown` sends on the watch channel directly.

7. **Config file watching** — The daemon loads config at startup and reloads on `ConfigUpdate` socket request. Should pick up TOML changes automatically. Simplest path: check mtime on each sync cycle. Could also reuse the notify watcher with a poll fallback for network/FUSE filesystems.

8. **Stale repo/branch handling** — When repos disappear (unmounted drives, deleted checkouts) or branches are deleted, do NOT auto-delete from the DB. Mark entries as "not seen" with a timestamp, surface in `gitsitter status`. Only clean up on explicit `gitsitter forget <repo|branch>` command. Rationale: drives may be temporarily unmounted; auto-deletion loses state that's expensive to reconstruct.

9. **`gitsitter install` interactive TUI** — Currently requires subcommands (`shell`, `daemon`). Should offer an interactive installer when called with no arguments.

### Lower Priority

10. **Shallow clone handling** — No special handling currently. Pushing from a shallow clone may fail because the remote rejects incomplete history. Detect shallow clones and skip pushing (log a warning instead).

---

## Improvements / Tech Debt

### Code Quality

- **Daemon lock contention** — `daemon.db` is behind a `Mutex`, `daemon.repos` behind `RwLock`. The sync loop acquires and drops these locks repeatedly within a single repo sync (sometimes 3-4 times per branch). Consider: acquire locks once at the top of `sync_repo`, or batch DB writes.

- **Repetitive branch state updates** — The match arms in `sync_repo` for each `MergeAnalysis` variant each construct a `BranchState` and call `upsert_branch`. This creates ~200 lines of near-identical code. Extract a helper.

- ~~**Mode Display formatting**~~ — **Done.** Implemented `Display` for `RepoSyncMode`, replacing `format!("{:?}", mode).to_lowercase()` usage.

### Robustness

- **Daemon start race** — `handle_daemon_start` spawns a detached process and immediately returns. No verification that the daemon actually started successfully or bound the socket.

- **Socket cleanup on crash** — If the daemon crashes without cleanup, the stale socket file blocks the next start. The daemon removes it on startup, but could also check if a PID file exists and whether that PID is alive.

- **Fetch path** — Uses the first worktree path for fetch, which may not be the main worktree. Should use the main working directory (from `repo.workdir()`).

### Testing

- **Integration test coverage** — There's one end-to-end daemon integration test. Could use more: multi-branch sync, divergence detection, push flow, backoff behavior, config resolution with in-repo files.

- **No tests for config resolution** — `resolve_repo_mode` and `resolve_branch_mode` are complex but untested beyond the glob matching tests.

### UX

- **`gitsitter config --explain` readability** — The resolution chain output is functional but doesn't match the nicely formatted plan example with tree lines and "applied" markers.

- **Status output when not registered** — `gitsitter status` in an unregistered repo shows "repo not registered" but doesn't auto-register. The prompt hook does auto-register, so there's a gap if someone runs `gitsitter status` before the hook fires.

- **Error messages for untrusted hosts** — When a repo's remote is on an untrusted host, the status just shows `none` mode. Could be more helpful: "host X not trusted, run `gitsitter config --global` to add it".

---

## Dropped

These were considered and intentionally dropped:

- **Log streaming (`--follow`)** — Defer to system log infrastructure (journald, macOS unified logging).
- **`--since` filtering for logs** — Same; `journalctl --since` already handles this.
- **Log rotation** — System infra handles this. No need for tracing-appender rotation.
- **Repo disappearance notifications** — No good use case for nagging about repos that vanished (may just be unmounted drives).
- **Windows support** — Not a priority. Unix-only for now.
