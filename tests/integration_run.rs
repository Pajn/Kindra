mod common;

use common::{current_branch, kin_cmd, repo_init, run_ok};
use predicates::str::contains;
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
    // run is a reporter: a failed command reports via the exit code and leaves
    // no blocking state behind.
    assert!(!dir.path().join(".git/kindra_run_state.json").exists());
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
    assert!(!dir.path().join(".git/kindra_run_state.json").exists());
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
fn run_failed_command_is_terminal_and_does_not_block() {
    let dir = setup_run_repo();
    let state_path = dir.path().join(".git/kindra_run_state.json");

    // A failing command reports via a non-zero exit code...
    kin_cmd()
        .arg("run")
        .arg("--command")
        .arg("exit 1")
        .current_dir(dir.path())
        .assert()
        .failure();

    // ...but leaves no blocking run state behind.
    assert!(
        !state_path.exists(),
        "a failed run must not leave blocking state"
    );

    // `kin status` confirms nothing is in progress.
    kin_cmd()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(contains("No Kindra operation active."));

    // And a subsequent Kindra operation is not blocked by the earlier failure.
    kin_cmd()
        .arg("run")
        .arg("--command")
        .arg("true")
        .current_dir(dir.path())
        .assert()
        .success();
}

#[test]
fn run_refuses_dirty_working_tree_by_default() {
    let dir = setup_run_repo();
    let log_path = dir.path().join("run.log");

    // Pin the config so the default resolves to "off" regardless of the host's
    // global rebase.autostash setting.
    run_ok("git", &["config", "rebase.autostash", "false"], dir.path());

    // Modify a tracked file so the working tree is dirty.
    fs::write(dir.path().join("base.txt"), "dirty").unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("run")
        .arg("--command")
        .arg("echo ran >> run.log")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(contains("uncommitted changes"));

    // The command never ran, the dirty change is untouched, and no state leaks.
    assert!(!log_path.exists(), "command must not run on a dirty tree");
    assert_eq!(
        fs::read_to_string(dir.path().join("base.txt")).unwrap(),
        "dirty"
    );
    assert_eq!(current_branch(dir.path()), "feature-b");
    assert!(!dir.path().join(".git/kindra_run_state.json").exists());
}

#[test]
fn continue_rejects_leftover_run_state() {
    let dir = setup_run_repo();

    // A leftover run-state file means a `kin run` was interrupted before it could
    // restore the working tree. `kin run` is not resumable, so `kin continue`
    // cannot resolve it and must say so, pointing the user at `kin abort`.
    fs::write(dir.path().join(".git/kindra_run_state.json"), "{}").unwrap();

    kin_cmd()
        .arg("continue")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(contains("interrupted before it could restore"));
}

#[test]
fn run_autostash_sets_aside_changes_and_restores_them() {
    let dir = setup_run_repo();
    let log_path = dir.path().join("run.log");
    let status_path = dir.path().join("status.log");

    fs::write(dir.path().join("base.txt"), "dirty").unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("run")
        .arg("--command")
        // Record tracked-file status on each branch (ignoring the untracked log
        // files this command itself creates) and mark that we ran.
        .arg("git status --porcelain -uno >> status.log; echo ran >> run.log")
        .arg("--autostash")
        .current_dir(dir.path())
        .assert()
        .success();

    // Ran on both branches...
    assert_eq!(read_lines(&log_path), vec!["ran", "ran"]);
    // ...each time with a clean working tree (autostash set the change aside).
    assert!(
        read_lines(&status_path).is_empty(),
        "working tree should be clean during the run, got: {:?}",
        read_lines(&status_path)
    );
    // The uncommitted change is restored afterward on the original branch.
    assert_eq!(current_branch(dir.path()), "feature-b");
    assert_eq!(
        fs::read_to_string(dir.path().join("base.txt")).unwrap(),
        "dirty"
    );
    assert!(!dir.path().join(".git/kindra_run_state.json").exists());
}

#[test]
fn run_autostash_restored_on_failure() {
    let dir = setup_run_repo();
    let state_path = dir.path().join(".git/kindra_run_state.json");

    // Dirty a tracked file, then run a command that fails on the first branch.
    fs::write(dir.path().join("base.txt"), "dirty").unwrap();

    kin_cmd()
        .arg("run")
        .arg("--command")
        .arg("exit 1")
        .arg("--autostash")
        .current_dir(dir.path())
        .assert()
        .failure();

    // Even though the command failed, the autostashed change is restored on the
    // original branch and no blocking state is left behind — so the user's
    // uncommitted work is never stranded in a stash.
    assert_eq!(current_branch(dir.path()), "feature-b");
    assert_eq!(
        fs::read_to_string(dir.path().join("base.txt")).unwrap(),
        "dirty"
    );
    assert!(!state_path.exists());
}

#[test]
fn run_autostash_restored_after_continue_on_failure_run() {
    // The `--continue-on-failure` path runs every branch and then reports the
    // failures. The autostash must still be restored (and no state left) — the
    // exact case that used to strand the user's work pending a `kin continue`
    // that could never restore it.
    let dir = setup_run_repo();
    let state_path = dir.path().join(".git/kindra_run_state.json");

    fs::write(dir.path().join("base.txt"), "dirty").unwrap();

    kin_cmd()
        .arg("run")
        .arg("--command")
        .arg("exit 1")
        .arg("--autostash")
        .arg("--continue-on-failure")
        .current_dir(dir.path())
        .assert()
        .failure();

    assert_eq!(current_branch(dir.path()), "feature-b");
    assert_eq!(
        fs::read_to_string(dir.path().join("base.txt")).unwrap(),
        "dirty",
        "autostashed work must be restored even when every branch failed"
    );
    assert!(
        !state_path.exists(),
        "a completed continue-on-failure run must leave no blocking state"
    );
}
