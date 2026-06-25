# gitsitter — workflow reference

> **Auto-generated** by `cargo test --test workflows -- --ignored generate_workflow_docs`.
> All outputs reflect actual behavior: each scenario constructs real git repositories and runs gitsitter's sync pipeline against them.
> Version **0.1.0**, commit `336f4b9`.
> **Do not edit** — re-run the test to regenerate.

Each scenario below shows a simulated multi-user git workflow, then runs `gitsitter sync` and shows the resulting status. Shell prompts indicate which simulated host is acting (`alice`, `bob`, etc.).


## Scenario: Remote ahead — fast-forward

Someone else pushes to a branch you have checked out. Your local copy is behind. gitsitter fast-forwards your local branch to match the remote.


### Setup

```sh
alice $ git clone origin ~/project && cd ~/project
alice $ echo '# project' > README.md
alice $ git add . && git commit -m 'initial commit'   # => 9347046
alice $ git push -u origin main
```

Meanwhile, another developer pushes:

```sh
bob   $ echo 'new feature' > feature.txt
bob   $ git add . && git commit -m 'add feature'
bob   $ git push origin main
```


### gitsitter sync

```sh
$ gitsitter sync
fetched origin
main: FastForward → FastForwardMerge → fast-forwarded [synced]
```

gitsitter fetches, sees the remote is ahead, and fast-forwards the local branch.


### Result

- **Status**: `synced`
- **Outcome**: `local branch updated to match remote`


## Scenario: Local ahead — auto-push

You commit locally and the remote hasn't changed. gitsitter pushes your commits automatically.


### Setup

```sh
alice $ git clone origin ~/project && cd ~/project
alice $ echo '# project' > README.md
alice $ git add . && git commit -m 'initial commit'
alice $ git push -u origin main
```

You make a new commit locally:

```sh
alice $ echo 'buy milk' > todo.txt
alice $ git add . && git commit -m 'add todo list'   # => 4f9d42b
```


### gitsitter sync

```sh
$ gitsitter sync
fetched origin
main: LocalAhead → Push → pushed [synced]
```

gitsitter pushes your commit to the remote.


### Result

- **Status**: `synced`
- **Outcome**: `local commits pushed to remote`


## Scenario: Normal divergence — held for manual resolution

Your branch diverges from the remote (e.g. a CI merge landed on remote while you committed locally). gitsitter detects this as ordinary divergence (not a history rewrite). It only ever fast-forwards and pushes, so it does not rebase or rewrite history — it flags the branch and leaves it for you to resolve with git (merge or rebase).


### Setup

```sh
alice $ git clone origin ~/project && cd ~/project
alice $ echo '# project' > README.md
alice $ git add . && git commit -m 'initial commit'
alice $ git push -u origin main
```

A CI merge lands on the remote, then Alice commits locally:

```sh
ci    $ # merge commit lands on origin/main   # => c587afe
```

```sh
alice $ echo 'from alice' > local.txt
alice $ git add . && git commit -m 'alice: add file'   # => 95669a6
```

Now local (`95669a6`) and remote (`c587afe`) have diverged.


### gitsitter sync

```sh
$ gitsitter sync
fetched origin
main: Diverged → Diverged (rewrite: None) → diverged — holding for manual resolution [diverged_yours]
```

gitsitter checks the reflog, confirms this is normal divergence (not a history rewrite), and holds — leaving Alice to merge or rebase with git.


### Result

- **Status**: `diverged_yours`
- **Outcome**: `diverged — held for manual resolution`


## Scenario: Interactive rebase — rewrite detected, remote unchanged

You squash or reorder commits with `git rebase -i`. The remote still has the old history. gitsitter detects the rewrite via the reflog and holds — it does **not** rebase on top of the remote (which would duplicate commits).


### Setup

```sh
alice $ git clone origin ~/project && cd ~/project
alice $ echo aaa > a.txt && git add . && git commit -m 'commit A'
alice $ git push -u origin main
alice $ echo bbb > b.txt && git add . && git commit -m 'commit B'   # => 7e710f7
alice $ git push
```

Remote and local are both at `7e710f7`. Two commits published: A and B.


### Rewrite

Alice squashes commits A and B into one (simulating `git rebase -i HEAD~2`):

```sh
alice $ git rebase -i HEAD~2   # squash A+B
alice $ # branch is now at df2e9d3  (was 7e710f7)
```

Local is at `df2e9d3`, remote is still at `7e710f7`. The branches have diverged, but this is an intentional rewrite — not concurrent work.


### gitsitter sync

```sh
$ gitsitter sync
fetched origin
main: Diverged → Diverged (rewrite: RemoteUnchanged) → history rewrite detected, remote unchanged — holding [history_rewritten_remote_unchanged]
```

gitsitter walks the reflog, finds that a prior branch tip (`commit B`) is reachable from the remote but not from the current local tip — evidence of intentional history editing. It holds instead of rebasing.

This is critical: without this check, gitsitter would run `git rebase origin/main`, replaying the old un-squashed commits and creating duplicates.


### Result

- **Status**: `history_rewritten_remote_unchanged`
- **Outcome**: `hold — waiting for user to force-push`


## Scenario: Commit amend — rewrite detected

You amend the most recent commit after pushing. gitsitter detects the rewrite and holds, same as with interactive rebase.


### Setup

```sh
alice $ git clone origin ~/project && cd ~/project
alice $ echo '# v1' > README.md && git add . && git commit -m 'initial'
alice $ git push -u origin main
alice $ echo wip > feature.txt && git add . && git commit -m 'add feature (wip)'   # => c3dfcf9
alice $ git push
```


### Amend

```sh
alice $ echo done > feature.txt && git add .
alice $ git commit --amend -m 'add feature (done)'   # c3dfcf9 => 1b3f70f
```


### gitsitter sync

```sh
$ gitsitter sync
fetched origin
main: Diverged → Diverged (rewrite: RemoteUnchanged) → history rewrite detected, remote unchanged — holding [history_rewritten_remote_unchanged]
```

Same as interactive rebase: gitsitter detects the rewrite and holds.


### Result

- **Status**: `history_rewritten_remote_unchanged`
- **Outcome**: `hold — waiting for user to force-push`


## Scenario: Rewrite + remote advanced — warning

You rewrite local history, but in the meantime someone else pushes to the remote. A force-push would now discard their commits. gitsitter detects this and warns instead of rebasing.


### Setup

```sh
alice $ git clone origin ~/project && cd ~/project
alice $ echo aaa > a.txt && git add . && git commit -m 'commit A'
alice $ git push -u origin main
alice $ echo bbb > b.txt && git add . && git commit -m 'commit B'   # => 7e710f7
alice $ git push
```


### Alice rewrites

```sh
alice $ git rebase -i HEAD~1   # rewrite B into C
alice $ # local is now at 61e07ec
```


### Remote advances

```sh
ci    $ # merge commit lands on origin/main   # => 576a967
```

Remote is now at `576a967` (past the old published tip `7e710f7`), while local was rewritten to `61e07ec`.


### gitsitter sync

```sh
$ gitsitter sync
fetched origin
main: Diverged → Diverged (rewrite: RemoteAdvanced) → history rewrite detected, remote advanced — holding [history_rewritten_remote_advanced]
```

gitsitter detects both the rewrite *and* that the remote advanced past the old published tip. A force-push would discard Bob's commit D. gitsitter warns and holds.


### Result

- **Status**: `history_rewritten_remote_advanced`
- **Outcome**: `hold — force-push would discard remote commits`


## Scenario: Diverged but not your branch — flag only

The branch has diverged, but the last remote commit was by someone else. gitsitter doesn't rebase — it flags the branch and lets you decide.


### Setup

```sh
alice $ git clone origin ~/project && cd ~/project
alice $ echo '# project' > README.md && git add . && git commit -m 'initial'
alice $ git push -u origin main
```

Bob pushes, then Alice commits locally:

```sh
bob   $ echo theirs > other.txt && git add . && git commit -m 'bob: other work' && git push
```

```sh
alice $ echo mine > mine.txt && git add . && git commit -m 'alice: my work'
```


### gitsitter sync

```sh
$ gitsitter sync
fetched origin
main: Diverged → DivergedNotOwned → diverged, not owned — flagged [diverged]
```

The last commit on the remote is by Bob, not Alice. gitsitter won't auto-rebase someone else's branch — it flags the divergence and waits for the user to resolve it.


### Result

- **Status**: `diverged`
- **Outcome**: `flagged — user must resolve manually`


## Scenario: Dirty worktree — skip

The remote is ahead, but you have uncommitted changes. gitsitter skips the fast-forward to avoid clobbering your work.


### Setup

```sh
alice $ git clone origin ~/project && cd ~/project
alice $ echo '# project' > README.md && git add . && git commit -m 'initial commit'
alice $ git push -u origin main
```

Bob pushes, then Alice starts editing a tracked file (without committing):

```sh
bob   $ echo 'new stuff' > update.txt && git add . && git commit -m 'update' && git push
```

```sh
alice $ echo 'work in progress' >> README.md   # modified but not committed
```


### gitsitter sync

```sh
$ gitsitter sync
fetched origin
main: FastForward → SkipDirty → skipped — worktree dirty [pending_dirty]
```

gitsitter sees the remote is ahead (fast-forward possible), but the worktree has uncommitted changes. It skips the update to avoid data loss and will retry when the worktree is clean.


### Result

- **Status**: `pending_dirty`
- **Outcome**: `skipped — will retry when worktree is clean`

