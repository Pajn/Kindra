mod common;

use common::{current_branch, kin_cmd, repo_init, run_ok};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn setup_run_repo() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let _repo = repo_init(dir.path());

    fs::write(dir.path().join("base.txt"), "base").unwrap();
    run_ok("git", &["add", "base.txt"], dir.path());
    run_ok("git", &["commit", "-m", "base"], dir.path());

    run_ok("git", &["checkout", "-b", "feature-a"], dir.path());
    fs::write(dir.path().join("a.txt"), "a").unwrap();
    run_ok("git", &["add", "a.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature-a"], dir.path());

    run_ok("git", &["checkout", "-b", "feature-b"], dir.path());
    fs::write(dir.path().join("b.txt"), "b").unwrap();
    run_ok("git", &["add", "b.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature-b"], dir.path());

    dir
}

fn read_lines(path: &Path) -> Vec<String> {
    if !path.exists() {
        return Vec::new();
    }
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| line.to_string())
        .collect()
}

fn git_stdout(cwd: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout:\n{}\nstderr:\n{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn run_happy_path_traverses_stack() {
    let dir = setup_run_repo();
    let log_path = dir.path().join("run.log");

    let mut cmd = kin_cmd();
    cmd.arg("run")
        .arg("--command")
        .arg("echo \"$(git branch --show-current)\" >> run.log")
        .current_dir(dir.path())
        .assert()
        .success();

    assert_eq!(read_lines(&log_path), vec!["feature-a", "feature-b"]);
    assert_eq!(current_branch(dir.path()), "feature-b");
    assert!(!dir.path().join(".git/kindra_run_state.json").exists());
}

#[test]
fn run_continue_on_failure_processes_later_branches() {
    let dir = setup_run_repo();
    let log_path = dir.path().join("run.log");

    let mut cmd = kin_cmd();
    cmd.arg("run")
        .arg("--command")
        .arg(
            "branch=$(git branch --show-current); echo \"$branch\" >> run.log; if [ \"$branch\" = \"feature-a\" ]; then exit 1; fi",
        )
        .arg("--continue-on-failure")
        .current_dir(dir.path())
        .assert()
        .failure();

    assert_eq!(read_lines(&log_path), vec!["feature-a", "feature-b"]);
    assert_eq!(current_branch(dir.path()), "feature-b");
    assert!(dir.path().join(".git/kindra_run_state.json").exists());
}

#[test]
fn run_failure_restores_original_checkout() {
    let dir = setup_run_repo();
    let log_path = dir.path().join("run.log");

    let mut cmd = kin_cmd();
    cmd.arg("run")
        .arg("--command")
        .arg("echo \"$(git branch --show-current)\" >> run.log; exit 1")
        .current_dir(dir.path())
        .assert()
        .failure();

    assert_eq!(read_lines(&log_path), vec!["feature-a"]);
    assert_eq!(current_branch(dir.path()), "feature-b");
    assert!(dir.path().join(".git/kindra_run_state.json").exists());
}

#[test]
fn run_failure_restores_original_detached_head() {
    let dir = setup_run_repo();
    run_ok("git", &["checkout", "--detach", "feature-b"], dir.path());
    let original_head = git_stdout(dir.path(), &["rev-parse", "HEAD"]);

    let mut cmd = kin_cmd();
    cmd.arg("run")
        .arg("--command")
        .arg("exit 1")
        .current_dir(dir.path())
        .assert()
        .failure();

    let current_head = git_stdout(dir.path(), &["rev-parse", "HEAD"]);
    assert_eq!(current_head, original_head);
    let symbolic_head = git_stdout(dir.path(), &["branch", "--show-current"]);
    assert!(symbolic_head.is_empty(), "HEAD should stay detached");
}

#[test]
fn run_status_after_failed_run() {
    let dir = setup_run_repo();
    let state_path = dir.path().join(".git/kindra_run_state.json");

    let mut run_cmd = kin_cmd();
    run_cmd
        .arg("run")
        .arg("--command")
        .arg("exit 1")
        .current_dir(dir.path())
        .assert()
        .failure();
    assert!(
        state_path.exists(),
        "run state should exist after failed run"
    );

    let mut status_cmd = kin_cmd();
    let status_assert = status_cmd
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .success();
    let status_stdout = String::from_utf8_lossy(&status_assert.get_output().stdout);
    assert!(
        status_stdout.contains("Run failed:"),
        "status should report failed run state\nstdout:\n{}",
        status_stdout
    );
    assert!(
        status_stdout.contains("Next branch: feature-a"),
        "status should include next branch from stored run state\nstdout:\n{}",
        status_stdout
    );
    assert!(
        status_stdout.contains("Failed branches: feature-a"),
        "status should include failed branch list\nstdout:\n{}",
        status_stdout
    );
}

#[test]
fn run_abort_restores_state() {
    let dir = setup_run_repo();
    let state_path = dir.path().join(".git/kindra_run_state.json");

    let mut run_cmd = kin_cmd();
    run_cmd
        .arg("run")
        .arg("--command")
        .arg("exit 1")
        .current_dir(dir.path())
        .assert()
        .failure();
    assert!(
        state_path.exists(),
        "run state should exist after failed run"
    );

    // Move away from the original checkout so abort must restore it from state.
    run_ok("git", &["checkout", "feature-a"], dir.path());
    assert_eq!(current_branch(dir.path()), "feature-a");

    let mut abort_cmd = kin_cmd();
    abort_cmd
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();

    assert!(
        !state_path.exists(),
        "run state should be removed after successful abort"
    );
    assert_eq!(
        current_branch(dir.path()),
        "feature-b",
        "abort should restore original branch checkout"
    );
}

#[test]
fn run_continue_resumes_run() {
    let dir = setup_run_repo();
    let log_path = dir.path().join("run.log");
    let state_path = dir.path().join(".git/kindra_run_state.json");

    // First run fails on feature-a once, continues to feature-b, and leaves failed state.
    let mut run_cmd = kin_cmd();
    run_cmd
        .arg("run")
        .arg("--command")
        .arg(
            "branch=$(git branch --show-current); echo \"$branch\" >> run.log; if [ \"$branch\" = \"feature-a\" ] && [ ! -f .retry-ok ]; then touch .retry-ok; exit 1; fi",
        )
        .arg("--continue-on-failure")
        .current_dir(dir.path())
        .assert()
        .failure();

    assert!(
        state_path.exists(),
        "run state should be persisted on failure"
    );
    assert_eq!(read_lines(&log_path), vec!["feature-a", "feature-b"]);

    let mut continue_cmd = kin_cmd();
    continue_cmd
        .arg("continue")
        .current_dir(dir.path())
        .assert()
        .success();

    // Continue should retry remaining work, clear run state, and restore original checkout.
    assert!(
        !state_path.exists(),
        "run state should be removed after successful continue"
    );
    assert_eq!(
        read_lines(&log_path),
        vec!["feature-a", "feature-b", "feature-a", "feature-b"]
    );
    assert_eq!(current_branch(dir.path()), "feature-b");
}
