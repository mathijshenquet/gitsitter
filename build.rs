fn main() {
    // Expose the current git commit hash via env var (used by `--version`).
    // Prefer a hash injected by the build environment (the nix flake passes
    // self.shortRev, since the build sandbox has no .git); otherwise ask git.
    let hash = std::env::var("GIT_COMMIT_HASH")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        String::from_utf8(o.stdout)
                            .ok()
                            .map(|s| s.trim().to_string())
                    } else {
                        None
                    }
                })
        })
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rerun-if-env-changed=GIT_COMMIT_HASH");
    println!("cargo:rustc-env=GIT_COMMIT_HASH={hash}");

    // Rebuild when the commit changes. HEAD only changes on branch switches, so
    // also watch the ref it points at (where new commits on a branch land) and
    // packed-refs (where refs live after `git pack-refs`).
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/packed-refs");
    if let Ok(head) = std::fs::read_to_string(".git/HEAD")
        && let Some(ref_path) = head.strip_prefix("ref: ")
    {
        println!("cargo:rerun-if-changed=.git/{}", ref_path.trim());
    }
}
