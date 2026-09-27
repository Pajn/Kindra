mod common;

use common::{kin_cmd, repo_init};
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

#[test]
fn status_reports_overlapping_operations_before_reading_or_reconciling_them() {
    let states = [
        "kindra_checkout_state.json",
        "kindra_rebase_state.json",
        "kindra_run_state.json",
    ];
    for selected in [
        [true, true, false],
        [true, false, true],
        [true, true, true],
        [false, true, true],
    ] {
        let dir = tempdir().unwrap();
        let repo = repo_init(dir.path());
        for (name, present) in states.iter().zip(selected) {
            if present {
                // Overlap must be reported even if a saved state is malformed.
                fs::write(repo.path().join(name), "{}").unwrap();
            }
        }
        kin_cmd()
            .arg("status")
            .current_dir(dir.path())
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "Multiple Kindra operations are persisted",
            ))
            .stdout(predicate::str::contains("Checkout hydration in progress").not())
            .stdout(predicate::str::contains("Run 'kin continue' to resume").not());
        for (name, present) in states.iter().zip(selected) {
            if present {
                assert_eq!(fs::read_to_string(repo.path().join(name)).unwrap(), "{}");
            }
        }
    }
}

#[test]
fn status_preserves_single_operation_messages() {
    let cases = [
        (
            "kindra_checkout_state.json",
            "{}",
            "Checkout hydration in progress. Run 'kin continue' to resume or 'kin abort' to stop.",
        ),
        (
            "kindra_run_state.json",
            r#"{
            "target_branches":["feature"], "current_index":0,
            "args":{"command":"true", "continue_on_failure":false},
            "original_branch":"main", "original_head_id":"", "status":"in_progress"
        }"#,
            "Run in progress: 0 of 1 branch(es) processed\nNext branch: feature",
        ),
        (
            "kindra_rebase_state.json",
            r#"{
            "operation":"Reorder", "original_branch":"feature", "target_branch":"main",
            "remaining_branches":[], "in_progress_branch":"feature"
        }"#,
            "Reorder in progress from feature\nRemaining branches:",
        ),
    ];
    for (file, state, expected) in cases {
        let dir = tempdir().unwrap();
        let repo = repo_init(dir.path());
        fs::write(repo.path().join(file), state).unwrap();
        kin_cmd()
            .arg("status")
            .current_dir(dir.path())
            .assert()
            .success()
            .stdout(predicate::str::contains(expected))
            .stdout(predicate::str::contains("Multiple Kindra operations").not());
    }
}

#[test]
fn overlapping_operations_point_status_continue_and_abort_at_clear_state() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());
    common::make_commit(&repo, "refs/heads/main", "file.txt", "base", "base", &[]);
    for name in ["kindra_rebase_state.json", "kindra_run_state.json"] {
        fs::write(repo.path().join(name), "{}").unwrap();
    }
    for command in ["status", "continue", "abort"] {
        let output = kin_cmd()
            .arg(command)
            .current_dir(dir.path())
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            text.contains("Multiple Kindra operations are persisted"),
            "{command}: {text}"
        );
        assert!(
            text.contains("kin abort --clear-state"),
            "{command}: {text}"
        );
        assert_eq!(output.status.success(), command == "status", "{command}");
    }
    kin_cmd()
        .args(["abort", "--clear-state"])
        .current_dir(dir.path())
        .assert()
        .success();
    common::assert_no_kindra_operation(dir.path());
}

/// A move whose conflicting rebase the user finished with `git rebase
/// --continue`, so only reconciliation stands between it and "done".
fn move_completed_with_git(dir: &std::path::Path) -> git2::Repository {
    let repo = repo_init(dir);
    let base_id = common::make_commit(&repo, "refs/heads/main", "file.txt", "base", "base", &[]);
    let base = repo.find_commit(base_id).unwrap();
    common::make_commit(
        &repo,
        "refs/heads/target",
        "file.txt",
        "target",
        "target",
        &[&base],
    );
    common::make_commit(
        &repo,
        "refs/heads/feature",
        "file.txt",
        "feature",
        "feature",
        &[&base],
    );
    common::run_ok("git", &["checkout", "-q", "-f", "feature"], dir);
    kin_cmd()
        .args(["move", "--onto", "target"])
        .current_dir(dir)
        .assert()
        .failure()
        .stderr(predicate::str::contains("Resolve conflicts"));
    fs::write(dir.join("file.txt"), "resolved").unwrap();
    common::run_ok("git", &["add", "file.txt"], dir);
    common::run_ok(
        "git",
        &["-c", "core.editor=true", "rebase", "--continue"],
        dir,
    );
    git2::Repository::open(dir).unwrap()
}

#[test]
fn status_reports_busy_and_reconciles_nothing_while_another_kin_holds_the_lock() {
    use fs2::FileExt;
    let dir = tempdir().unwrap();
    let repo = move_completed_with_git(dir.path());
    let state_path = repo.path().join("kindra_rebase_state.json");
    let saved = fs::read(&state_path).unwrap();

    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(repo.path().join("kindra.lock"))
        .unwrap();
    lock.try_lock_exclusive().unwrap();

    let output = kin_cmd()
        .arg("status")
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(
        stdout.contains("Another 'kin' process is running"),
        "stdout: {stdout}"
    );
    // Without the lock, status reports the saved state as it is on disk.
    assert!(stdout.contains("Move in progress"), "stdout: {stdout}");
    assert_eq!(fs::read(&state_path).unwrap(), saved);

    // Once the lock is free, status reconciles and sees the move is done.
    FileExt::unlock(&lock).unwrap();
    kin_cmd()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("No Kindra operation active."))
        .stdout(predicate::str::contains("Another 'kin' process").not());
    assert!(!state_path.exists());
}

#[test]
fn status_names_a_native_git_operation() {
    let dir = common::setup_repo();
    common::stop_native_operation(dir.path(), common::NativeStop::Revert, false);
    kin_cmd()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("No Kindra operation active."))
        .stdout(predicate::str::contains("git revert --continue"));
}
