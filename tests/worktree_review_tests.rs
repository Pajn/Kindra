mod common;

use common::{current_branch, kin_cmd, setup_repo};
use std::fs;

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
