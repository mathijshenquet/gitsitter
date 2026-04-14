//! Workflow tests that double as generated documentation.
//!
//! Each scenario sets up real git repositories, constructs a minimal in-process
//! daemon (no background tasks, no socket — just the shared state), and calls
//! `sync_repo` — the same function the daemon loop calls. Assertions verify the
//! resulting branch status. The markdown output documents the observable behavior.
//!
//! Run with:
//!     cargo test --test workflows -- --ignored generate_workflow_docs
//!
//! Output: docs/workflows.md

use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Notify, RwLock, watch};

use gitsitter::config::UserConfig;
use gitsitter::daemon::{self, Daemon, TrackedRepo};
use gitsitter::forge::ForgeCache;
use gitsitter::git_ops;
use gitsitter::paths::Paths;
use gitsitter::transport::SyncEvent;

// ===========================================================================
// Temp dir / paths helpers
// ===========================================================================

fn temp_dir() -> tempfile::TempDir {
    let base = std::env::current_dir()
        .unwrap()
        .join("target")
        .join("test-tmp");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("gitsitter-wf-")
        .tempdir_in(base)
        .unwrap()
}

fn test_paths(base: &Path) -> Paths {
    let config_dir = base.join("config");
    let state_dir = base.join("state");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    Paths {
        config_file: config_dir.join("config.toml"),
        repos_file: config_dir.join("repos.toml"),
        trusted_hosts_file: config_dir.join("trusted_hosts"),
        daemon_log: state_dir.join("daemon.log"),
        daemon_pid: state_dir.join("daemon.pid"),
        socket_path: base.join("gitsitter-test.sock"),
    }
}

// ===========================================================================
// Git helpers
// ===========================================================================

fn create_bare_repo(dir: &Path) -> git2::Repository {
    git2::Repository::init_bare(dir).unwrap()
}

fn clone_repo(bare_path: &Path, working_path: &Path) -> git2::Repository {
    let url = format!("file://{}", bare_path.display());
    let repo = git2::build::RepoBuilder::new()
        .clone(&url, working_path)
        .unwrap();
    // Set user identity so ownership checks work.
    repo.config()
        .unwrap()
        .set_str("user.name", "Test User")
        .unwrap();
    repo.config()
        .unwrap()
        .set_str("user.email", "test@example.com")
        .unwrap();
    repo
}

fn make_commit(repo: &git2::Repository, filename: &str, content: &str, message: &str) {
    let workdir = repo.workdir().expect("not a bare repo");
    std::fs::write(workdir.join(filename), content).unwrap();

    let mut index = repo.index().unwrap();
    index.add_path(Path::new(filename)).unwrap();
    index.write().unwrap();

    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = git2::Signature::now("Test User", "test@example.com").unwrap();

    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<&git2::Commit> = parent.iter().collect();

    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
        .unwrap();
}

fn make_commit_as(
    repo: &git2::Repository,
    filename: &str,
    content: &str,
    message: &str,
    name: &str,
    email: &str,
) {
    let workdir = repo.workdir().expect("not a bare repo");
    std::fs::write(workdir.join(filename), content).unwrap();

    let mut index = repo.index().unwrap();
    index.add_path(Path::new(filename)).unwrap();
    index.write().unwrap();

    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = git2::Signature::now(name, email).unwrap();

    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<&git2::Commit> = parent.iter().collect();

    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
        .unwrap();
}

fn push(repo: &git2::Repository, branch: &str) {
    let mut remote = repo.find_remote("origin").unwrap();
    remote
        .push(&[&format!("refs/heads/{}", branch)], None)
        .unwrap();
}

/// Push a commit directly into the bare repo (simulates someone else pushing).
/// Uses "Other Dev" identity so ownership checks correctly identify it as not yours.
fn push_to_bare(bare_path: &Path, branch: &str, filename: &str, content: &str, message: &str) {
    push_to_bare_as(
        bare_path,
        branch,
        filename,
        content,
        message,
        "Other Dev",
        "other@example.com",
    );
}

/// Push a commit into the bare repo with a specific identity.
fn push_to_bare_as(
    bare_path: &Path,
    branch: &str,
    filename: &str,
    content: &str,
    message: &str,
    name: &str,
    email: &str,
) {
    let bare = git2::Repository::open(bare_path).unwrap();
    let head_oid = bare
        .find_reference(&format!("refs/heads/{}", branch))
        .unwrap()
        .target()
        .unwrap();
    let parent = bare.find_commit(head_oid).unwrap();
    let tree = {
        let blob_oid = bare.blob(content.as_bytes()).unwrap();
        let mut tb = bare.treebuilder(Some(&parent.tree().unwrap())).unwrap();
        tb.insert(filename, blob_oid, 0o100644).unwrap();
        let oid = tb.write().unwrap();
        bare.find_tree(oid).unwrap()
    };
    let sig = git2::Signature::now(name, email).unwrap();
    bare.commit(
        Some(&format!("refs/heads/{}", branch)),
        &sig,
        &sig,
        message,
        &tree,
        &[&parent],
    )
    .unwrap();
}

fn short_oid(repo: &git2::Repository) -> String {
    repo.head().unwrap().target().unwrap().to_string()[..7].to_string()
}

fn remote_short_oid(repo: &git2::Repository, branch: &str) -> String {
    let refname = format!("refs/remotes/origin/{}", branch);
    repo.find_reference(&refname)
        .unwrap()
        .target()
        .unwrap()
        .to_string()[..7]
        .to_string()
}

// ===========================================================================
// Daemon test harness
// ===========================================================================

/// Create a minimal in-process daemon with one registered repo.
/// No background tasks, no socket — just the shared state needed for sync_repo.
fn test_daemon(tmp: &Path, repo_id: &str, work_dir: &Path) -> Arc<Daemon> {
    let paths = test_paths(tmp);
    let remote_urls = git_ops::get_all_remote_urls(Path::new(repo_id)).unwrap_or_default();
    let display_path = work_dir.display().to_string();

    let mut repos = HashMap::new();
    repos.insert(
        repo_id.to_string(),
        TrackedRepo::new(repo_id.to_string(), display_path, remote_urls),
    );

    let (shutdown_tx, _shutdown_rx) = watch::channel(false);

    Arc::new(Daemon {
        paths,
        config: RwLock::new(UserConfig::default()),
        start_time: Instant::now(),
        repos: RwLock::new(repos),
        sync_notify: Notify::new(),
        shutdown_tx,
        forge_cache: ForgeCache::new(),
        latest_version: RwLock::new(None),
    })
}

struct SyncResult {
    status: String,
    events: Vec<SyncEvent>,
}

/// Run sync and return the events + sync_status for the given branch.
async fn sync_and_get_result(daemon: &Arc<Daemon>, repo_id: &str, branch: &str) -> SyncResult {
    let events = daemon::sync_repo(daemon, repo_id).await.unwrap();
    let repos = daemon.repos.read().await;
    let tr = repos.get(repo_id).unwrap();
    let status = tr
        .branches
        .get(branch)
        .map(|b| b.sync_status.clone())
        .unwrap_or_else(|| "no_status".to_string());
    SyncResult { status, events }
}

// ===========================================================================
// Workflow recorder
// ===========================================================================

struct Recorder {
    buf: String,
}

impl Recorder {
    fn new() -> Self {
        Self { buf: String::new() }
    }

    fn heading(&mut self, level: usize, text: &str) {
        let prefix: String = "#".repeat(level);
        writeln!(self.buf, "\n{} {}\n", prefix, text).unwrap();
    }

    fn paragraph(&mut self, text: &str) {
        writeln!(self.buf, "{}\n", text).unwrap();
    }

    fn shell(&mut self, host: &str, lines: &[&str]) {
        writeln!(self.buf, "```sh").unwrap();
        for line in lines {
            writeln!(self.buf, "{}$ {}", host, line).unwrap();
        }
        writeln!(self.buf, "```\n").unwrap();
    }

    fn sync_result(&mut self, result: &SyncResult) {
        writeln!(self.buf, "```sh").unwrap();
        writeln!(self.buf, "$ gitsitter sync").unwrap();
        for event in &result.events {
            match event {
                SyncEvent::Fetch { remotes } => {
                    writeln!(self.buf, "fetched {}", remotes.join(", ")).unwrap();
                }
                SyncEvent::Branch {
                    branch,
                    analysis,
                    sync_action,
                    rewrite,
                    status,
                    detail,
                } => {
                    write!(self.buf, "{}: {} → {}", branch, analysis, sync_action).unwrap();
                    if let Some(rw) = rewrite {
                        write!(self.buf, " (rewrite: {})", rw).unwrap();
                    }
                    writeln!(self.buf, " → {} [{}]", detail, status).unwrap();
                }
            }
        }
        writeln!(self.buf, "```\n").unwrap();
    }

    fn status_line(&mut self, label: &str, value: &str) {
        writeln!(self.buf, "- **{}**: `{}`", label, value).unwrap();
    }

    fn finish_list(&mut self) {
        writeln!(self.buf).unwrap();
    }

    fn into_string(self) -> String {
        self.buf
    }
}

// ===========================================================================
// Scenarios
// ===========================================================================

async fn scenario_fast_forward() -> String {
    let mut r = Recorder::new();
    r.heading(2, "Scenario: Remote ahead — fast-forward");
    r.paragraph(
        "Someone else pushes to a branch you have checked out. Your local copy \
         is behind. gitsitter fast-forwards your local branch to match the remote.",
    );

    let tmp = temp_dir();
    let bare_dir = tmp.path().join("bare");
    let work_dir = tmp.path().join("work");
    let _bare = create_bare_repo(&bare_dir);
    let work = clone_repo(&bare_dir, &work_dir);

    make_commit(&work, "README.md", "# project\n", "initial commit");
    let branch = work.head().unwrap().shorthand().unwrap().to_string();
    push(&work, &branch);

    r.heading(3, "Setup");
    r.shell(
        "alice ",
        &[
            "git clone origin ~/project && cd ~/project",
            "echo '# project' > README.md",
            &format!(
                "git add . && git commit -m 'initial commit'   # => {}",
                short_oid(&work)
            ),
            "git push -u origin main",
        ],
    );

    // Someone else pushes
    push_to_bare(
        &bare_dir,
        &branch,
        "feature.txt",
        "new feature\n",
        "add feature",
    );

    r.paragraph("Meanwhile, another developer pushes:");
    r.shell(
        "bob   ",
        &[
            "echo 'new feature' > feature.txt",
            "git add . && git commit -m 'add feature'",
            "git push origin main",
        ],
    );

    let id = git_ops::discover_repo_id(work_dir.as_path()).unwrap();
    let id_str = id.to_string_lossy().to_string();
    let daemon = test_daemon(tmp.path(), &id_str, &work_dir);

    r.heading(3, "gitsitter sync");
    let result = sync_and_get_result(&daemon, &id_str, &branch).await;
    r.sync_result(&result);

    assert_eq!(result.status, "synced");

    r.paragraph("gitsitter fetches, sees the remote is ahead, and fast-forwards the local branch.");
    r.heading(3, "Result");
    r.status_line("Status", &result.status);
    r.status_line("Outcome", "local branch updated to match remote");
    r.finish_list();

    r.into_string()
}

async fn scenario_local_ahead_push() -> String {
    let mut r = Recorder::new();
    r.heading(2, "Scenario: Local ahead — auto-push");
    r.paragraph(
        "You commit locally and the remote hasn't changed. gitsitter pushes \
         your commits automatically.",
    );

    let tmp = temp_dir();
    let bare_dir = tmp.path().join("bare");
    let work_dir = tmp.path().join("work");
    let _bare = create_bare_repo(&bare_dir);
    let work = clone_repo(&bare_dir, &work_dir);

    make_commit(&work, "README.md", "# project\n", "initial commit");
    let branch = work.head().unwrap().shorthand().unwrap().to_string();
    push(&work, &branch);

    r.heading(3, "Setup");
    r.shell(
        "alice ",
        &[
            "git clone origin ~/project && cd ~/project",
            "echo '# project' > README.md",
            "git add . && git commit -m 'initial commit'",
            "git push -u origin main",
        ],
    );

    // Local commit (not pushed)
    make_commit(&work, "todo.txt", "buy milk\n", "add todo list");

    r.paragraph("You make a new commit locally:");
    r.shell(
        "alice ",
        &[
            "echo 'buy milk' > todo.txt",
            &format!(
                "git add . && git commit -m 'add todo list'   # => {}",
                short_oid(&work)
            ),
        ],
    );

    let id = git_ops::discover_repo_id(work_dir.as_path()).unwrap();
    let id_str = id.to_string_lossy().to_string();
    let daemon = test_daemon(tmp.path(), &id_str, &work_dir);

    r.heading(3, "gitsitter sync");
    let result = sync_and_get_result(&daemon, &id_str, &branch).await;
    r.sync_result(&result);

    assert_eq!(result.status, "synced");

    r.paragraph("gitsitter pushes your commit to the remote.");
    r.heading(3, "Result");
    r.status_line("Status", &result.status);
    r.status_line("Outcome", "local commits pushed to remote");
    r.finish_list();

    r.into_string()
}

async fn scenario_normal_divergence_rebase() -> String {
    let mut r = Recorder::new();
    r.heading(2, "Scenario: Normal divergence — rebase then push");
    r.paragraph(
        "Your branch diverges from the remote (e.g. a CI merge landed on remote \
         while you committed locally). gitsitter detects this as ordinary divergence \
         (not a history rewrite), rebases your work on top of the remote, and pushes.",
    );

    let tmp = temp_dir();
    let bare_dir = tmp.path().join("bare");
    let work_dir = tmp.path().join("work");
    let _bare = create_bare_repo(&bare_dir);
    let work = clone_repo(&bare_dir, &work_dir);

    make_commit(&work, "README.md", "# project\n", "initial commit");
    let branch = work.head().unwrap().shorthand().unwrap().to_string();
    push(&work, &branch);

    r.heading(3, "Setup");
    r.shell(
        "alice ",
        &[
            "git clone origin ~/project && cd ~/project",
            "echo '# project' > README.md",
            "git add . && git commit -m 'initial commit'",
            "git push -u origin main",
        ],
    );

    // Remote advances (e.g. a CI merge with your identity — you still own the branch)
    push_to_bare_as(
        &bare_dir,
        &branch,
        "ci.txt",
        "merged by ci\n",
        "ci: merge main",
        "Test User",
        "test@example.com",
    );

    // Local commit
    make_commit(&work, "local.txt", "from alice\n", "alice: add file");
    let local_oid = short_oid(&work);

    // Fetch just to show the remote oid in docs
    {
        let mut remote = work.find_remote("origin").unwrap();
        remote.fetch::<&str>(&[], None, None).unwrap();
    }
    let remote_oid = remote_short_oid(&work, &branch);

    r.paragraph("A CI merge lands on the remote, then Alice commits locally:");
    r.shell(
        "ci    ",
        &[&format!(
            "# merge commit lands on origin/main   # => {}",
            remote_oid
        )],
    );
    r.shell(
        "alice ",
        &[
            "echo 'from alice' > local.txt",
            &format!(
                "git add . && git commit -m 'alice: add file'   # => {}",
                local_oid
            ),
        ],
    );
    r.paragraph(&format!(
        "Now local (`{}`) and remote (`{}`) have diverged.",
        local_oid, remote_oid,
    ));

    let id = git_ops::discover_repo_id(work_dir.as_path()).unwrap();
    let id_str = id.to_string_lossy().to_string();
    let daemon = test_daemon(tmp.path(), &id_str, &work_dir);

    r.heading(3, "gitsitter sync");
    let result = sync_and_get_result(&daemon, &id_str, &branch).await;
    r.sync_result(&result);

    assert_eq!(result.status, "synced");

    r.paragraph(
        "gitsitter checks the reflog, confirms this is normal divergence (not a \
         history rewrite), rebases Alice's commit on top of the remote, and pushes.",
    );
    r.heading(3, "Result");
    r.status_line("Status", &result.status);
    r.status_line("Outcome", "rebase onto remote, then push");
    r.finish_list();

    r.into_string()
}

async fn scenario_interactive_rebase_detected() -> String {
    let mut r = Recorder::new();
    r.heading(
        2,
        "Scenario: Interactive rebase — rewrite detected, remote unchanged",
    );
    r.paragraph(
        "You squash or reorder commits with `git rebase -i`. The remote still has \
         the old history. gitsitter detects the rewrite via the reflog and holds — \
         it does **not** rebase on top of the remote (which would duplicate commits).",
    );

    let tmp = temp_dir();
    let bare_dir = tmp.path().join("bare");
    let work_dir = tmp.path().join("work");
    let _bare = create_bare_repo(&bare_dir);
    let work = clone_repo(&bare_dir, &work_dir);

    // Commit A + push
    make_commit(&work, "a.txt", "aaa\n", "commit A");
    let branch = work.head().unwrap().shorthand().unwrap().to_string();
    push(&work, &branch);

    // Commit B + push (this becomes the published tip)
    make_commit(&work, "b.txt", "bbb\n", "commit B");
    push(&work, &branch);
    let published_oid = short_oid(&work);

    r.heading(3, "Setup");
    r.shell(
        "alice ",
        &[
            "git clone origin ~/project && cd ~/project",
            "echo aaa > a.txt && git add . && git commit -m 'commit A'",
            "git push -u origin main",
            &format!(
                "echo bbb > b.txt && git add . && git commit -m 'commit B'   # => {}",
                published_oid
            ),
            "git push",
        ],
    );
    r.paragraph(&format!(
        "Remote and local are both at `{}`. Two commits published: A and B.",
        published_oid,
    ));

    // Simulate interactive rebase: reset to parent of B, make new commit
    let head = work.head().unwrap().target().unwrap();
    let commit_b = work.find_commit(head).unwrap();
    let commit_a = commit_b.parent(0).unwrap();
    work.reset(commit_a.as_object(), git2::ResetType::Hard, None)
        .unwrap();
    make_commit(&work, "ab.txt", "aaa\nbbb\n", "squash A+B into one commit");
    let rewritten_oid = short_oid(&work);

    r.heading(3, "Rewrite");
    r.paragraph("Alice squashes commits A and B into one (simulating `git rebase -i HEAD~2`):");
    r.shell(
        "alice ",
        &[
            "git rebase -i HEAD~2   # squash A+B",
            &format!(
                "# branch is now at {}  (was {})",
                rewritten_oid, published_oid
            ),
        ],
    );
    r.paragraph(&format!(
        "Local is at `{}`, remote is still at `{}`. The branches have diverged, \
         but this is an intentional rewrite — not concurrent work.",
        rewritten_oid, published_oid,
    ));

    let id = git_ops::discover_repo_id(work_dir.as_path()).unwrap();
    let id_str = id.to_string_lossy().to_string();
    let daemon = test_daemon(tmp.path(), &id_str, &work_dir);

    r.heading(3, "gitsitter sync");
    let result = sync_and_get_result(&daemon, &id_str, &branch).await;
    r.sync_result(&result);

    assert_eq!(result.status, "history_rewritten_remote_unchanged");

    r.paragraph(
        "gitsitter walks the reflog, finds that a prior branch tip (`commit B`) is \
         reachable from the remote but not from the current local tip — evidence of \
         intentional history editing. It holds instead of rebasing.",
    );
    r.paragraph(
        "This is critical: without this check, gitsitter would run \
         `git rebase origin/main`, replaying the old un-squashed commits and \
         creating duplicates.",
    );
    r.heading(3, "Result");
    r.status_line("Status", &result.status);
    r.status_line("Outcome", "hold — waiting for user to force-push");
    r.finish_list();

    r.into_string()
}

async fn scenario_amend_detected() -> String {
    let mut r = Recorder::new();
    r.heading(2, "Scenario: Commit amend — rewrite detected");
    r.paragraph(
        "You amend the most recent commit after pushing. gitsitter detects the \
         rewrite and holds, same as with interactive rebase.",
    );

    let tmp = temp_dir();
    let bare_dir = tmp.path().join("bare");
    let work_dir = tmp.path().join("work");
    let _bare = create_bare_repo(&bare_dir);
    let work = clone_repo(&bare_dir, &work_dir);

    make_commit(&work, "README.md", "# v1\n", "initial");
    let branch = work.head().unwrap().shorthand().unwrap().to_string();
    push(&work, &branch);

    make_commit(&work, "feature.txt", "wip\n", "add feature (wip)");
    push(&work, &branch);
    let before_amend = short_oid(&work);

    r.heading(3, "Setup");
    r.shell("alice ", &[
        "git clone origin ~/project && cd ~/project",
        "echo '# v1' > README.md && git add . && git commit -m 'initial'",
        "git push -u origin main",
        &format!(
            "echo wip > feature.txt && git add . && git commit -m 'add feature (wip)'   # => {}",
            before_amend
        ),
        "git push",
    ]);

    // Simulate --amend: reset to same parent, make different commit
    let head = work.head().unwrap().target().unwrap();
    let commit = work.find_commit(head).unwrap();
    let parent = commit.parent(0).unwrap();
    work.reset(parent.as_object(), git2::ResetType::Hard, None)
        .unwrap();
    make_commit(&work, "feature.txt", "done\n", "add feature (done)");
    let after_amend = short_oid(&work);

    r.heading(3, "Amend");
    r.shell(
        "alice ",
        &[
            "echo done > feature.txt && git add .",
            &format!(
                "git commit --amend -m 'add feature (done)'   # {} => {}",
                before_amend, after_amend
            ),
        ],
    );

    let id = git_ops::discover_repo_id(work_dir.as_path()).unwrap();
    let id_str = id.to_string_lossy().to_string();
    let daemon = test_daemon(tmp.path(), &id_str, &work_dir);

    r.heading(3, "gitsitter sync");
    let result = sync_and_get_result(&daemon, &id_str, &branch).await;
    r.sync_result(&result);

    assert_eq!(result.status, "history_rewritten_remote_unchanged");

    r.paragraph("Same as interactive rebase: gitsitter detects the rewrite and holds.");
    r.heading(3, "Result");
    r.status_line("Status", &result.status);
    r.status_line("Outcome", "hold — waiting for user to force-push");
    r.finish_list();

    r.into_string()
}

async fn scenario_rewrite_remote_advanced() -> String {
    let mut r = Recorder::new();
    r.heading(2, "Scenario: Rewrite + remote advanced — warning");
    r.paragraph(
        "You rewrite local history, but in the meantime someone else pushes to the \
         remote. A force-push would now discard their commits. gitsitter detects this \
         and warns instead of rebasing.",
    );

    let tmp = temp_dir();
    let bare_dir = tmp.path().join("bare");
    let work_dir = tmp.path().join("work");
    let _bare = create_bare_repo(&bare_dir);
    let work = clone_repo(&bare_dir, &work_dir);

    make_commit(&work, "a.txt", "aaa\n", "commit A");
    let branch = work.head().unwrap().shorthand().unwrap().to_string();
    push(&work, &branch);
    let base_oid = work.head().unwrap().target().unwrap();

    make_commit(&work, "b.txt", "bbb\n", "commit B");
    push(&work, &branch);
    let published_oid = short_oid(&work);

    r.heading(3, "Setup");
    r.shell(
        "alice ",
        &[
            "git clone origin ~/project && cd ~/project",
            "echo aaa > a.txt && git add . && git commit -m 'commit A'",
            "git push -u origin main",
            &format!(
                "echo bbb > b.txt && git add . && git commit -m 'commit B'   # => {}",
                published_oid
            ),
            "git push",
        ],
    );

    // Rewrite local: reset to A, make new commit
    let commit_a = work.find_commit(base_oid).unwrap();
    work.reset(commit_a.as_object(), git2::ResetType::Hard, None)
        .unwrap();
    make_commit(&work, "c.txt", "ccc\n", "rewritten commit C");
    let rewritten_oid = short_oid(&work);

    r.heading(3, "Alice rewrites");
    r.shell(
        "alice ",
        &[
            "git rebase -i HEAD~1   # rewrite B into C",
            &format!("# local is now at {}", rewritten_oid),
        ],
    );

    // Remote advances past published tip (e.g. a CI merge still under your identity)
    push_to_bare_as(
        &bare_dir,
        &branch,
        "d.txt",
        "ddd\n",
        "ci: merge commit D",
        "Test User",
        "test@example.com",
    );

    // Fetch just to get the oid for docs
    {
        let mut remote = work.find_remote("origin").unwrap();
        remote.fetch::<&str>(&[], None, None).unwrap();
    }
    let remote_oid = remote_short_oid(&work, &branch);

    r.heading(3, "Remote advances");
    r.shell(
        "ci    ",
        &[&format!(
            "# merge commit lands on origin/main   # => {}",
            remote_oid
        )],
    );
    r.paragraph(&format!(
        "Remote is now at `{}` (past the old published tip `{}`), \
         while local was rewritten to `{}`.",
        remote_oid, published_oid, rewritten_oid,
    ));

    let id = git_ops::discover_repo_id(work_dir.as_path()).unwrap();
    let id_str = id.to_string_lossy().to_string();
    let daemon = test_daemon(tmp.path(), &id_str, &work_dir);

    r.heading(3, "gitsitter sync");
    let result = sync_and_get_result(&daemon, &id_str, &branch).await;
    r.sync_result(&result);

    assert_eq!(result.status, "history_rewritten_remote_advanced");

    r.paragraph(
        "gitsitter detects both the rewrite *and* that the remote advanced past the \
         old published tip. A force-push would discard Bob's commit D. gitsitter \
         warns and holds.",
    );
    r.heading(3, "Result");
    r.status_line("Status", &result.status);
    r.status_line("Outcome", "hold — force-push would discard remote commits");
    r.finish_list();

    r.into_string()
}

async fn scenario_diverged_not_owned() -> String {
    let mut r = Recorder::new();
    r.heading(2, "Scenario: Diverged but not your branch — flag only");
    r.paragraph(
        "The branch has diverged, but the last remote commit was by someone else. \
         gitsitter doesn't rebase — it flags the branch and lets you decide.",
    );

    let tmp = temp_dir();
    let bare_dir = tmp.path().join("bare");
    let work_dir = tmp.path().join("work");
    let _bare = create_bare_repo(&bare_dir);
    let work = clone_repo(&bare_dir, &work_dir);

    // Set user identity
    work.config()
        .unwrap()
        .set_str("user.name", "Alice")
        .unwrap();
    work.config()
        .unwrap()
        .set_str("user.email", "alice@example.com")
        .unwrap();

    make_commit_as(
        &work,
        "README.md",
        "# project\n",
        "initial",
        "Alice",
        "alice@example.com",
    );
    let branch = work.head().unwrap().shorthand().unwrap().to_string();
    push(&work, &branch);

    // Someone else pushes to remote
    push_to_bare(
        &bare_dir,
        &branch,
        "other.txt",
        "theirs\n",
        "bob: other work",
    );

    // Local commit
    make_commit_as(
        &work,
        "mine.txt",
        "mine\n",
        "alice: my work",
        "Alice",
        "alice@example.com",
    );

    r.heading(3, "Setup");
    r.shell(
        "alice ",
        &[
            "git clone origin ~/project && cd ~/project",
            "echo '# project' > README.md && git add . && git commit -m 'initial'",
            "git push -u origin main",
        ],
    );
    r.paragraph("Bob pushes, then Alice commits locally:");
    r.shell(
        "bob   ",
        &["echo theirs > other.txt && git add . && git commit -m 'bob: other work' && git push"],
    );
    r.shell(
        "alice ",
        &["echo mine > mine.txt && git add . && git commit -m 'alice: my work'"],
    );

    let id = git_ops::discover_repo_id(work_dir.as_path()).unwrap();
    let id_str = id.to_string_lossy().to_string();
    let daemon = test_daemon(tmp.path(), &id_str, &work_dir);

    r.heading(3, "gitsitter sync");
    let result = sync_and_get_result(&daemon, &id_str, &branch).await;
    r.sync_result(&result);

    assert_eq!(result.status, "diverged");

    r.paragraph(
        "The last commit on the remote is by Bob, not Alice. gitsitter won't \
         auto-rebase someone else's branch — it flags the divergence and waits \
         for the user to resolve it.",
    );
    r.heading(3, "Result");
    r.status_line("Status", &result.status);
    r.status_line("Outcome", "flagged — user must resolve manually");
    r.finish_list();

    r.into_string()
}

async fn scenario_dirty_worktree_skipped() -> String {
    let mut r = Recorder::new();
    r.heading(2, "Scenario: Dirty worktree — skip");
    r.paragraph(
        "The remote is ahead, but you have uncommitted changes. gitsitter skips \
         the fast-forward to avoid clobbering your work.",
    );

    let tmp = temp_dir();
    let bare_dir = tmp.path().join("bare");
    let work_dir = tmp.path().join("work");
    let _bare = create_bare_repo(&bare_dir);
    let work = clone_repo(&bare_dir, &work_dir);

    make_commit(&work, "README.md", "# project\n", "initial commit");
    let branch = work.head().unwrap().shorthand().unwrap().to_string();
    push(&work, &branch);

    push_to_bare(
        &bare_dir,
        &branch,
        "update.txt",
        "new stuff\n",
        "bob: update",
    );

    // Dirty the worktree (modify a tracked file — untracked files don't count)
    std::fs::write(work_dir.join("README.md"), "# project\nwork in progress\n").unwrap();

    r.heading(3, "Setup");
    r.shell(
        "alice ",
        &[
            "git clone origin ~/project && cd ~/project",
            "echo '# project' > README.md && git add . && git commit -m 'initial commit'",
            "git push -u origin main",
        ],
    );
    r.paragraph("Bob pushes, then Alice starts editing a tracked file (without committing):");
    r.shell(
        "bob   ",
        &["echo 'new stuff' > update.txt && git add . && git commit -m 'update' && git push"],
    );
    r.shell(
        "alice ",
        &["echo 'work in progress' >> README.md   # modified but not committed"],
    );

    let id = git_ops::discover_repo_id(work_dir.as_path()).unwrap();
    let id_str = id.to_string_lossy().to_string();
    let daemon = test_daemon(tmp.path(), &id_str, &work_dir);

    r.heading(3, "gitsitter sync");
    let result = sync_and_get_result(&daemon, &id_str, &branch).await;
    r.sync_result(&result);

    assert_eq!(result.status, "pending_dirty");

    r.paragraph(
        "gitsitter sees the remote is ahead (fast-forward possible), but the worktree \
         has uncommitted changes. It skips the update to avoid data loss and will \
         retry when the worktree is clean.",
    );
    r.heading(3, "Result");
    r.status_line("Status", &result.status);
    r.status_line("Outcome", "skipped — will retry when worktree is clean");
    r.finish_list();

    r.into_string()
}

// ===========================================================================
// Doc generator
// ===========================================================================

fn generate_header() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let commit = option_env!("GIT_COMMIT_HASH").unwrap_or("unknown");

    let mut s = String::new();
    writeln!(s, "# gitsitter — workflow reference\n").unwrap();
    writeln!(
        s,
        "> **Auto-generated** by `cargo test --test workflows -- --ignored generate_workflow_docs`."
    )
    .unwrap();
    writeln!(
        s,
        "> All outputs reflect actual behavior: each scenario constructs real git \
         repositories and runs gitsitter's sync pipeline against them."
    )
    .unwrap();
    writeln!(s, "> Version **{}**, commit `{}`.", version, commit).unwrap();
    writeln!(s, "> **Do not edit** — re-run the test to regenerate.\n").unwrap();

    writeln!(
        s,
        "Each scenario below shows a simulated multi-user git workflow, then runs \
         `gitsitter sync` and shows the resulting status. Shell prompts indicate \
         which simulated host is acting (`alice`, `bob`, etc.).\n"
    )
    .unwrap();

    s
}

#[tokio::test]
#[ignore] // run explicitly: cargo test --test workflows -- --ignored
async fn generate_workflow_docs() {
    let mut doc = generate_header();
    doc += &scenario_fast_forward().await;
    doc += &scenario_local_ahead_push().await;
    doc += &scenario_normal_divergence_rebase().await;
    doc += &scenario_interactive_rebase_detected().await;
    doc += &scenario_amend_detected().await;
    doc += &scenario_rewrite_remote_advanced().await;
    doc += &scenario_diverged_not_owned().await;
    doc += &scenario_dirty_worktree_skipped().await;

    let out_path = std::env::current_dir()
        .unwrap()
        .join("docs")
        .join("workflows.md");
    std::fs::create_dir_all(out_path.parent().unwrap()).unwrap();
    std::fs::write(&out_path, &doc).unwrap();
    eprintln!("wrote {}", out_path.display());
}

// ===========================================================================
// CI-friendly test (asserts only, no file output)
// ===========================================================================

#[cfg(unix)]
#[tokio::test]
async fn workflow_scenarios_pass() {
    scenario_fast_forward().await;
    scenario_local_ahead_push().await;
    scenario_normal_divergence_rebase().await;
    scenario_interactive_rebase_detected().await;
    scenario_amend_detected().await;
    scenario_rewrite_remote_advanced().await;
    scenario_diverged_not_owned().await;
    scenario_dirty_worktree_skipped().await;
}
