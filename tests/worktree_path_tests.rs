mod common;

use common::{
    canonical_output_path, kin_cmd, managed_worktree_path, repo_init, run_ok, write_repo_config,
};
use std::fs;
use tempfile::TempDir;

fn setup_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    let repo = repo_init(dir.path());
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Test User").unwrap();
    config.set_str("user.email", "test@example.com").unwrap();

    fs::write(dir.path().join("file.txt"), "main").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok("git", &["commit", "-m", "initial"], dir.path());

    run_ok("git", &["checkout", "-b", "feature-a"], dir.path());
    fs::write(dir.path().join("feature.txt"), "feature").unwrap();
    run_ok("git", &["add", "feature.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature"], dir.path());

    dir
}

#[test]
fn worktree_path_resolves_main_review_and_temp_targets() {
    let dir = setup_repo();
    let main_path = dir.path().join(".git/kindra-worktrees/main");
    let review_path = dir.path().join(".git/kindra-worktrees/review");
    let temp_path = dir.path().join(".git/kindra-worktrees/temp/feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "main"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        kin_cmd()
            .args(["wt", "review"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );

    let output = kin_cmd()
        .args(["wt", "path", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        canonical_output_path(&output.stdout, dir.path()),
        fs::canonicalize(&main_path).unwrap()
    );

    let output = kin_cmd()
        .args(["wt", "path", "review"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        canonical_output_path(&output.stdout, dir.path()),
        fs::canonicalize(&review_path).unwrap()
    );

    let output = kin_cmd()
        .args(["wt", "path", "feature-a"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        canonical_output_path(&output.stdout, dir.path()),
        fs::canonicalize(&temp_path).unwrap()
    );
}

#[test]
fn worktree_path_fails_for_unknown_target() {
    let dir = setup_repo();
    let output = kin_cmd()
        .args(["wt", "path", "missing-branch"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("No managed temp worktree exists for branch 'missing-branch'.")
    );
}

#[test]
fn worktree_path_and_remove_support_branch_disambiguation_for_main_and_review_names() {
    let dir = setup_repo();
    let review_path = managed_worktree_path(dir.path(), "review");
    let temp_main_path = managed_worktree_path(dir.path(), "temp/main");
    let temp_review_path = managed_worktree_path(dir.path(), "temp/review");

    run_ok("git", &["checkout", "main"], dir.path());
    run_ok("git", &["checkout", "-b", "review"], dir.path());
    fs::write(dir.path().join("review.txt"), "review").unwrap();
    run_ok("git", &["add", "review.txt"], dir.path());
    run_ok("git", &["commit", "-m", "review"], dir.path());
    run_ok("git", &["checkout", "main"], dir.path());

    for args in [
        vec!["wt", "main"],
        vec!["wt", "review", "feature-a"],
        vec!["wt", "temp", "main"],
        vec!["wt", "temp", "review"],
    ] {
        let output = kin_cmd()
            .args(args)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "worktree command failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let output = kin_cmd()
        .args(["wt", "path", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        canonical_output_path(&output.stdout, dir.path()),
        fs::canonicalize(managed_worktree_path(dir.path(), "main")).unwrap()
    );

    let output = kin_cmd()
        .args(["wt", "path", "branch:main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        canonical_output_path(&output.stdout, dir.path()),
        fs::canonicalize(&temp_main_path).unwrap()
    );

    let output = kin_cmd()
        .args(["wt", "path", "review"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        canonical_output_path(&output.stdout, dir.path()),
        fs::canonicalize(&review_path).unwrap()
    );

    let output = kin_cmd()
        .args(["wt", "path", "branch:review"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        canonical_output_path(&output.stdout, dir.path()),
        fs::canonicalize(&temp_review_path).unwrap()
    );

    let output = kin_cmd()
        .args(["wt", "remove", "branch:main", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!temp_main_path.exists());
    assert!(managed_worktree_path(dir.path(), "main").exists());
}

#[test]
fn worktree_commands_from_linked_worktree_use_shared_repo_config_root() {
    let dir = setup_repo();
    write_repo_config(
        dir.path(),
        "[worktrees]\nroot = \".git/shared-worktrees\"\n",
    );
    run_ok("git", &["branch", "linked-feature"], dir.path());
    let linked_path = dir.path().join("linked-worktree");
    run_ok(
        "git",
        &[
            "worktree",
            "add",
            linked_path.to_str().unwrap(),
            "linked-feature",
        ],
        dir.path(),
    );

    let output = kin_cmd()
        .args(["wt", "main"])
        .current_dir(&linked_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "kin wt main from linked worktree failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        canonical_output_path(&output.stdout, &linked_path),
        fs::canonicalize(dir.path().join(".git/shared-worktrees/main")).unwrap()
    );
}
