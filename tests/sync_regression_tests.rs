mod common;

use common::{gits_cmd, make_commit, run_ok};
use git2::{BranchType, Repository};
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

#[test]
fn sync_aborts_deletions_if_fallback_checkout_is_blocked() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&base],
    );
    let _a = repo.find_commit(a_id).unwrap();

    // Merge feature-a into main
    run_ok("git", &["checkout", "-f", "main"], dir.path());
    run_ok(
        "git",
        &["merge", "--ff-only", &a_id.to_string()],
        dir.path(),
    );

    // Go back to feature-a
    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    // Create a commit on main that adds a file
    run_ok("git", &["checkout", "-f", "main"], dir.path());
    fs::write(dir.path().join("main_only.txt"), "main content").unwrap();
    run_ok("git", &["add", "main_only.txt"], dir.path());
    run_ok("git", &["commit", "-m", "add main_only"], dir.path());

    // Go back to feature-a
    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    // Create a dirty worktree by adding an untracked file that conflicts with the one in 'main'
    fs::write(dir.path().join("main_only.txt"), "dirty content").unwrap();

    // Now 'gits sync' will try to delete 'feature-a', which is the current branch.
    // It should first try to checkout 'main'.
    // 'git checkout main' should fail because main_only.txt would be overwritten.

    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("fallback git checkout failed"));

    // Verify feature-a was NOT deleted.
    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo.find_branch("feature-a", BranchType::Local).is_ok());
}

#[test]
fn sync_no_delete_with_open_worktree_does_not_fail() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&base],
    );
    let a = repo.find_commit(a_id).unwrap();

    let _b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feature b",
        &[&a],
    );

    // Merge feature-a into main
    run_ok("git", &["checkout", "-f", "main"], dir.path());
    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());

    // Create a worktree for feature-a
    let wt_dir = tempdir().unwrap();
    run_ok(
        "git",
        &[
            "worktree",
            "add",
            wt_dir.path().to_str().unwrap(),
            "feature-a",
        ],
        dir.path(),
    );

    // Checkout feature-b in the main worktree
    run_ok("git", &["checkout", "-f", "feature-b"], dir.path());

    // 'gits sync --no-delete' should NOT fail even though feature-a is checked out elsewhere,
    // because we are not going to delete it.
    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .arg("--no-delete")
        .current_dir(dir.path())
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo.find_branch("feature-a", BranchType::Local).is_ok());
}

#[test]
fn sync_onto_remote_tracking_ref_does_not_delete_local_base() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let remote_dir = dir.path().join("remote.git");
    fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    run_ok("git", &["push", "-u", "origin", "main:main"], dir.path());

    let _feature_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&repo.find_commit(base_id).unwrap()],
    );

    // Advance main on remote
    let remote_worktree = tempdir().unwrap();
    run_ok(
        "git",
        &[
            "clone",
            remote_dir.to_str().unwrap(),
            remote_worktree.path().to_str().unwrap(),
        ],
        dir.path(),
    );
    run_ok("git", &["checkout", "main"], remote_worktree.path());
    fs::write(remote_worktree.path().join("remote.txt"), "remote").unwrap();
    run_ok("git", &["add", "remote.txt"], remote_worktree.path());
    run_ok(
        "git",
        &["commit", "-m", "remote advanced"],
        remote_worktree.path(),
    );
    run_ok("git", &["push", "origin", "main"], remote_worktree.path());

    // Local 'main' is still at base_id. 'origin/main' is ahead.
    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    // 'gits sync' should rebase feature-a onto origin/main.
    // It should NOT delete local 'main' branch, even though 'main' is an ancestor of 'origin/main'.
    let mut cmd = gits_cmd();
    cmd.arg("sync").current_dir(dir.path()).assert().success();

    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo.find_branch("main", BranchType::Local).is_ok());
    assert!(repo.find_branch("feature-a", BranchType::Local).is_ok());

    let feature_a_tip = repo.revparse_single("feature-a").unwrap().id();
    let origin_main_tip = repo.revparse_single("origin/main").unwrap().id();
    assert!(
        repo.graph_descendant_of(feature_a_tip, origin_main_tip)
            .unwrap()
    );
}
