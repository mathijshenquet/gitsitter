use std::path::Path;
use std::time::Duration;

use tempfile::TempDir;

use gitsitter::config::{
    self, BranchSyncMode, InRepoConfig, RepoConfig,
    RepoSyncMode, UserConfig,
};
use gitsitter::state::{BranchState, StateDb};
use gitsitter::transport::{self, Request, Response};

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

/// Set up env overrides pointing to a temp directory structure.
/// Returns (config_dir, state_dir, socket_path).
#[cfg(unix)]
fn setup_test_env(base: &Path) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let config_dir = base.join("config");
    let state_dir = base.join("state");
    let socket_path = base.join("gitsitter-test.sock");

    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();

    unsafe {
        std::env::set_var("GITSITTER_CONFIG_DIR", &config_dir);
        std::env::set_var("GITSITTER_STATE_DIR", &state_dir);
        std::env::set_var("GITSITTER_SOCKET_PATH", &socket_path);
    }

    (config_dir, state_dir, socket_path)
}

#[cfg(unix)]
fn teardown_test_env() {
    unsafe {
        std::env::remove_var("GITSITTER_CONFIG_DIR");
        std::env::remove_var("GITSITTER_STATE_DIR");
        std::env::remove_var("GITSITTER_SOCKET_PATH");
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
    fn parse_complete_user_config() {
        // Test complete config parsing via in-repo config features
        // and a UserConfig constructed from defaults + manual assertions.
        // We test the TOML parsing through InRepoConfig (which doesn't need env vars)
        // and test the UserConfig fields directly.

        // First, verify InRepoConfig parsing covers most TOML features
        let tmp = temp_dir();
        let toml_str = r#"
mode = "push"
refresh_interval = "120s"

[branches]
"main" = "push+pull"
"release/*" = "none"
"#;
        std::fs::write(tmp.path().join(".gitsitter.toml"), toml_str).unwrap();
        let irc = InRepoConfig::load(tmp.path()).unwrap().unwrap();
        assert_eq!(irc.mode, Some(RepoSyncMode::Push));
        assert_eq!(irc.refresh_interval, Some(Duration::from_secs(120)));
        assert_eq!(irc.branches.len(), 2);
        assert_eq!(irc.branches[0].0, "main");
        assert_eq!(irc.branches[0].1, BranchSyncMode::PushPull);
        assert_eq!(irc.branches[1].0, "release/*");
        assert_eq!(irc.branches[1].1, BranchSyncMode::None);

        // Now test a full UserConfig by loading from a temp env dir.
        // We set+use+unset the env var as quickly as possible.
        let tmp2 = temp_dir();
        let config_dir = tmp2.path().join("cfg");
        std::fs::create_dir_all(&config_dir).unwrap();

        let full_toml = r#"
[global]
refresh_interval = "120s"
colors = false
emoji = false
notification_cooldown = "10m"
git_path = "/usr/bin/git"

[trusted_hosts]
"github.com" = true
"evil.example.com" = false

[defaults.remotes]
"github.com/*" = "push+pull"
"gitlab.com/*" = "fetch"

[defaults.branches]
"main" = "push+pull"
"develop" = "pull"
"feature/*" = "push"

[repos."/home/user/project"]
mode = "push"
refresh_interval = "30s"
disabled = false

[repos."/home/user/project".branches]
"main" = "push+pull"
"release/*" = "none"
"#;
        std::fs::write(config_dir.join("config.toml"), full_toml).unwrap();

        unsafe { std::env::set_var("GITSITTER_CONFIG_DIR", &config_dir); }
        let cfg = UserConfig::load().unwrap();
        unsafe { std::env::remove_var("GITSITTER_CONFIG_DIR"); }

        assert_eq!(cfg.global.refresh_interval, Duration::from_secs(120));
        assert!(!cfg.global.colors);
        assert!(!cfg.global.emoji);
        assert_eq!(cfg.global.notification_cooldown, Duration::from_secs(600));
        assert_eq!(cfg.global.git_path.as_deref(), Some("/usr/bin/git"));

        assert_eq!(cfg.trusted_hosts.get("github.com"), Some(&true));
        assert_eq!(cfg.trusted_hosts.get("evil.example.com"), Some(&false));

        assert_eq!(cfg.defaults.remotes.len(), 2);
        assert_eq!(cfg.defaults.remotes[0].0, "github.com/*");
        assert_eq!(cfg.defaults.remotes[0].1, RepoSyncMode::PushPull);
        assert_eq!(cfg.defaults.remotes[1].0, "gitlab.com/*");
        assert_eq!(cfg.defaults.remotes[1].1, RepoSyncMode::Fetch);

        assert_eq!(cfg.defaults.branches.len(), 3);

        let repo_cfg = cfg.repos.get("/home/user/project").unwrap();
        assert_eq!(repo_cfg.mode, Some(RepoSyncMode::Push));
        assert_eq!(repo_cfg.refresh_interval, Some(Duration::from_secs(30)));
        assert_eq!(repo_cfg.disabled, Some(false));
        assert_eq!(repo_cfg.branches.len(), 2);
    }

    #[test]
    fn parse_in_repo_config() {
        let tmp = temp_dir();
        let repo_root = tmp.path();
        let toml_str = r#"
mode = "fetch"
refresh_interval = "5m"

[branches]
"main" = "push+pull"
"staging" = "pull"
"#;
        std::fs::write(repo_root.join(".gitsitter.toml"), toml_str).unwrap();

        let irc = InRepoConfig::load(repo_root).unwrap().unwrap();
        assert_eq!(irc.mode, Some(RepoSyncMode::Fetch));
        assert_eq!(irc.refresh_interval, Some(Duration::from_secs(300)));
        assert_eq!(irc.branches.len(), 2);
        assert_eq!(irc.branches[0].0, "main");
        assert_eq!(irc.branches[0].1, BranchSyncMode::PushPull);
    }

    #[test]
    fn parse_empty_config() {
        // Empty in-repo config should parse to None for all optional fields
        let tmp = temp_dir();
        std::fs::write(tmp.path().join(".gitsitter.toml"), "").unwrap();
        let irc = InRepoConfig::load(tmp.path()).unwrap().unwrap();
        assert!(irc.mode.is_none());
        assert!(irc.refresh_interval.is_none());
        assert!(irc.branches.is_empty());

        // Also test UserConfig defaults
        let cfg = UserConfig::default();
        assert_eq!(cfg.global.refresh_interval, Duration::from_secs(60));
        assert!(cfg.global.colors);
        assert!(cfg.global.emoji);
        assert_eq!(cfg.global.notification_cooldown, Duration::from_secs(300));
        assert!(cfg.global.git_path.is_none());
        assert!(cfg.trusted_hosts.is_empty());
        assert!(cfg.defaults.remotes.is_empty());
        assert!(cfg.repos.is_empty());
    }

    #[test]
    fn parse_missing_config_returns_defaults() {
        // InRepoConfig returns None when file doesn't exist
        let tmp = temp_dir();
        let irc = InRepoConfig::load(tmp.path()).unwrap();
        assert!(irc.is_none());

        // UserConfig::default() provides sensible defaults
        let cfg = UserConfig::default();
        assert_eq!(cfg.global.refresh_interval, Duration::from_secs(60));
        assert!(cfg.global.colors);
    }

    #[test]
    fn duration_parsing_60s() {
        // Test duration parsing via in-repo config (no env vars needed)
        let tmp = temp_dir();
        std::fs::write(tmp.path().join(".gitsitter.toml"), "refresh_interval = \"60s\"").unwrap();
        let irc = InRepoConfig::load(tmp.path()).unwrap().unwrap();
        assert_eq!(irc.refresh_interval, Some(Duration::from_secs(60)));
    }

    #[test]
    fn duration_parsing_5m() {
        let tmp = temp_dir();
        std::fs::write(tmp.path().join(".gitsitter.toml"), "refresh_interval = \"5m\"").unwrap();
        let irc = InRepoConfig::load(tmp.path()).unwrap().unwrap();
        assert_eq!(irc.refresh_interval, Some(Duration::from_secs(300)));
    }

    #[test]
    fn duration_parsing_1h() {
        let tmp = temp_dir();
        std::fs::write(tmp.path().join(".gitsitter.toml"), "refresh_interval = \"1h\"").unwrap();
        let irc = InRepoConfig::load(tmp.path()).unwrap().unwrap();
        assert_eq!(irc.refresh_interval, Some(Duration::from_secs(3600)));
    }

    #[test]
    fn duration_parsing_500ms() {
        let tmp = temp_dir();
        std::fs::write(tmp.path().join(".gitsitter.toml"), "refresh_interval = \"500ms\"").unwrap();
        let irc = InRepoConfig::load(tmp.path()).unwrap().unwrap();
        assert_eq!(irc.refresh_interval, Some(Duration::from_millis(500)));
    }

    #[test]
    fn sync_mode_deserialization() {
        // Test via in-repo config parsing (no env vars needed)
        let cases = vec![
            ("none", RepoSyncMode::None),
            ("fetch", RepoSyncMode::Fetch),
            ("pull", RepoSyncMode::Pull),
            ("push", RepoSyncMode::Push),
            ("push+pull", RepoSyncMode::PushPull),
        ];

        for (input, expected) in cases {
            let tmp = temp_dir();
            let toml_str = format!("mode = \"{}\"", input);
            std::fs::write(tmp.path().join(".gitsitter.toml"), &toml_str).unwrap();
            let irc = InRepoConfig::load(tmp.path()).unwrap().unwrap();
            assert_eq!(irc.mode, Some(expected), "failed for input: {}", input);
        }
    }

    // -----------------------------------------------------------------------
    // Config resolution: host trust
    // -----------------------------------------------------------------------

    #[test]
    fn builtin_hosts_trusted_by_default() {
        let cfg = UserConfig::default();
        assert!(cfg.is_host_trusted("github.com"));
        assert!(cfg.is_host_trusted("gitlab.com"));
        assert!(cfg.is_host_trusted("codeberg.org"));
        assert!(cfg.is_host_trusted("bitbucket.org"));
        assert!(cfg.is_host_trusted("sr.ht"));
    }

    #[test]
    fn unknown_host_untrusted() {
        let cfg = UserConfig::default();
        assert!(!cfg.is_host_trusted("evil.example.com"));
        assert!(!cfg.is_host_trusted("my-private-git.local"));
    }

    #[test]
    fn explicitly_disabled_builtin_host() {
        let mut cfg = UserConfig::default();
        cfg.trusted_hosts.insert("github.com".to_string(), false);
        assert!(!cfg.is_host_trusted("github.com"));
        // Other builtins still trusted
        assert!(cfg.is_host_trusted("gitlab.com"));
    }

    // -----------------------------------------------------------------------
    // Config resolution: repo mode
    // -----------------------------------------------------------------------

    #[test]
    fn repo_mode_untrusted_host_returns_none() {
        let cfg = UserConfig::default();
        let mode = cfg.resolve_repo_mode(
            "git@evil.example.com:user/repo.git",
            "/home/user/repo",
            None,
        );
        assert_eq!(mode, RepoSyncMode::None);
    }

    #[test]
    fn repo_mode_user_per_repo_wins_over_in_repo() {
        let mut cfg = UserConfig::default();
        cfg.repos.insert(
            "/home/user/repo".to_string(),
            RepoConfig {
                mode: Some(RepoSyncMode::Push),
                ..Default::default()
            },
        );
        let in_repo = InRepoConfig {
            mode: Some(RepoSyncMode::Pull),
            refresh_interval: None,
            branches: vec![],
        };
        let mode = cfg.resolve_repo_mode(
            "git@github.com:user/repo.git",
            "/home/user/repo",
            Some(&in_repo),
        );
        assert_eq!(mode, RepoSyncMode::Push);
    }

    #[test]
    fn repo_mode_in_repo_wins_over_defaults_glob() {
        let mut cfg = UserConfig::default();
        cfg.defaults.remotes.push((
            "github.com/*".to_string(),
            RepoSyncMode::Fetch,
        ));
        let in_repo = InRepoConfig {
            mode: Some(RepoSyncMode::PushPull),
            refresh_interval: None,
            branches: vec![],
        };
        let mode = cfg.resolve_repo_mode(
            "https://github.com/user/repo.git",
            "/home/user/repo",
            Some(&in_repo),
        );
        assert_eq!(mode, RepoSyncMode::PushPull);
    }

    #[test]
    fn repo_mode_defaults_remotes_glob_match() {
        let mut cfg = UserConfig::default();
        cfg.defaults.remotes.push((
            "*github.com*".to_string(),
            RepoSyncMode::Fetch,
        ));
        let mode = cfg.resolve_repo_mode(
            "https://github.com/user/repo.git",
            "/home/user/repo",
            None,
        );
        assert_eq!(mode, RepoSyncMode::Fetch);
    }

    #[test]
    fn repo_mode_fallback_to_pull() {
        let cfg = UserConfig::default();
        let mode = cfg.resolve_repo_mode(
            "git@github.com:user/repo.git",
            "/home/user/repo",
            None,
        );
        assert_eq!(mode, RepoSyncMode::Pull);
    }

    #[test]
    fn repo_mode_disabled_returns_none() {
        let mut cfg = UserConfig::default();
        cfg.repos.insert(
            "/home/user/repo".to_string(),
            RepoConfig {
                disabled: Some(true),
                ..Default::default()
            },
        );
        let mode = cfg.resolve_repo_mode(
            "git@github.com:user/repo.git",
            "/home/user/repo",
            None,
        );
        assert_eq!(mode, RepoSyncMode::None);
    }

    // -----------------------------------------------------------------------
    // Config resolution: branch mode
    // -----------------------------------------------------------------------

    #[test]
    fn branch_mode_exact_match_beats_glob() {
        let mut cfg = UserConfig::default();
        cfg.repos.insert(
            "/repo".to_string(),
            RepoConfig {
                branches: vec![
                    ("feature/*".to_string(), BranchSyncMode::Pull),
                    ("feature/special".to_string(), BranchSyncMode::Push),
                ],
                ..Default::default()
            },
        );
        let mode = cfg.resolve_branch_mode(
            "/repo",
            "feature/special",
            None,
            RepoSyncMode::Pull,
        );
        assert_eq!(mode, BranchSyncMode::Push);
    }

    #[test]
    fn branch_mode_longer_glob_beats_shorter() {
        let mut cfg = UserConfig::default();
        cfg.repos.insert(
            "/repo".to_string(),
            RepoConfig {
                branches: vec![
                    ("f*".to_string(), BranchSyncMode::Pull),
                    ("feature/*".to_string(), BranchSyncMode::Push),
                ],
                ..Default::default()
            },
        );
        let mode = cfg.resolve_branch_mode(
            "/repo",
            "feature/foo",
            None,
            RepoSyncMode::Pull,
        );
        assert_eq!(mode, BranchSyncMode::Push);
    }

    #[test]
    fn branch_mode_user_config_beats_in_repo() {
        let mut cfg = UserConfig::default();
        cfg.repos.insert(
            "/repo".to_string(),
            RepoConfig {
                branches: vec![("main".to_string(), BranchSyncMode::Push)],
                ..Default::default()
            },
        );
        let in_repo = InRepoConfig {
            mode: None,
            refresh_interval: None,
            branches: vec![("main".to_string(), BranchSyncMode::Pull)],
        };
        let mode = cfg.resolve_branch_mode(
            "/repo",
            "main",
            Some(&in_repo),
            RepoSyncMode::Pull,
        );
        assert_eq!(mode, BranchSyncMode::Push);
    }

    #[test]
    fn branch_mode_inherits_from_repo_when_no_match() {
        let cfg = UserConfig::default();
        let mode = cfg.resolve_branch_mode(
            "/repo",
            "some-branch",
            None,
            RepoSyncMode::PushPull,
        );
        assert_eq!(mode, BranchSyncMode::PushPull);
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

    #[test]
    fn matches_remote_glob_exact() {
        assert!(config::matches_remote_glob(
            "https://github.com/user/repo.git",
            "https://github.com/user/repo.git"
        ));
    }

    #[test]
    fn matches_remote_glob_wildcard() {
        assert!(config::matches_remote_glob(
            "https://github.com/user/repo.git",
            "*github.com*"
        ));
        assert!(!config::matches_remote_glob(
            "https://gitlab.com/user/repo.git",
            "*github.com*"
        ));
    }

    #[test]
    fn matches_branch_glob_feature() {
        assert!(config::matches_branch_glob("feature/foo", "feature/*"));
        assert!(!config::matches_branch_glob("bugfix/foo", "feature/*"));
    }

    // -----------------------------------------------------------------------
    // Config round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn config_save_and_load_round_trip() {
        // This test verifies that saving a UserConfig and loading it back
        // produces the same values. We use env vars to redirect config paths,
        // and do save+load atomically (back-to-back).
        let tmp = temp_dir();
        let config_dir = tmp.path().join("config_rt");
        std::fs::create_dir_all(&config_dir).unwrap();

        // Set env vars, save, load, then restore -- all in sequence.
        // There is inherent raciness with env vars in parallel tests,
        // but we minimize the window.
        unsafe {
            std::env::set_var("GITSITTER_CONFIG_DIR", &config_dir);
        }

        let mut cfg = UserConfig::default();
        cfg.global.refresh_interval = Duration::from_secs(120);
        cfg.global.colors = false;
        cfg.global.emoji = false;
        cfg.global.notification_cooldown = Duration::from_secs(600);
        cfg.global.git_path = Some("/usr/bin/git".to_string());
        cfg.trusted_hosts.insert("myhost.com".to_string(), true);
        cfg.defaults.remotes.push(("*github.com*".to_string(), RepoSyncMode::PushPull));
        cfg.defaults.branches.push(("main".to_string(), BranchSyncMode::PushPull));
        cfg.repos.insert(
            "/home/user/project".to_string(),
            RepoConfig {
                mode: Some(RepoSyncMode::Push),
                refresh_interval: Some(Duration::from_secs(30)),
                disabled: Some(false),
                branches: vec![("dev".to_string(), BranchSyncMode::Pull)],
            },
        );

        cfg.save().unwrap();
        let loaded = UserConfig::load().unwrap();

        unsafe {
            std::env::remove_var("GITSITTER_CONFIG_DIR");
        }

        // Verify the loaded config matches what we saved
        assert_eq!(loaded.global.refresh_interval, Duration::from_secs(120));
        assert!(!loaded.global.colors);
        assert!(!loaded.global.emoji);
        assert_eq!(loaded.global.notification_cooldown, Duration::from_secs(600));
        assert_eq!(loaded.global.git_path.as_deref(), Some("/usr/bin/git"));
        assert_eq!(loaded.trusted_hosts.get("myhost.com"), Some(&true));
        assert_eq!(loaded.defaults.remotes.len(), 1);
        assert_eq!(loaded.defaults.branches.len(), 1);
        let repo = loaded.repos.get("/home/user/project").unwrap();
        assert_eq!(repo.mode, Some(RepoSyncMode::Push));
        assert_eq!(repo.refresh_interval, Some(Duration::from_secs(30)));
        assert_eq!(repo.branches.len(), 1);
        assert_eq!(repo.branches[0].0, "dev");
        assert_eq!(repo.branches[0].1, BranchSyncMode::Pull);
    }
}

// ===========================================================================
// 2. State Database Tests
// ===========================================================================

mod state_tests {
    use super::*;

    fn open_temp_db() -> (TempDir, StateDb) {
        let tmp = temp_dir();
        let db_path = tmp.path().join("test.db");
        let db = StateDb::open_at(&db_path).unwrap();
        (tmp, db)
    }

    #[test]
    fn open_database_in_temp_dir() {
        let (_tmp, _db) = open_temp_db();
        // Just verify it opens without error
    }

    #[test]
    fn upsert_and_get_repo() {
        let (_tmp, db) = open_temp_db();
        db.upsert_repo("repo1", "/home/user/repo1", Some("https://github.com/user/repo1.git"))
            .unwrap();

        let repo = db.get_repo("repo1").unwrap().unwrap();
        assert_eq!(repo.repo_id, "repo1");
        assert_eq!(repo.display_path, "/home/user/repo1");
        assert_eq!(
            repo.remote_url.as_deref(),
            Some("https://github.com/user/repo1.git")
        );
        assert_eq!(repo.status, "active");
    }

    #[test]
    fn list_repos_empty_then_with_data() {
        let (_tmp, db) = open_temp_db();

        let repos = db.list_repos().unwrap();
        assert!(repos.is_empty());

        db.upsert_repo("repo_a", "/a", None).unwrap();
        db.upsert_repo("repo_b", "/b", None).unwrap();

        let repos = db.list_repos().unwrap();
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].repo_id, "repo_a");
        assert_eq!(repos[1].repo_id, "repo_b");
    }

    #[test]
    fn set_repo_status() {
        let (_tmp, db) = open_temp_db();
        db.upsert_repo("r1", "/r1", None).unwrap();

        assert_eq!(db.get_repo("r1").unwrap().unwrap().status, "active");

        db.set_repo_missing("r1").unwrap();
        assert_eq!(db.get_repo("r1").unwrap().unwrap().status, "missing");

        db.set_repo_status("r1", "active").unwrap();
        assert_eq!(db.get_repo("r1").unwrap().unwrap().status, "active");
    }

    #[test]
    fn update_fetch_and_sync_timestamps() {
        let (_tmp, db) = open_temp_db();
        db.upsert_repo("r1", "/r1", None).unwrap();

        // Initially null
        let repo = db.get_repo("r1").unwrap().unwrap();
        assert!(repo.last_fetch_at.is_none());
        assert!(repo.last_sync_at.is_none());

        db.update_repo_fetch_time("r1").unwrap();
        let repo = db.get_repo("r1").unwrap().unwrap();
        assert!(repo.last_fetch_at.is_some());
        assert!(repo.last_sync_at.is_none());

        db.update_repo_sync_time("r1").unwrap();
        let repo = db.get_repo("r1").unwrap().unwrap();
        assert!(repo.last_fetch_at.is_some());
        assert!(repo.last_sync_at.is_some());
    }

    #[test]
    fn upsert_and_list_branches() {
        let (_tmp, db) = open_temp_db();
        db.upsert_repo("r1", "/r1", None).unwrap();

        let b1 = BranchState {
            branch_name: "main".to_string(),
            sync_status: "synced".to_string(),
            last_pull_at: None,
            last_push_at: None,
            local_oid: Some("abc123".to_string()),
            remote_oid: Some("abc123".to_string()),
            error_message: None,
            push_backoff_until: None,
        };
        let b2 = BranchState {
            branch_name: "develop".to_string(),
            sync_status: "diverged".to_string(),
            last_pull_at: None,
            last_push_at: None,
            local_oid: Some("def456".to_string()),
            remote_oid: Some("ghi789".to_string()),
            error_message: None,
            push_backoff_until: None,
        };

        db.upsert_branch("r1", &b1).unwrap();
        db.upsert_branch("r1", &b2).unwrap();

        let branches = db.list_branches("r1").unwrap();
        assert_eq!(branches.len(), 2);
        assert_eq!(branches[0].branch_name, "develop");
        assert_eq!(branches[1].branch_name, "main");
    }

    #[test]
    fn branch_state_updates() {
        let (_tmp, db) = open_temp_db();
        db.upsert_repo("r1", "/r1", None).unwrap();

        let b = BranchState {
            branch_name: "main".to_string(),
            sync_status: "synced".to_string(),
            last_pull_at: None,
            last_push_at: None,
            local_oid: Some("abc".to_string()),
            remote_oid: Some("abc".to_string()),
            error_message: None,
            push_backoff_until: None,
        };
        db.upsert_branch("r1", &b).unwrap();

        let fetched = db.get_branch("r1", "main").unwrap().unwrap();
        assert_eq!(fetched.sync_status, "synced");

        // Update to diverged
        let b2 = BranchState {
            branch_name: "main".to_string(),
            sync_status: "diverged".to_string(),
            local_oid: Some("abc".to_string()),
            remote_oid: Some("xyz".to_string()),
            ..b.clone()
        };
        db.upsert_branch("r1", &b2).unwrap();

        let fetched = db.get_branch("r1", "main").unwrap().unwrap();
        assert_eq!(fetched.sync_status, "diverged");

        // Back to synced
        let b3 = BranchState {
            sync_status: "synced".to_string(),
            remote_oid: Some("abc".to_string()),
            ..b2
        };
        db.upsert_branch("r1", &b3).unwrap();

        let fetched = db.get_branch("r1", "main").unwrap().unwrap();
        assert_eq!(fetched.sync_status, "synced");
    }

    #[test]
    fn remove_repo_cascades_to_branches() {
        let (_tmp, db) = open_temp_db();
        db.upsert_repo("r1", "/r1", None).unwrap();

        let b = BranchState {
            branch_name: "main".to_string(),
            sync_status: "synced".to_string(),
            last_pull_at: None,
            last_push_at: None,
            local_oid: None,
            remote_oid: None,
            error_message: None,
            push_backoff_until: None,
        };
        db.upsert_branch("r1", &b).unwrap();
        assert_eq!(db.list_branches("r1").unwrap().len(), 1);

        db.remove_repo("r1").unwrap();
        assert!(db.get_repo("r1").unwrap().is_none());
        assert!(db.list_branches("r1").unwrap().is_empty());
    }

    #[test]
    fn notification_cooldown_logic() {
        let (_tmp, db) = open_temp_db();
        db.upsert_repo("r1", "/r1", None).unwrap();

        // No prior notification -> should notify
        let should = db
            .should_notify("r1", "diverged", Duration::from_secs(300))
            .unwrap();
        assert!(should, "should notify when no prior notification");

        // Record notification
        db.record_notification("r1", "diverged").unwrap();

        // Within cooldown -> should not notify
        let should = db
            .should_notify("r1", "diverged", Duration::from_secs(300))
            .unwrap();
        assert!(!should, "should not notify within cooldown");

        // With zero cooldown -> should notify immediately
        let should = db
            .should_notify("r1", "diverged", Duration::from_secs(0))
            .unwrap();
        assert!(should, "should notify with zero cooldown");
    }
}

// ===========================================================================
// 3. Transport Protocol Tests
// ===========================================================================

mod transport_tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn request_round_trip() {
        let (mut client, mut server) = duplex(4096);
        let req = Request::Register {
            repo_path: "/home/user/repo".into(),
        };
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
            Request::Register {
                repo_path: "/repo".into(),
            },
            Request::ConfigUpdate {
                repo_path: Some("/repo".into()),
            },
            Request::ConfigUpdate { repo_path: None },
            Request::Enable {
                repo_path: "/repo".into(),
            },
            Request::Disable {
                repo_path: "/repo".into(),
                purge: false,
            },
            Request::Disable {
                repo_path: "/repo".into(),
                purge: true,
            },
            Request::Log {
                repo_path: None,
                global: true,
                follow: false,
                since: None,
            },
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
                    mode: "pull".into(),
                    last_sync: None,
                    branches: vec![transport::BranchStatusData {
                        name: "main".into(),
                        upstream: Some("origin/main".into()),
                        status: "synced".into(),
                        last_action: None,
                    }],
                },
            },
            Response::GlobalStatus {
                repos: vec![transport::RepoStatusData {
                    display_path: "/path".into(),
                    mode: "pull".into(),
                    status_summary: "1 synced".into(),
                    last_sync: None,
                }],
            },
            Response::DaemonStatus {
                pid: 1234,
                uptime_secs: 3600,
                repos_watched: 5,
            },
            Response::LogEntry {
                entry: "some log".into(),
            },
            Response::LogEnd,
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
// 4. Git Operations Tests
// ===========================================================================

mod git_ops_tests {
    use super::*;
    use gitsitter::git_ops;

    #[test]
    fn discover_repo_id_finds_git_dir() {
        let tmp = temp_dir();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        // Make an initial commit so HEAD is valid
        make_commit(&repo, "README.md", "hello", "Initial commit");

        let repo_id = git_ops::discover_repo_id(tmp.path()).unwrap();
        // The repo_id should be the canonicalized .git directory
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
        // The display path should match the working directory
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
        // There should be at least one branch (the default one)
        let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
        // Default branch could be "main" or "master" depending on git config
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

        // Create an index.lock file to simulate an in-progress operation
        let lock_path = repo_id.join("index.lock");
        std::fs::write(&lock_path, "").unwrap();

        assert!(git_ops::is_operation_in_progress(&repo_id));

        // Clean up
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

        // Modify the tracked file
        std::fs::write(tmp.path().join("README.md"), "modified content").unwrap();

        let dirty = git_ops::is_worktree_dirty(tmp.path()).unwrap();
        assert!(dirty);
    }
}

// ===========================================================================
// 5. End-to-End Integration Tests
// ===========================================================================

// TODO: add a Windows E2E harness for named-pipe/service coverage.
#[cfg(unix)]
mod e2e_tests {
    use super::*;
    use serial_test::serial;
    use std::path::PathBuf;
    use tokio::net::UnixStream;
    use tokio::task::JoinHandle;

    // -----------------------------------------------------------------------
    // Shared harness
    // -----------------------------------------------------------------------

    struct E2eHarness {
        _tmp: TempDir,
        socket_path: PathBuf,
        repo_id_str: String,
        local_dir: PathBuf,
        bare_dir: PathBuf,
        branch_name: String,
        daemon_handle: JoinHandle<()>,
    }

    impl E2eHarness {
        /// Boot a daemon with a bare remote + local clone already registered.
        /// `repo_mode` controls the sync mode written to config (e.g. "push+pull").
        async fn start(repo_mode: &str) -> Self {
            let tmp = temp_dir();
            let base = tmp.path().to_path_buf();
            let (config_dir, _state_dir, socket_path) = setup_test_env(&base);

            // Create bare remote via a temp working copy
            let bare_dir = base.join("remote.git");
            let init_dir = base.join("init_tmp");
            let init_repo = git2::Repository::init(&init_dir).unwrap();
            make_commit(&init_repo, "README.md", "initial", "Initial commit");
            create_bare_repo(&bare_dir);

            let branch_name = init_repo
                .head()
                .unwrap()
                .shorthand()
                .unwrap()
                .to_string();

            let mut remote = init_repo
                .remote("origin", &format!("file://{}", bare_dir.display()))
                .unwrap();
            remote
                .push(
                    &[&format!(
                        "refs/heads/{}:refs/heads/{}",
                        branch_name, branch_name
                    )],
                    None,
                )
                .unwrap();
            drop(remote);
            drop(init_repo);

            // Clone
            let local_dir = base.join("local");
            let _local_repo = clone_repo(&bare_dir, &local_dir);

            let repo_id =
                gitsitter::git_ops::discover_repo_id(&local_dir).unwrap();
            let repo_id_str = repo_id.to_string_lossy().to_string();

            // Write config with explicit per-repo mode.
            let config_toml = format!(
                r#"
[global]
refresh_interval = "1s"

[repos."{}"]
mode = "{}"
"#,
                repo_id_str, repo_mode
            );
            std::fs::write(config_dir.join("config.toml"), &config_toml).unwrap();

            // Start daemon
            let daemon_handle = tokio::spawn(async move {
                let _ = gitsitter::daemon::run_daemon().await;
            });

            // Wait for socket
            let deadline =
                tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                if socket_path.exists() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    break;
                }
                if tokio::time::Instant::now() > deadline {
                    panic!("daemon socket did not appear within 5 seconds");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            // Register the repo
            let resp = Self::roundtrip_static(
                &socket_path,
                &Request::Register {
                    repo_path: repo_id_str.clone(),
                },
            )
            .await;
            match &resp {
                Response::Ok { .. } => {}
                other => panic!("register failed: {:?}", other),
            }

            Self {
                _tmp: tmp,
                socket_path,
                repo_id_str,
                local_dir,
                bare_dir,
                branch_name,
                daemon_handle,
            }
        }

        async fn roundtrip_static(
            socket: &std::path::Path,
            req: &Request,
        ) -> Response {
            let mut stream = UnixStream::connect(socket).await.unwrap();
            let (mut reader, mut writer) = stream.split();
            transport::send_request(&mut writer, req).await.unwrap();
            transport::recv_response(&mut reader).await.unwrap()
        }

        async fn roundtrip(&self, req: &Request) -> Response {
            Self::roundtrip_static(&self.socket_path, req).await
        }

        /// Trigger a sync and poll until the branch appears in the DB with
        /// one of the expected statuses, or until `timeout` elapses.
        async fn trigger_sync_and_wait_for(
            &self,
            expected_statuses: &[&str],
            timeout: Duration,
        ) {
            self.roundtrip(&Request::Sync {
                repo_path: Some(self.repo_id_str.clone()),
                all: false,
            })
            .await;

            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                tokio::time::sleep(Duration::from_millis(300)).await;
                if let Some(bs) = self.get_branch_status(&self.branch_name) {
                    if expected_statuses.contains(&bs.sync_status.as_str()) {
                        return;
                    }
                }
                if tokio::time::Instant::now() > deadline {
                    return; // let the caller's assert produce the error
                }
            }
        }

        /// Open a second clone of the same bare repo ("another user").
        fn open_second_clone(&self) -> (TempDir, git2::Repository) {
            let tmp = temp_dir();
            let repo = clone_repo(&self.bare_dir, tmp.path());
            (tmp, repo)
        }

        /// Get the branch status from the state DB.
        fn get_branch_status(&self, branch: &str) -> Option<BranchState> {
            let db = StateDb::open().unwrap();
            db.get_branch(&self.repo_id_str, branch).unwrap()
        }

        /// Open the local repo via git2.
        fn open_local_repo(&self) -> git2::Repository {
            git2::Repository::open(&self.local_dir).unwrap()
        }

        async fn shutdown(self) {
            let _ = self
                .roundtrip(&Request::Shutdown)
                .await;
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                self.daemon_handle,
            )
            .await;
            teardown_test_env();
        }
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    /// Basic lifecycle: register, status, daemon-status, config-update, shutdown.
    #[tokio::test]
    #[serial]
    async fn daemon_lifecycle() {
        let h = E2eHarness::start("push+pull").await;

        // Verify repo in state DB
        {
            let db = StateDb::open().unwrap();
            let repo = db.get_repo(&h.repo_id_str).unwrap();
            assert!(
                repo.is_some(),
                "repo should exist in state DB after registration"
            );
        }

        // Status
        let resp = h
            .roundtrip(&Request::Status {
                repo_path: Some(h.repo_id_str.clone()),
                global: false,
            })
            .await;
        match &resp {
            Response::Status { data } => {
                assert_eq!(data.repo_id, h.repo_id_str);
            }
            Response::Error { message } => {
                eprintln!("status error (may be expected): {}", message);
            }
            other => panic!("unexpected: {:?}", other),
        }

        // DaemonStatus
        let resp = h.roundtrip(&Request::DaemonStatus).await;
        match &resp {
            Response::DaemonStatus {
                pid,
                repos_watched,
                ..
            } => {
                assert!(*pid > 0);
                assert!(*repos_watched >= 1);
            }
            other => panic!("unexpected: {:?}", other),
        }

        // ConfigUpdate
        let resp = h
            .roundtrip(&Request::ConfigUpdate {
                repo_path: Some(h.repo_id_str.clone()),
            })
            .await;
        assert!(matches!(resp, Response::Ok { .. }));

        // Shutdown cleans up socket
        let socket = h.socket_path.clone();
        h.shutdown().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !socket.exists(),
            "socket should be removed after shutdown"
        );
    }

    /// A local commit is auto-pushed to the remote.
    #[tokio::test]
    #[serial]
    async fn local_commit_is_pushed() {
        let h = E2eHarness::start("push+pull").await;

        // Make a local commit
        let local_repo = h.open_local_repo();
        make_commit(&local_repo, "new.txt", "hello", "Add new file");
        let local_oid = local_repo
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        drop(local_repo);

        // Trigger sync and wait for push to complete
        h.trigger_sync_and_wait_for(&["synced"], Duration::from_secs(10)).await;

        // Verify branch is synced
        let bs = h.get_branch_status(&h.branch_name);
        assert!(bs.is_some(), "branch should exist in state DB");
        let bs = bs.unwrap();
        assert_eq!(
            bs.sync_status, "synced",
            "branch should be synced after push, got: {} (error: {:?})",
            bs.sync_status, bs.error_message
        );

        // Verify the remote has the commit
        let bare = git2::Repository::open_bare(&h.bare_dir).unwrap();
        let remote_ref = bare
            .find_reference(&format!("refs/heads/{}", h.branch_name))
            .unwrap();
        assert_eq!(
            remote_ref.target().unwrap().to_string(),
            local_oid,
            "remote should have the local commit"
        );

        h.shutdown().await;
    }

    /// A remote update is fast-forward merged into the local branch.
    #[tokio::test]
    #[serial]
    async fn remote_update_is_pulled() {
        let h = E2eHarness::start("push+pull").await;

        // Simulate another user pushing to the remote
        let (_tmp2, other_clone) = h.open_second_clone();
        make_commit(
            &other_clone,
            "remote_file.txt",
            "from remote",
            "Remote commit",
        );
        let expected_oid = other_clone
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        // Push to bare
        let mut remote = other_clone.find_remote("origin").unwrap();
        remote
            .push(
                &[&format!(
                    "refs/heads/{}:refs/heads/{}",
                    h.branch_name, h.branch_name
                )],
                None,
            )
            .unwrap();
        drop(remote);
        drop(other_clone);

        // Trigger sync — daemon should fetch + ff-merge
        h.trigger_sync_and_wait_for(&["synced"], Duration::from_secs(10)).await;

        let bs = h.get_branch_status(&h.branch_name);
        assert!(bs.is_some(), "branch should exist in state DB");
        let bs = bs.unwrap();
        assert_eq!(
            bs.sync_status, "synced",
            "branch should be synced after pull, got: {} (error: {:?})",
            bs.sync_status, bs.error_message
        );

        // Verify local repo has the remote commit
        let local_repo = h.open_local_repo();
        let local_oid = local_repo
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        assert_eq!(
            local_oid, expected_oid,
            "local should have been fast-forwarded to the remote commit"
        );

        h.shutdown().await;
    }

    /// Diverged branches: local and remote both have independent commits.
    #[tokio::test]
    #[serial]
    async fn diverged_branches_detected() {
        let h = E2eHarness::start("pull").await;

        // Make a local commit (won't be pushed because mode is "pull")
        let local_repo = h.open_local_repo();
        make_commit(
            &local_repo,
            "local_only.txt",
            "local",
            "Local-only commit",
        );
        drop(local_repo);

        // Push an independent commit to the remote via second clone
        let (_tmp2, other_clone) = h.open_second_clone();
        make_commit(
            &other_clone,
            "remote_only.txt",
            "remote",
            "Remote-only commit",
        );
        let mut remote = other_clone.find_remote("origin").unwrap();
        remote
            .push(
                &[&format!(
                    "refs/heads/{}:refs/heads/{}",
                    h.branch_name, h.branch_name
                )],
                None,
            )
            .unwrap();
        drop(remote);
        drop(other_clone);

        // Trigger sync — daemon should detect divergence
        h.trigger_sync_and_wait_for(&["diverged"], Duration::from_secs(10)).await;

        let bs = h.get_branch_status(&h.branch_name);
        assert!(bs.is_some(), "branch should exist in state DB");
        let bs = bs.unwrap();
        assert_eq!(
            bs.sync_status, "diverged",
            "branch should be diverged, got: {} (error: {:?})",
            bs.sync_status, bs.error_message
        );
        assert!(
            bs.error_message
                .as_deref()
                .unwrap_or("")
                .contains("diverged"),
            "error_message should mention divergence"
        );

        h.shutdown().await;
    }

    /// Push rejected by a server-side pre-receive hook.
    #[tokio::test]
    #[serial]
    async fn push_rejected_by_hook() {
        let h = E2eHarness::start("push+pull").await;

        // Install a pre-receive hook that rejects all pushes
        let hooks_dir = h.bare_dir.join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("pre-receive");
        std::fs::write(
            &hook_path,
            "#!/bin/sh\necho 'rejected by policy'\nexit 1\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                &hook_path,
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }

        // Make a local commit
        let local_repo = h.open_local_repo();
        make_commit(
            &local_repo,
            "blocked.txt",
            "should not arrive",
            "Blocked commit",
        );
        drop(local_repo);

        // Trigger sync — push should be rejected
        h.trigger_sync_and_wait_for(&["push_rejected"], Duration::from_secs(10)).await;

        let bs = h.get_branch_status(&h.branch_name);
        assert!(bs.is_some(), "branch should exist in state DB");
        let bs = bs.unwrap();
        assert_eq!(
            bs.sync_status, "push_rejected",
            "branch should be push_rejected, got: {} (error: {:?})",
            bs.sync_status, bs.error_message
        );

        h.shutdown().await;
    }

    /// Dirty worktree prevents fast-forward pull.
    #[tokio::test]
    #[serial]
    async fn dirty_worktree_blocks_ff() {
        let h = E2eHarness::start("push+pull").await;

        // Push a commit to the remote via second clone
        let (_tmp2, other_clone) = h.open_second_clone();
        make_commit(
            &other_clone,
            "remote_new.txt",
            "from remote",
            "Remote commit",
        );
        let mut remote = other_clone.find_remote("origin").unwrap();
        remote
            .push(
                &[&format!(
                    "refs/heads/{}:refs/heads/{}",
                    h.branch_name, h.branch_name
                )],
                None,
            )
            .unwrap();
        drop(remote);
        drop(other_clone);

        // Make the local worktree dirty (modify tracked file without committing)
        std::fs::write(
            h.local_dir.join("README.md"),
            "uncommitted changes",
        )
        .unwrap();

        // Trigger sync — should detect pending_ff_dirty
        h.trigger_sync_and_wait_for(&["pending_ff_dirty"], Duration::from_secs(10)).await;

        let bs = h.get_branch_status(&h.branch_name);
        assert!(bs.is_some(), "branch should exist in state DB");
        let bs = bs.unwrap();
        assert_eq!(
            bs.sync_status, "pending_ff_dirty",
            "branch should be pending_ff_dirty, got: {} (error: {:?})",
            bs.sync_status, bs.error_message
        );

        h.shutdown().await;
    }

    /// Operation in progress (index.lock) causes daemon to skip sync.
    #[tokio::test]
    #[serial]
    async fn operation_in_progress_skips_sync() {
        let h = E2eHarness::start("push+pull").await;

        // Make a local commit so there's something to push
        let local_repo = h.open_local_repo();
        make_commit(
            &local_repo,
            "will_push.txt",
            "data",
            "Commit to push",
        );
        drop(local_repo);

        // Place an index.lock to simulate an in-progress operation
        let lock_path = h.local_dir.join(".git/index.lock");
        std::fs::write(&lock_path, "").unwrap();

        // Trigger sync — should be skipped due to index.lock
        h.roundtrip(&Request::Sync {
            repo_path: Some(h.repo_id_str.clone()),
            all: false,
        })
        .await;
        // Wait a few sync cycles
        tokio::time::sleep(Duration::from_secs(4)).await;

        // Branch should NOT be recorded as synced (no sync happened)
        let bs = h.get_branch_status(&h.branch_name);
        // Either None (never recorded) or not "synced" — the daemon
        // skips entirely when index.lock exists.
        if let Some(bs) = &bs {
            assert_ne!(
                bs.sync_status, "synced",
                "branch should not be synced while index.lock exists"
            );
        }

        // Remove lock and sync again — should succeed now
        std::fs::remove_file(&lock_path).unwrap();
        h.trigger_sync_and_wait_for(&["synced"], Duration::from_secs(10)).await;

        let bs = h.get_branch_status(&h.branch_name);
        assert!(bs.is_some(), "branch should exist after lock removed");
        let bs = bs.unwrap();
        assert_eq!(
            bs.sync_status, "synced",
            "branch should be synced after lock removed, got: {} (error: {:?})",
            bs.sync_status, bs.error_message
        );

        h.shutdown().await;
    }

    /// Pull-only mode: local commits are NOT pushed (status should be "local_ahead").
    #[tokio::test]
    #[serial]
    async fn pull_only_local_ahead_not_pushed() {
        let h = E2eHarness::start("pull").await;

        // Make a local commit — should NOT be pushed in pull-only mode
        let local_repo = h.open_local_repo();
        make_commit(
            &local_repo,
            "local_only.txt",
            "content",
            "Local commit",
        );
        let local_oid = local_repo
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        drop(local_repo);

        h.trigger_sync_and_wait_for(&["local_ahead"], Duration::from_secs(10)).await;

        let bs = h.get_branch_status(&h.branch_name);
        assert!(bs.is_some(), "branch should exist in state DB");
        let bs = bs.unwrap();
        assert_eq!(
            bs.sync_status, "local_ahead",
            "pull-only mode should report local_ahead, got: {} (error: {:?})",
            bs.sync_status, bs.error_message
        );

        // Verify remote does NOT have the local commit
        let bare = git2::Repository::open_bare(&h.bare_dir).unwrap();
        let remote_oid = bare
            .find_reference(&format!("refs/heads/{}", h.branch_name))
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        assert_ne!(
            remote_oid, local_oid,
            "remote should NOT have the local commit in pull-only mode"
        );

        h.shutdown().await;
    }

    /// Push-only mode: local commits are pushed but remote-ahead is not pulled.
    #[tokio::test]
    #[serial]
    async fn push_only_mode() {
        let h = E2eHarness::start("push").await;

        // Make a local commit — should be pushed
        let local_repo = h.open_local_repo();
        make_commit(
            &local_repo,
            "pushed.txt",
            "content",
            "Local commit",
        );
        let local_oid = local_repo
            .head()
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        drop(local_repo);

        h.trigger_sync_and_wait_for(&["synced"], Duration::from_secs(10)).await;

        // Verify push happened
        let bare = git2::Repository::open_bare(&h.bare_dir).unwrap();
        let remote_oid = bare
            .find_reference(&format!("refs/heads/{}", h.branch_name))
            .unwrap()
            .target()
            .unwrap()
            .to_string();
        assert_eq!(
            remote_oid, local_oid,
            "remote should have the local commit after push"
        );

        h.shutdown().await;
    }

    /// Fetch-only mode: remote changes are fetched but local branch is NOT
    /// fast-forwarded (status should be "remote_ahead").
    #[tokio::test]
    #[serial]
    async fn fetch_only_mode_no_merge() {
        let h = E2eHarness::start("fetch").await;

        // Push a commit to the remote via second clone
        let (_tmp2, other_clone) = h.open_second_clone();
        make_commit(
            &other_clone,
            "fetched.txt",
            "content",
            "Remote-only",
        );
        let mut remote = other_clone.find_remote("origin").unwrap();
        remote
            .push(
                &[&format!(
                    "refs/heads/{}:refs/heads/{}",
                    h.branch_name, h.branch_name
                )],
                None,
            )
            .unwrap();
        drop(remote);
        drop(other_clone);

        h.trigger_sync_and_wait_for(
            &["remote_ahead"],
            Duration::from_secs(10),
        )
        .await;

        let bs = h.get_branch_status(&h.branch_name);
        assert!(bs.is_some(), "branch should exist in state DB");
        let bs = bs.unwrap();
        assert_eq!(
            bs.sync_status, "remote_ahead",
            "fetch-only mode should report remote_ahead, got: {} (error: {:?})",
            bs.sync_status, bs.error_message
        );

        // Verify local branch was NOT updated
        let file = h.local_dir.join("fetched.txt");
        assert!(
            !file.exists(),
            "fetched.txt should not exist in worktree in fetch-only mode"
        );

        h.shutdown().await;
    }

    /// Explicit sync request triggers immediate processing.
    #[tokio::test]
    #[serial]
    async fn explicit_sync_request() {
        let h = E2eHarness::start("push+pull").await;

        let local_repo = h.open_local_repo();
        make_commit(
            &local_repo,
            "sync_me.txt",
            "data",
            "Commit for explicit sync",
        );
        drop(local_repo);

        // Send explicit sync and wait for it to complete
        h.trigger_sync_and_wait_for(&["synced"], Duration::from_secs(10)).await;

        let bs = h.get_branch_status(&h.branch_name);
        assert!(bs.is_some());
        let bs = bs.unwrap();
        assert_eq!(
            bs.sync_status, "synced",
            "should be synced after explicit sync, got: {} (error: {:?})",
            bs.sync_status, bs.error_message
        );

        h.shutdown().await;
    }

    /// PromptCheck registers the repo and returns status.
    #[tokio::test]
    #[serial]
    async fn prompt_check_registers_and_returns_status() {
        let h = E2eHarness::start("push+pull").await;

        let resp = h
            .roundtrip(&Request::PromptCheck {
                repo_path: h.local_dir.to_string_lossy().to_string(),
            })
            .await;
        match &resp {
            Response::Status { data } => {
                assert_eq!(data.repo_id, h.repo_id_str);
            }
            Response::Error { message } => {
                panic!("prompt_check failed: {}", message);
            }
            other => panic!("unexpected: {:?}", other),
        }

        h.shutdown().await;
    }
}
