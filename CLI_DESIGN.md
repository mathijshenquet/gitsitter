# CLI Design

Design document for gitsitter CLI output. All examples assume `emoji: true` and `colors: true` unless noted.

## Conventions

### Colors
- **Green**: success, active, synced
- **Yellow**: warnings, attention needed
- **Dim/gray**: inactive, not an error — just "off"
- **Subtle blue**: emphasis on paths, mode names, commands

### Mode indicator

Shows which operations are active for a repo. Active = green `Pull ✓`, inactive = dim `Pull ·`. The label name is colored too but in a more subtle shade.

| Mode        | Display                      |
|-------------|------------------------------|
| `none`      | `Fetch ·  Pull ·  Push ·`   |
| `fetch`     | `Fetch ✓  Pull ·  Push ·`   |
| `pull`      | `Fetch ✓  Pull ✓  Push ·`   |
| `push`      | `Fetch ✓  Pull ·  Push ✓`   |
| `push+pull` | `Fetch ✓  Pull ✓  Push ✓`   |

### Emoji

The `📦` repo icon and all other emoji are controlled by `emoji: true/false` in config. When `emoji: false`:
- `📦` is omitted
- `🎉` becomes `+`
- `⚠` becomes `!`
- `✅` `⬇️` `⚠️` branch status icons become text-only labels
- `✓` and `·` in the mode indicator become `[on]` and `[off]`
- `⏸` becomes `-`

### No-color fallback

When `colors: false`, no ANSI codes are emitted. The `✓` and `·` characters remain as-is since they're legible without color.

---

## Commands

### `gitsitter register`

Registers the current or given repo. Shows resolved config and daemon status.

```
🎉 Registered 📦 ~/gitsitter/.git

   Mode: pull   Fetch ✓  Pull ✓  Push ·

   Change with: gitsitter config --repo <mode>
```

Daemon not running:

```
🎉 Registered 📦 ~/gitsitter/.git
   ⚠ Daemon is not running — start with: gitsitter daemon start

   Mode: pull   Fetch ✓  Pull ✓  Push ·

   Change with: gitsitter config --repo <mode>
```

No-emoji:

```
+ Registered ~/gitsitter/.git

  Mode: pull   Fetch [on]  Pull [on]  Push [off]

  Change with: gitsitter config --repo <mode>
```

### `gitsitter status`

Single repo status. When the daemon is running:

```
📦 ~/gitsitter/.git  synced 30s ago

  main             ← origin/main        ✅ synced
  feature/foo      ← origin/feature/foo  ⬇️  remote ahead, pulled 2m ago
  hotfix           ← origin/hotfix       ⚠️  diverged, ff not possible

  Mode: pull   Fetch ✓  Pull ✓  Push ·

  Change with: gitsitter config --repo <mode>
```

When the daemon is not running:

```
⚠ Daemon is not running — start with: gitsitter daemon start

📦 ~/gitsitter/.git

   No sync data available, ensure the daemon is running

  Mode: pull   Fetch ✓  Pull ✓  Push ·

  Change with: gitsitter config --repo <mode>
```

### `gitsitter status --global`

Compact, two lines per repo with aligned columns:

```
📦 ~/gitsitter/.git       Mode: pull   Fetch ✓ Pull ✓ Push ·
   synced 30s ago, 3 branches, all synced

📦 ~/work/project/.git    Mode: push   Fetch ✓ Pull · Push ✓
   synced 2m ago, 5 branches, 1 diverged

📦 ~/oss/lib/.git         Mode: none   Fetch · Pull · Push ·
   never synced, disabled
```

### `gitsitter enable`

```
✓ Enabled 📦 ~/gitsitter/.git

  Mode: pull   Fetch ✓  Pull ✓  Push ·

  Change with: gitsitter config --repo <mode>
```

### `gitsitter disable`

```
⏸ Disabled 📦 ~/gitsitter/.git
```

### `gitsitter daemon start`

```
✓ Daemon started via systemd
```

Or:

```
✓ Daemon started, PID: 12345
```

Already running:

```
· Daemon is already running
```

### `gitsitter daemon stop`

```
✓ Daemon stopped
```

### `gitsitter daemon status`

```
✓ Daemon is running
  PID:           12345
  Uptime:        2h 30m
  Repos watched: 4
```

Not running:

```
· Daemon is not running
```

### `gitsitter sync`

```
✓ Synced 📦 ~/gitsitter/.git
```

### `gitsitter config`

No args, current repo:

```
Config for 📦 ~/gitsitter/.git

  Mode: pull   Fetch ✓  Pull ✓  Push ·

  Refresh interval: 1m
  Branches:
    main      pull
    feature/* fetch
```

### `gitsitter config --explain`

Unchanged from current implementation — it shows the resolution chain which is already detailed and useful.

### `gitsitter install`

Unchanged — informational output with next-step hints.

### `gitsitter log`

Unchanged — pass-through of daemon log.

---

## Implementation

### Dependencies

No new crates needed. We can use ANSI escape codes directly — the codebase already has `crossterm` which provides `style::Stylize` for colored output, but for simple `println!`-based CLI output, inline ANSI codes or a small helper are lighter weight. Since `crossterm` is already a dependency, we'll use `crossterm::style` for color.

Column alignment for `--global` can be done with `format!` width specifiers — no need for a table crate.

### Shared components

Extract at least these helpers:

```
repo_header(path, emoji)        → "📦 ~/gitsitter/.git" or "~/gitsitter/.git"
mode_line(mode, emoji, colors)  → "Mode: pull   Fetch ✓  Pull ✓  Push ·"
daemon_warning(emoji)           → "⚠ Daemon is not running — ..."
change_hint()                   → "Change with: gitsitter config --repo <mode>"
repo_info_block(path, mode, daemon_running, emoji, colors)
                                        → the full mode + daemon warning + hint block,
                                          reused by register, enable, status
```

These read `emoji` and `colors` from config to decide formatting. The config is loaded once at the call site and passed in or accessed via a shared context.

### Changes by file

- **`src/config.rs`**: remove the `eprintln!("initialized config at ...")` from `UserConfig::load`
- **`src/cli.rs`**: refactor `handle_register`, `handle_enable`, `handle_disable`, `handle_status`, `handle_sync`, `handle_daemon_*` to use helpers

### Config access

The handlers already load `UserConfig` or can do so cheaply. The `emoji` and `colors` booleans live on `UserConfig.global`. Pass these to `cli_ui` functions — no global state needed.

### Daemon check

`transport::is_daemon_running(&paths.socket_path)` is already available and non-blocking. Use it in `handle_register`, `handle_enable`, and `handle_status` to conditionally show the daemon warning.
