mod common;

use common::{current_branch, kin_cmd, run_ok, setup_repo, write_repo_config};
use std::fs;
use tempfile::TempDir;

#[test]
fn worktree_review_creates_and_reuses_fixed_path() {
    let dir = setup_repo();
    let review_path = dir.path().join(".git/kindra-worktrees/review");

    let output = kin_cmd()
        .args(["wt", "review"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(current_branch(&review_path), "feature-b");

    let output = kin_cmd()
        .args(["wt", "review", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(current_branch(&review_path), "main");
}

#[test]
fn worktree_review_respects_dirty_state_unless_forced() {
    let dir = setup_repo();
    let review_path = dir.path().join(".git/kindra-worktrees/review");

    let output = kin_cmd()
        .args(["wt", "review", "feature-a"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());

    fs::write(review_path.join("feature.txt"), "dirty change").unwrap();

    let output = kin_cmd()
        .args(["wt", "review", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("auto-denying"));
    assert_eq!(current_branch(&review_path), "feature-a");

    let output = kin_cmd()
        .args(["wt", "review", "--force", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(current_branch(&review_path), "main");
}

/// Sets up a repo with a remote bare repo and pushes a branch to it.
/// Returns the local repo dir and the remote-only branch name.
fn setup_repo_with_remote_branch() -> (TempDir, String) {
    let local_dir = tempfile::TempDir::new().unwrap();
    let remote_dir = tempfile::TempDir::new().unwrap();

    // Init local repo
    run_ok("git", &["init", "--initial-branch=main"], local_dir.path());

    run_ok(
        "git",
        &["config", "user.name", "Test User"],
        local_dir.path(),
    );
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        local_dir.path(),
    );

    fs::write(local_dir.path().join("file.txt"), "main content").unwrap();
    run_ok("git", &["add", "file.txt"], local_dir.path());
    run_ok("git", &["commit", "-m", "initial"], local_dir.path());

    // Create bare remote
    run_ok(
        "git",
        &[
            "clone",
            "--bare",
            local_dir.path().to_str().unwrap(),
            remote_dir.path().to_str().unwrap(),
        ],
        local_dir.path(),
    );

    // Add remote to local repo
    run_ok(
        "git",
        &[
            "remote",
            "add",
            "origin",
            remote_dir.path().to_str().unwrap(),
        ],
        local_dir.path(),
    );

    // Push a feature branch to remote
    run_ok(
        "git",
        &["checkout", "-b", "feature-remote"],
        local_dir.path(),
    );
    fs::write(local_dir.path().join("feature.txt"), "feature content").unwrap();
    run_ok("git", &["add", "feature.txt"], local_dir.path());
    run_ok("git", &["commit", "-m", "feature commit"], local_dir.path());
    run_ok(
        "git",
        &["push", "origin", "feature-remote"],
        local_dir.path(),
    );

    // Delete local branch to make it remote-only
    run_ok("git", &["checkout", "main"], local_dir.path());
    run_ok("git", &["branch", "-D", "feature-remote"], local_dir.path());

    (local_dir, "feature-remote".to_string())
}

#[test]
fn worktree_review_auto_creates_local_branch_from_remote() {
    let (dir, branch) = setup_repo_with_remote_branch();
    let review_path = dir.path().join(".git/kindra-worktrees/review");

    write_repo_config(dir.path(), "");

    let root_branch = current_branch(dir.path());

    // Run wt review on the remote-only branch - should auto-create local branch
    let output = kin_cmd()
        .args(["wt", "review", &branch])
        .current_dir(dir.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "wt review failed on remote branch.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // Verify worktree was created and is on the branch
    assert_eq!(current_branch(&review_path), branch);

    // Verify main worktree branch was not switched
    assert_eq!(current_branch(dir.path()), root_branch);

    // Verify local branch now exists and tracks origin/feature-remote
    let output = std::process::Command::new("git")
        .args([
            "rev-parse",
            "--abbrev-ref",
            &format!("{}@{{upstream}}", branch),
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Local branch should track a remote"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("origin/{}", branch)
    );
}
