mod common;

use common::{
    canonical_output_path, kin_cmd, read_worktree_metadata, repo_init, run_ok, write_repo_config,
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

    run_ok("git", &["checkout", "-b", "feature/auth"], dir.path());
    fs::write(dir.path().join("feature.txt"), "feature-auth").unwrap();
    run_ok("git", &["add", "feature.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature-auth"], dir.path());

    run_ok("git", &["checkout", "main"], dir.path());
    run_ok("git", &["checkout", "-b", "feature-auth"], dir.path());
    fs::write(dir.path().join("dash.txt"), "feature-auth").unwrap();
    run_ok("git", &["add", "dash.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature-auth-dash"], dir.path());
    run_ok("git", &["checkout", "feature/auth"], dir.path());

    dir
}

#[test]
fn worktree_temp_sanitizes_branch_names_and_reuses_existing_worktree() {
    let dir = setup_repo();
    let expected_path = dir.path().join(".git/kindra-worktrees/temp/feature-auth");

    let output = kin_cmd()
        .args(["wt", "temp"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        canonical_output_path(&output.stdout, dir.path()),
        fs::canonicalize(&expected_path).unwrap()
    );
    assert!(expected_path.exists());

    let output = kin_cmd()
        .args(["wt", "temp"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        canonical_output_path(&output.stdout, dir.path()),
        fs::canonicalize(&expected_path).unwrap()
    );

    let metadata = read_worktree_metadata(dir.path());
    assert!(
        metadata["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["role"] == "temp" && record["branch"] == "feature/auth")
    );
}

#[test]
fn worktree_temp_detects_sanitized_path_collisions() {
    let dir = setup_repo();

    let output = kin_cmd()
        .args(["wt", "temp", "feature/auth"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());

    let output = kin_cmd()
        .args(["wt", "temp", "feature-auth"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("already reserved for temp 'feature/auth'")
    );
}

#[test]
fn worktree_temp_rejects_paths_reserved_for_managed_non_temp_roles() {
    let dir = setup_repo();
    write_repo_config(
        dir.path(),
        "[worktrees.temp]\npath_template = \".git/kindra-worktrees/{branch}\"\n",
    );

    let output = kin_cmd()
        .args(["wt", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "kin wt main failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let output = kin_cmd()
        .args(["wt", "temp", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already reserved for main 'main'"));
}
