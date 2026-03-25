# Remove primary_remote_url abstraction

`resolve_repo_mode` takes a single `remote_url` and uses it for `defaults.remotes` glob matching. With multi-remote repos this is wrong — `primary_remote_url()` picks origin (or first available), so a repo with remotes on different hosts resolves mode based on whichever remote happens to be "primary".

This matters because:
- A defaults glob like `*github.com*` = push would match origin but not an upstream on gitlab
- The displayed mode reflects the primary remote, not the remote actually being synced
- Per-remote trust is checked at sync time, but mode resolution still uses a single URL

Fix: `resolve_repo_mode` should either take the full remote map or mode resolution should happen per-branch (since each branch tracks one remote). Per-branch is probably the right granularity — we already resolve branch sync mode, so folding remote-level mode resolution into that path would remove the need for a single "repo mode" entirely.
