mod common;

use common::{canonical_output_path, current_branch, kin_cmd, repo_init, run_ok};
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
    run_ok("git", &["checkout", "main"], dir.path());

    dir
}

#[test]
fn worktree_main_creates_and_reuses_pinned_path() {
    let dir = setup_repo();
    let expected_path = dir.path().join(".git/kindra-worktrees/main");

    let output = kin_cmd()
        .args(["wt", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        canonical_output_path(&output.stdout, dir.path()),
        fs::canonicalize(&expected_path).unwrap()
    );
    assert!(expected_path.exists());
    assert_eq!(current_branch(&expected_path), "main");

    let output = kin_cmd()
        .args(["wt", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        canonical_output_path(&output.stdout, dir.path()),
        fs::canonicalize(&expected_path).unwrap()
    );
}

#[test]
fn worktree_main_errors_when_pinned_path_is_on_wrong_branch() {
    let dir = setup_repo();
    let main_path = dir.path().join(".git/kindra-worktrees/main");

    let output = kin_cmd()
        .args(["wt", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());

    run_ok(
        "git",
        &[
            "-C",
            main_path.to_str().unwrap(),
            "checkout",
            "--ignore-other-worktrees",
            "feature-a",
        ],
        dir.path(),
    );

    let output = kin_cmd()
        .args(["wt", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("pinned"));
}
