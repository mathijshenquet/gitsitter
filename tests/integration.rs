use std::path::Path;
use std::time::Duration;

use tempfile::TempDir;

use gitsitter::config::{self, InRepoConfig, RepoConfig, UserConfig};
use gitsitter::paths::Paths;
use gitsitter::transport::{self, BranchStatusData, Request, Response};

// ===========================================================================
// Helper functions
// ===========================================================================

fn temp_dir() -> TempDir {
    let base = std::env::current_dir()
        .unwrap()
        .join("target")
        .join("test-tmp");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("gitsitter-test-")
        .tempdir_in(base)
        .unwrap()
}

#[cfg(unix)]
fn create_bare_repo(dir: &Path) -> git2::Repository {
    git2::Repository::init_bare(dir).unwrap()
}

#[cfg(unix)]
fn clone_repo(bare_path: &Path, working_path: &Path) -> git2::Repository {
    let url = format!("file://{}", bare_path.display());
    git2::build::RepoBuilder::new()
        .clone(&url, working_path)
        .unwrap()
}

fn make_commit(repo: &git2::Repository, filename: &str, content: &str, message: &str) {
    let workdir = repo.workdir().expect("not a bare repo");
    let file_path = workdir.join(filename);
    std::fs::write(&file_path, content).unwrap();

    let mut index = repo.index().unwrap();
    index.add_path(Path::new(filename)).unwrap();
    index.write().unwrap();

    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = git2::Signature::now("Test User", "test@example.com").unwrap();

    let parent_commit = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<&git2::Commit> = parent_commit.iter().collect();

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
    let file_path = workdir.join(filename);
    std::fs::write(&file_path, content).unwrap();

    let mut index = repo.index().unwrap();
    index.add_path(Path::new(filename)).unwrap();
    index.write().unwrap();

    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = git2::Signature::now(name, email).unwrap();

    let parent_commit = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<&git2::Commit> = parent_commit.iter().collect();

    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
        .unwrap();
}

/// Build test Paths from a temp base directory.
#[cfg(unix)]
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
// 1. Config Tests
// ===========================================================================

mod config_tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Config parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_flat_config_toml() {
        let tmp = temp_dir();
        let paths = test_paths(tmp.path());

        let config_toml = r#"
refresh_interval = "120s"
colors = false
emoji = false
notification_cooldown = "10m"
git_path = "/usr/bin/git"
"#;
        std::fs::write(&paths.config_file, config_toml).unwrap();

        // Write repos
        let repos_toml = r#"
["/home/user/project"]
refresh_interval = "30s"
disabled = false
"#;
        std::fs::write(&paths.repos_file, repos_toml).unwrap();

        // Write trusted hosts
        std::fs::write(&paths.trusted_hosts_file, "github.com\nevil.example.com\n").unwrap();

        let cfg = UserConfig::load(&paths).unwrap();

        assert_eq!(cfg.global.refresh_interval, Duration::from_secs(120));
        assert!(!cfg.global.colors);
        assert!(!cfg.global.emoji);
        assert_eq!(cfg.global.notification_cooldown, Duration::from_secs(600));
        assert_eq!(cfg.global.git_path.as_deref(), Some("/usr/bin/git"));

        assert!(cfg.trusted_hosts.contains("github.com"));
        assert!(cfg.trusted_hosts.contains("evil.example.com"));

        let repo_cfg = cfg.repos.get("/home/user/project").unwrap();
        assert_eq!(repo_cfg.refresh_interval, Some(Duration::from_secs(30)));
        assert_eq!(repo_cfg.disabled, Some(config::Disabled::All(false)));
    }

    #[test]
    fn parse_in_repo_config() {
        let tmp = temp_dir();
        let repo_root = tmp.path();
        let toml_str = r#"
refresh_interval = "5m"
"#;
        std::fs::write(repo_root.join(".gitsitter.toml"), toml_str).unwrap();

        let irc = InRepoConfig::load(repo_root).unwrap().unwrap();
        assert_eq!(irc.refresh_interval, Some(Duration::from_secs(300)));
    }

    #[test]
    fn empty_config_uses_defaults() {
        let tmp = temp_dir();
        let paths = test_paths(tmp.path());

        // No files created — all defaults
        let cfg = UserConfig::load(&paths).unwrap();
        assert_eq!(cfg.global.refresh_interval, Duration::from_secs(60));
        assert!(cfg.global.colors);
        assert!(cfg.global.emoji);
        assert_eq!(cfg.global.notification_cooldown, Duration::from_secs(300));
        assert!(cfg.global.git_path.is_none());
        assert!(cfg.trusted_hosts.is_empty());
        assert!(cfg.repos.is_empty());
    }

    #[test]
    fn parse_missing_in_repo_config() {
        let tmp = temp_dir();
        let irc = InRepoConfig::load(tmp.path()).unwrap();
        assert!(irc.is_none());
    }

    #[test]
    fn duration_parsing_60s() {
        let tmp = temp_dir();
        std::fs::write(
            tmp.path().join(".gitsitter.toml"),
            "refresh_interval = \"60s\"",
        )
        .unwrap();
        let irc = InRepoConfig::load(tmp.path()).unwrap().unwrap();
        assert_eq!(irc.refresh_interval, Some(Duration::from_secs(60)));
    }

    #[test]
    fn duration_parsing_5m() {
        let tmp = temp_dir();
        std::fs::write(
            tmp.path().join(".gitsitter.toml"),
            "refresh_interval = \"5m\"",
        )
        .unwrap();
        let irc = InRepoConfig::load(tmp.path()).unwrap().unwrap();
        assert_eq!(irc.refresh_interval, Some(Duration::from_secs(300)));
    }

    #[test]
    fn duration_parsing_1h() {
        let tmp = temp_dir();
        std::fs::write(
            tmp.path().join(".gitsitter.toml"),
            "refresh_interval = \"1h\"",
        )
        .unwrap();
        let irc = InRepoConfig::load(tmp.path()).unwrap().unwrap();
        assert_eq!(irc.refresh_interval, Some(Duration::from_secs(3600)));
    }

    #[test]
    fn duration_parsing_500ms() {
        let tmp = temp_dir();
        std::fs::write(
            tmp.path().join(".gitsitter.toml"),
            "refresh_interval = \"500ms\"",
        )
        .unwrap();
        let irc = InRepoConfig::load(tmp.path()).unwrap().unwrap();
        assert_eq!(irc.refresh_interval, Some(Duration::from_millis(500)));
    }

    // -----------------------------------------------------------------------
    // Config resolution: host trust
    // -----------------------------------------------------------------------

    #[test]
    fn no_default_trusted_hosts() {
        let cfg = UserConfig::default();
        assert!(!cfg.is_host_trusted("github.com"));
        assert!(cfg.trusted_hosts.is_empty());
    }

    #[test]
    fn trusted_hosts_from_file() {
        let tmp = temp_dir();
        let paths = test_paths(tmp.path());
        std::fs::write(&paths.trusted_hosts_file, "github.com\ngitlab.com\n").unwrap();

        let cfg = UserConfig::load(&paths).unwrap();
        assert!(cfg.is_host_trusted("github.com"));
        assert!(cfg.is_host_trusted("gitlab.com"));
        assert!(!cfg.is_host_trusted("evil.example.com"));
    }

    #[test]
    fn unknown_host_untrusted() {
        let cfg = UserConfig::default();
        assert!(!cfg.is_host_trusted("evil.example.com"));
        assert!(!cfg.is_host_trusted("my-private-git.local"));
    }

    #[test]
    fn untrusted_remote_detected() {
        let tmp = temp_dir();
        let paths = test_paths(tmp.path());
        std::fs::write(&paths.trusted_hosts_file, "github.com\n").unwrap();

        let cfg = UserConfig::load(&paths).unwrap();
        assert!(!cfg.is_remote_trusted("git@evil.example.com:user/repo.git"));
        assert!(cfg.is_remote_trusted("git@github.com:user/repo.git"));
        assert!(cfg.is_remote_trusted("")); // no remote = trusted
        assert!(cfg.is_remote_trusted("file:///local/path")); // local = trusted
    }

    #[test]
    fn repo_disabled() {
        let mut cfg = UserConfig::default();
        cfg.repos.insert(
            "/home/user/repo".to_string(),
            RepoConfig {
                disabled: Some(config::Disabled::All(true)),
                ..Default::default()
            },
        );
        assert!(cfg.is_repo_disabled("/home/user/repo"));
        assert!(!cfg.is_repo_disabled("/home/user/other"));
    }

    #[test]
    fn remote_disabled() {
        let mut cfg = UserConfig::default();
        cfg.repos.insert(
            "/home/user/repo".to_string(),
            RepoConfig {
                disabled: Some(config::Disabled::Remotes(vec!["upstream".to_string()])),
                ..Default::default()
            },
        );
        assert!(cfg.is_remote_disabled("/home/user/repo", "upstream"));
        assert!(!cfg.is_remote_disabled("/home/user/repo", "origin"));
        assert!(!cfg.is_repo_disabled("/home/user/repo"));
    }

    // -----------------------------------------------------------------------
    // URL helpers
    // -----------------------------------------------------------------------

    #[test]
    fn extract_host_ssh_scp_style() {
        let host = config::extract_host("git@github.com:user/repo.git");
        assert_eq!(host.as_deref(), Some("github.com"));
    }

    #[test]
    fn extract_host_https() {
        let host = config::extract_host("https://github.com/user/repo.git");
        assert_eq!(host.as_deref(), Some("github.com"));
    }

    #[test]
    fn extract_host_ssh_scheme() {
        let host = config::extract_host("ssh://git@github.com/user/repo.git");
        assert_eq!(host.as_deref(), Some("github.com"));
    }

    // -----------------------------------------------------------------------
    // Trust/untrust and repo operations
    // -----------------------------------------------------------------------

    #[test]
    fn trust_and_untrust_round_trip() {
        let tmp = temp_dir();
        let paths = test_paths(tmp.path());

        UserConfig::trust(&paths, "github.com").unwrap();
        UserConfig::trust(&paths, "gitlab.com").unwrap();

        let cfg = UserConfig::load(&paths).unwrap();
        assert!(cfg.is_host_trusted("github.com"));
        assert!(cfg.is_host_trusted("gitlab.com"));

        UserConfig::untrust(&paths, "github.com").unwrap();

        let cfg = UserConfig::load(&paths).unwrap();
        assert!(!cfg.is_host_trusted("github.com"));
        assert!(cfg.is_host_trusted("gitlab.com"));
    }

    #[test]
    fn update_repo_and_remove_round_trip() {
        let tmp = temp_dir();
        let paths = test_paths(tmp.path());

        UserConfig::update_repo(&paths, "/home/user/project", |repo| {
            repo.refresh_interval = Some(Duration::from_secs(30));
        })
        .unwrap();

        let cfg = UserConfig::load(&paths).unwrap();
        let repo = cfg.repos.get("/home/user/project").unwrap();
        assert_eq!(repo.refresh_interval, Some(Duration::from_secs(30)));

        UserConfig::remove_repo(&paths, "/home/user/project").unwrap();

        let cfg = UserConfig::load(&paths).unwrap();
        assert!(!cfg.repos.contains_key("/home/user/project"));
    }
}

// ===========================================================================
// 2. Transport Protocol Tests
// ===========================================================================

mod transport_tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn request_round_trip() {
        let (mut client, mut server) = duplex(4096);
        let req = Request::ReloadConfig;
        transport::send_request(&mut client, &req).await.unwrap();
        let got = transport::recv_request(&mut server).await.unwrap();
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            serde_json::to_string(&got).unwrap(),
        );
    }

    #[tokio::test]
    async fn response_round_trip() {
        let (mut client, mut server) = duplex(4096);
        let resp = Response::Ok {
            message: "registered".into(),
        };
        transport::send_response(&mut client, &resp).await.unwrap();
        let got = transport::recv_response(&mut server).await.unwrap();
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            serde_json::to_string(&got).unwrap(),
        );
    }

    #[tokio::test]
    async fn all_request_variants_serialize() {
        let variants: Vec<Request> = vec![
            Request::Status {
                repo_path: Some("/repo".into()),
                global: false,
            },
            Request::Status {
                repo_path: None,
                global: true,
            },
            Request::Sync {
                repo_path: Some("/repo".into()),
                all: false,
            },
            Request::Sync {
                repo_path: None,
                all: true,
            },
            Request::ReloadConfig,
            Request::PromptCheck {
                repo_path: "/repo".into(),
            },
            Request::DaemonStatus,
            Request::Shutdown,
        ];

        for req in &variants {
            let (mut client, mut server) = duplex(4096);
            transport::send_request(&mut client, req).await.unwrap();
            let got = transport::recv_request(&mut server).await.unwrap();
            assert_eq!(
                serde_json::to_string(req).unwrap(),
                serde_json::to_string(&got).unwrap(),
            );
        }
    }

    #[tokio::test]
    async fn all_response_variants_serialize() {
        let variants: Vec<Response> = vec![
            Response::Ok {
                message: "done".into(),
            },
            Response::Error {
                message: "fail".into(),
            },
            Response::Status {
                data: transport::StatusData {
                    repo_id: "id".into(),
                    display_path: "/path".into(),
                    last_sync: None,
                    branches: vec![transport::BranchStatusData {
                        name: "main".into(),
                        upstream: Some("origin/main".into()),
                        status: "synced".into(),
                        last_action: None,
                    }],
                    untrusted_remotes: vec![],
                    untrusted_hosts: vec![],
                    disabled_remotes: vec![],
                    remote_urls: std::collections::HashMap::new(),
                    newly_registered: false,
                },
            },
            Response::GlobalStatus {
                repos: vec![transport::RepoStatusData {
                    display_path: "/path".into(),
                    status_summary: "1 synced".into(),
                    last_sync: None,
                }],
            },
            Response::DaemonStatus {
                pid: 1234,
                uptime_secs: 3600,
                repos_watched: 5,
                latest_version: None,
            },
        ];

        for resp in &variants {
            let (mut client, mut server) = duplex(8192);
            transport::send_response(&mut client, resp).await.unwrap();
            let got = transport::recv_response(&mut server).await.unwrap();
            assert_eq!(
                serde_json::to_string(resp).unwrap(),
                serde_json::to_string(&got).unwrap(),
            );
        }
    }
}

// ===========================================================================
// 3. Git Operations Tests
// ===========================================================================

mod git_ops_tests {
    use super::*;
    use gitsitter::git_ops;

    #[test]
    fn discover_repo_id_finds_git_dir() {
        let tmp = temp_dir();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        make_commit(&repo, "README.md", "hello", "Initial commit");

        let repo_id = git_ops::discover_repo_id(tmp.path()).unwrap();
        assert!(repo_id.exists());
        assert!(repo_id.is_dir());
    }

    #[test]
    fn get_display_path_returns_working_tree() {
        let tmp = temp_dir();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        make_commit(&repo, "README.md", "hello", "Initial commit");

        let repo_id = git_ops::discover_repo_id(tmp.path()).unwrap();
        let display = git_ops::get_display_path(&repo_id).unwrap();
        assert_eq!(
            display.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn list_branches_finds_default_branch() {
        let tmp = temp_dir();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        make_commit(&repo, "README.md", "hello", "Initial commit");

        let repo_id = git_ops::discover_repo_id(tmp.path()).unwrap();
        let branches = git_ops::list_branches(&repo_id).unwrap();
        assert!(!branches.is_empty());
        let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
        assert!(
            names.contains(&"main") || names.contains(&"master"),
            "expected 'main' or 'master', got: {:?}",
            names
        );
    }

    #[test]
    fn is_valid_repo_true_for_repo() {
        let tmp = temp_dir();
        git2::Repository::init(tmp.path()).unwrap();
        assert!(git_ops::is_valid_repo(tmp.path()));
    }

    #[test]
    fn is_valid_repo_false_for_random_dir() {
        let tmp = temp_dir();
        assert!(!git_ops::is_valid_repo(tmp.path()));
    }

    #[test]
    fn is_operation_in_progress_false_normally() {
        let tmp = temp_dir();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        make_commit(&repo, "README.md", "hello", "Initial commit");

        let repo_id = git_ops::discover_repo_id(tmp.path()).unwrap();
        assert!(!git_ops::is_operation_in_progress(&repo_id));
    }

    #[test]
    fn is_operation_in_progress_true_with_index_lock() {
        let tmp = temp_dir();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        make_commit(&repo, "README.md", "hello", "Initial commit");

        let repo_id = git_ops::discover_repo_id(tmp.path()).unwrap();
        let lock_path = repo_id.join("index.lock");
        std::fs::write(&lock_path, "").unwrap();

        assert!(git_ops::is_operation_in_progress(&repo_id));

        std::fs::remove_file(&lock_path).unwrap();
    }

    #[test]
    fn is_worktree_dirty_clean_repo() {
        let tmp = temp_dir();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        make_commit(&repo, "README.md", "hello", "Initial commit");

        let dirty = git_ops::is_worktree_dirty(tmp.path()).unwrap();
        assert!(!dirty);
    }

    #[test]
    fn is_worktree_dirty_modified_file() {
        let tmp = temp_dir();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        make_commit(&repo, "README.md", "hello", "Initial commit");

        std::fs::write(tmp.path().join("README.md"), "modified content").unwrap();
        let dirty = git_ops::is_worktree_dirty(tmp.path()).unwrap();
        assert!(dirty);
    }

    #[test]
    fn get_current_user_email_reads_config() {
        let tmp = temp_dir();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        repo.config()
            .unwrap()
            .set_str("user.email", "me@example.com")
            .unwrap();

        let repo_id = git_ops::discover_repo_id(tmp.path()).unwrap();
        let email = git_ops::get_current_user_email(&repo_id).unwrap();
        assert_eq!(email.as_deref(), Some("me@example.com"));
    }

    #[cfg(unix)]
    #[test]
    fn is_branch_owned_by_user() {
        // Set up bare + clone to have a proper upstream
        let tmp = temp_dir();
        let bare_dir = tmp.path().join("bare");
        let work_dir = tmp.path().join("work");

        let bare = create_bare_repo(&bare_dir);
        let work = clone_repo(&bare_dir, &work_dir);

        // Configure user email in the working repo
        work.config()
            .unwrap()
            .set_str("user.email", "me@example.com")
            .unwrap();

        // Make a commit as "me" and push
        make_commit_as(
            &work,
            "file.txt",
            "content",
            "my commit",
            "Me",
            "me@example.com",
        );
        // Push to origin
        let mut remote = work.find_remote("origin").unwrap();
        let branch = work.head().unwrap().shorthand().unwrap().to_string();
        remote
            .push(&[&format!("refs/heads/{}", branch)], None)
            .unwrap();
        drop(remote);

        // Fetch to update remote tracking refs
        let mut remote = work.find_remote("origin").unwrap();
        remote.fetch::<&str>(&[], None, None).unwrap();

        let repo_id = git_ops::discover_repo_id(work_dir.as_path()).unwrap();
        assert!(git_ops::is_branch_owned_by_user(&repo_id, &branch).unwrap());

        // Now push a commit from a different user
        make_commit_as(
            &work,
            "file2.txt",
            "other",
            "their commit",
            "Other",
            "other@example.com",
        );
        let mut remote = work.find_remote("origin").unwrap();
        remote
            .push(&[&format!("refs/heads/{}", branch)], None)
            .unwrap();
        drop(remote);

        let mut remote = work.find_remote("origin").unwrap();
        remote.fetch::<&str>(&[], None, None).unwrap();

        // Now the upstream tip is by "other" — should not be owned
        assert!(!git_ops::is_branch_owned_by_user(&repo_id, &branch).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn detect_history_rewrite_none_for_normal_divergence() {
        // Normal divergence: both sides add new commits on top of a shared base.
        // The reflog shows the branch always advancing forward, so no rewrite is detected.
        let tmp = temp_dir();
        let bare_dir = tmp.path().join("bare");
        let work_dir = tmp.path().join("work");

        let _bare = create_bare_repo(&bare_dir);
        let work = clone_repo(&bare_dir, &work_dir);

        // Initial commit + push
        make_commit(&work, "a.txt", "a", "initial");
        let branch = work.head().unwrap().shorthand().unwrap().to_string();
        let mut remote = work.find_remote("origin").unwrap();
        remote.push(&[&format!("refs/heads/{}", branch)], None).unwrap();
        drop(remote);

        // Simulate remote advancing: push a commit directly to bare
        {
            let bare_repo = git2::Repository::open(&bare_dir).unwrap();
            let head_oid = bare_repo.head().unwrap().target().unwrap();
            let head_commit = bare_repo.find_commit(head_oid).unwrap();
            let tree = head_commit.tree().unwrap();
            // Create a new blob + tree for the remote commit
            let blob_oid = bare_repo.blob(b"remote content").unwrap();
            let mut tb = bare_repo.treebuilder(Some(&tree)).unwrap();
            tb.insert("remote.txt", blob_oid, 0o100644).unwrap();
            let new_tree_oid = tb.write().unwrap();
            let new_tree = bare_repo.find_tree(new_tree_oid).unwrap();
            let sig = git2::Signature::now("Remote", "remote@example.com").unwrap();
            bare_repo.commit(
                Some(&format!("refs/heads/{}", branch)),
                &sig, &sig,
                "remote commit",
                &new_tree,
                &[&head_commit],
            ).unwrap();
        }

        // Add a local commit (normal divergence — local just advances)
        make_commit(&work, "b.txt", "b", "local commit");

        // Fetch to get the remote advance
        let mut remote = work.find_remote("origin").unwrap();
        remote.fetch::<&str>(&[], None, None).unwrap();
        drop(remote);

        let repo_id = git_ops::discover_repo_id(work_dir.as_path()).unwrap();
        let analysis = git_ops::analyze_merge(&repo_id, &branch).unwrap();
        assert_eq!(analysis, git_ops::MergeAnalysis::Diverged);

        let rewrite = git_ops::detect_history_rewrite(&repo_id, &branch).unwrap();
        assert_eq!(rewrite, git_ops::HistoryRewrite::None);
    }

    #[cfg(unix)]
    #[test]
    fn detect_history_rewrite_remote_unchanged() {
        // Simulate: user publishes commits, then rewrites local history (e.g. rebase -i).
        // Remote stays at the old published tip.
        let tmp = temp_dir();
        let bare_dir = tmp.path().join("bare");
        let work_dir = tmp.path().join("work");

        let _bare = create_bare_repo(&bare_dir);
        let work = clone_repo(&bare_dir, &work_dir);

        // Commit A + push
        make_commit(&work, "a.txt", "a", "commit A");
        let branch = work.head().unwrap().shorthand().unwrap().to_string();
        let mut remote = work.find_remote("origin").unwrap();
        remote.push(&[&format!("refs/heads/{}", branch)], None).unwrap();
        drop(remote);

        // Commit B + push (this is the published tip H that will become R)
        make_commit(&work, "b.txt", "b", "commit B");
        let mut remote = work.find_remote("origin").unwrap();
        remote.push(&[&format!("refs/heads/{}", branch)], None).unwrap();
        drop(remote);

        // Now "rewrite" local history: reset to commit A and make a new commit.
        // This simulates `git rebase -i` that squashed B away.
        let published_oid = work.head().unwrap().target().unwrap();
        let commit_a = work.find_commit(published_oid).unwrap()
            .parent(0).unwrap();
        work.reset(commit_a.as_object(), git2::ResetType::Hard, None).unwrap();
        // Make a new, different commit (the rewritten history)
        make_commit(&work, "c.txt", "c", "rewritten commit C");

        // Fetch to update tracking refs
        let mut remote = work.find_remote("origin").unwrap();
        remote.fetch::<&str>(&[], None, None).unwrap();
        drop(remote);

        let repo_id = git_ops::discover_repo_id(work_dir.as_path()).unwrap();
        let analysis = git_ops::analyze_merge(&repo_id, &branch).unwrap();
        assert_eq!(analysis, git_ops::MergeAnalysis::Diverged);

        let rewrite = git_ops::detect_history_rewrite(&repo_id, &branch).unwrap();
        assert_eq!(rewrite, git_ops::HistoryRewrite::RemoteUnchanged);
    }

    #[cfg(unix)]
    #[test]
    fn detect_history_rewrite_remote_advanced() {
        // Simulate: user publishes commits, then rewrites local history,
        // but remote has also advanced past the old published tip.
        let tmp = temp_dir();
        let bare_dir = tmp.path().join("bare");
        let work_dir = tmp.path().join("work");

        let _bare = create_bare_repo(&bare_dir);
        let work = clone_repo(&bare_dir, &work_dir);

        // Commit A + push (base)
        make_commit(&work, "a.txt", "a", "commit A");
        let branch = work.head().unwrap().shorthand().unwrap().to_string();
        let mut remote = work.find_remote("origin").unwrap();
        remote.push(&[&format!("refs/heads/{}", branch)], None).unwrap();
        drop(remote);

        let base_oid = work.head().unwrap().target().unwrap();

        // Commit B + push (published tip — this becomes H in the reflog)
        make_commit(&work, "b.txt", "b", "commit B");
        let mut remote = work.find_remote("origin").unwrap();
        remote.push(&[&format!("refs/heads/{}", branch)], None).unwrap();
        drop(remote);

        // "Rewrite" local: reset back to A, then create a different commit
        let commit_a = work.find_commit(base_oid).unwrap();
        work.reset(commit_a.as_object(), git2::ResetType::Hard, None).unwrap();
        make_commit(&work, "d.txt", "d", "rewritten commit D");

        // Simulate remote advancing past the published tip B
        {
            let bare_repo = git2::Repository::open(&bare_dir).unwrap();
            let head_oid = bare_repo.head().unwrap().target().unwrap();
            let head_commit = bare_repo.find_commit(head_oid).unwrap();
            let tree = head_commit.tree().unwrap();
            let blob_oid = bare_repo.blob(b"remote extra").unwrap();
            let mut tb = bare_repo.treebuilder(Some(&tree)).unwrap();
            tb.insert("remote_extra.txt", blob_oid, 0o100644).unwrap();
            let new_tree_oid = tb.write().unwrap();
            let new_tree = bare_repo.find_tree(new_tree_oid).unwrap();
            let sig = git2::Signature::now("Remote", "remote@example.com").unwrap();
            bare_repo.commit(
                Some(&format!("refs/heads/{}", branch)),
                &sig, &sig,
                "remote advance",
                &new_tree,
                &[&head_commit],
            ).unwrap();
        }

        // Fetch
        let mut remote = work.find_remote("origin").unwrap();
        remote.fetch::<&str>(&[], None, None).unwrap();
        drop(remote);

        let repo_id = git_ops::discover_repo_id(work_dir.as_path()).unwrap();
        let analysis = git_ops::analyze_merge(&repo_id, &branch).unwrap();
        assert_eq!(analysis, git_ops::MergeAnalysis::Diverged);

        let rewrite = git_ops::detect_history_rewrite(&repo_id, &branch).unwrap();
        assert_eq!(rewrite, git_ops::HistoryRewrite::RemoteAdvanced);
    }
}
