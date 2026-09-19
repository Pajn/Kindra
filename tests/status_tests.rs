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
