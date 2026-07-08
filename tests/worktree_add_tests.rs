mod common;

use common::{current_branch, kin_cmd, run_ok, setup_worktree_repo, write_repo_config};
use git2::{BranchType, Repository};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn wt_add(dir: &Path, args: &[&str]) -> std::process::Output {
    kin_cmd()
        .args(["wt", "add"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn list(dir: &Path) -> String {
    let output = kin_cmd()
        .args(["wt", "list"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[test]
fn add_creates_durable_plain_worktree_for_existing_branch() {
    let dir = setup_worktree_repo();
    let wt_home = TempDir::new().unwrap();
    let wt_path = wt_home.path().join("feature-a");

    let output = wt_add(
        dir.path(),
        &["feature-a", "--path", wt_path.to_str().unwrap()],
    );
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(wt_path.exists());
    assert_eq!(current_branch(&wt_path), "feature-a");

    // It lists as a plain (`-`) worktree — no role policy attached.
    let listed = list(dir.path());
    assert!(
        listed
            .lines()
            .any(|line| line.starts_with('-') && line.contains("feature-a")),
        "expected a plain row for feature-a:\n{listed}"
    );
}

#[test]
fn add_b_creates_branch_and_worktree() {
    let dir = setup_worktree_repo();
    let wt_home = TempDir::new().unwrap();
    let wt_path = wt_home.path().join("spike");

    let output = wt_add(
        dir.path(),
        &["-b", "feature/spike", "--path", wt_path.to_str().unwrap()],
    );
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(wt_path.exists());
    assert_eq!(current_branch(&wt_path), "feature/spike");
    assert!(
        Repository::open(dir.path())
            .unwrap()
            .find_branch("feature/spike", BranchType::Local)
            .is_ok()
    );
}

#[test]
fn add_is_idempotent_and_reuses_existing_worktree() {
    let dir = setup_worktree_repo();
    let wt_home = TempDir::new().unwrap();
    let wt_path = wt_home.path().join("feature-a");

    let first = wt_add(
        dir.path(),
        &["feature-a", "--path", wt_path.to_str().unwrap()],
    );
    assert!(first.status.success());

    // A second add for the same branch (no --path) returns the existing worktree
    // rather than erroring or creating a duplicate.
    let second = wt_add(dir.path(), &["feature-a"]);
    assert!(
        second.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_path = String::from_utf8_lossy(&second.stdout).trim().to_string();
    assert_eq!(
        fs::canonicalize(&second_path).unwrap(),
        fs::canonicalize(&wt_path).unwrap(),
        "re-adding a branch should reuse its existing worktree"
    );

    // And there is exactly one worktree for feature-a in git's list.
    let feature_worktrees = String::from_utf8_lossy(
        &std::process::Command::new("git")
            .args(["worktree", "list"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .stdout,
    )
    .lines()
    .filter(|line| line.contains("[feature-a]"))
    .count();
    assert_eq!(
        feature_worktrees, 1,
        "reuse must not create a duplicate worktree"
    );
}

#[test]
fn add_warns_when_reuse_ignores_explicit_path_override() {
    let dir = setup_worktree_repo();
    let wt_home = TempDir::new().unwrap();
    let wt_path = wt_home.path().join("feature-a");
    let ignored_path = wt_home.path().join("ignored");

    let first = wt_add(
        dir.path(),
        &["feature-a", "--path", wt_path.to_str().unwrap()],
    );
    assert!(first.status.success());

    let second = wt_add(
        dir.path(),
        &["feature-a", "--path", ignored_path.to_str().unwrap()],
    );
    assert!(
        second.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_path = String::from_utf8_lossy(&second.stdout).trim().to_string();
    assert_eq!(
        fs::canonicalize(&second_path).unwrap(),
        fs::canonicalize(&wt_path).unwrap(),
        "re-adding a branch should still reuse its existing worktree"
    );
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("Warning: ignoring --path"),
        "stderr:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        !ignored_path.exists(),
        "ignored override path should not be created"
    );
}

#[test]
fn cleanup_ignores_added_plain_worktree_even_when_merged() {
    let dir = setup_worktree_repo();
    let wt_home = TempDir::new().unwrap();
    let wt_path = wt_home.path().join("feature-a");

    assert!(
        wt_add(
            dir.path(),
            &["feature-a", "--path", wt_path.to_str().unwrap()]
        )
        .status
        .success()
    );
    // Merge feature-a into trunk so it would qualify as "merged" for a temp.
    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());

    let output = kin_cmd()
        .args(["wt", "cleanup", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    // The added worktree is not under the temp root, so cleanup never touches it.
    assert!(
        wt_path.exists(),
        "cleanup must not remove a plain (non-temp) worktree"
    );
}

#[test]
fn remove_removes_added_worktree_by_branch() {
    let dir = setup_worktree_repo();
    let wt_home = TempDir::new().unwrap();
    let wt_path = wt_home.path().join("feature-a");

    assert!(
        wt_add(
            dir.path(),
            &["feature-a", "--path", wt_path.to_str().unwrap()]
        )
        .status
        .success()
    );

    let output = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // A roleless worktree is reported as `plain`.
    assert!(String::from_utf8_lossy(&output.stdout).contains("Removed plain worktree 'feature-a'"));
    assert!(!wt_path.exists());
}

#[test]
fn add_runs_global_create_hooks() {
    let dir = setup_worktree_repo();
    write_repo_config(
        dir.path(),
        "[worktrees.hooks]\non_create = [\"printf hooked > hook-marker.txt\"]\n",
    );
    let wt_home = TempDir::new().unwrap();
    let wt_path = wt_home.path().join("feature-a");

    let output = wt_add(
        dir.path(),
        &["feature-a", "--path", wt_path.to_str().unwrap()],
    );
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(wt_path.join("hook-marker.txt")).unwrap(),
        "hooked"
    );
}

#[test]
fn add_rolls_back_worktree_when_global_hook_fails() {
    let dir = setup_worktree_repo();
    write_repo_config(dir.path(), "[worktrees.hooks]\non_create = [\"exit 1\"]\n");
    let wt_home = TempDir::new().unwrap();
    let wt_path = wt_home.path().join("feature-a");

    let output = wt_add(
        dir.path(),
        &["feature-a", "--path", wt_path.to_str().unwrap()],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("hook failed"));
    assert!(
        !wt_path.exists(),
        "a failed create hook must roll back the worktree"
    );
}

#[test]
fn add_uses_configured_default_location() {
    let dir = setup_worktree_repo();
    // Point the default at a deterministic path inside the repo for the test.
    write_repo_config(
        dir.path(),
        "[worktrees]\nadd_path_template = \"managed-adds/{branch}\"\n",
    );

    let output = wt_add(dir.path(), &["feature-a"]);
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = dir.path().join("managed-adds/feature-a");
    assert!(
        expected.exists(),
        "worktree should land at the configured default"
    );
    assert_eq!(current_branch(&expected), "feature-a");
}

/// `kin wt add <branch> --path P` where P is already a live worktree for a
/// *different* branch is rejected.
#[test]
fn add_rejects_path_already_used_by_another_branch() {
    let dir = setup_worktree_repo();
    run_ok("git", &["branch", "feature-b"], dir.path());
    let wt_home = TempDir::new().unwrap();
    // Canonicalize so the path we pass matches what `git worktree list` reports
    // (git resolves symlinks like macOS's /var -> /private/var); otherwise the
    // path lookup misses and we'd hit the "not a worktree" branch instead.
    let shared = std::fs::canonicalize(wt_home.path())
        .unwrap()
        .join("shared");

    // feature-a takes the path first.
    let out = wt_add(
        dir.path(),
        &["feature-a", "--path", shared.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // feature-b at the same path must fail, naming the current occupant.
    let out = wt_add(
        dir.path(),
        &["feature-b", "--path", shared.to_str().unwrap()],
    );
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("already in use by branch 'feature-a'"),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The occupant is untouched.
    assert_eq!(current_branch(&shared), "feature-a");
}

#[test]
fn add_resolves_relative_path_override_from_repo_root() {
    let dir = setup_worktree_repo();
    run_ok("git", &["branch", "feature-b"], dir.path());
    let nested = dir.path().join("nested");
    fs::create_dir_all(&nested).unwrap();
    let shared = dir.path().join("managed/shared");

    let first = wt_add(dir.path(), &["feature-a", "--path", "managed/shared"]);
    assert!(
        first.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&first.stderr)
    );

    let second = kin_cmd()
        .args(["wt", "add", "feature-b", "--path", "managed/shared"])
        .current_dir(&nested)
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("already in use by branch 'feature-a'"),
        "stderr:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(current_branch(&shared), "feature-a");
}

/// `kin wt add <branch> --path P` where P exists on disk but is not a git
/// worktree is rejected.
#[test]
fn add_rejects_path_that_exists_but_is_not_a_worktree() {
    let dir = setup_worktree_repo();
    let wt_home = TempDir::new().unwrap();
    let occupied = wt_home.path().join("occupied");
    fs::create_dir_all(&occupied).unwrap();
    fs::write(occupied.join("stray.txt"), "x").unwrap();

    let out = wt_add(
        dir.path(),
        &["feature-a", "--path", occupied.to_str().unwrap()],
    );
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("already exists but is not a git worktree"),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Nothing was added; only the primary and the stray dir exist.
    assert!(!list(dir.path()).contains("feature-a"));
}

/// Removing a plain (roleless) worktree runs the global `on_remove` hook with
/// `KINDRA_WORKTREE_ROLE=-`, not the creation label `add`.
#[cfg(unix)]
#[test]
fn remove_plain_worktree_runs_global_hook_with_dash_role() {
    let dir = setup_worktree_repo();
    let marker = dir.path().join("removed-role.txt");
    // The hook runs in the worktree being removed, so write the role to an
    // absolute path outside it.
    write_repo_config(
        dir.path(),
        &format!(
            "[worktrees.hooks]\non_remove = [\"printf %s \\\"$KINDRA_WORKTREE_ROLE\\\" > {}\"]\n",
            marker.display()
        ),
    );
    let wt_home = TempDir::new().unwrap();
    let wt_path = wt_home.path().join("feature-a");
    assert!(
        wt_add(
            dir.path(),
            &["feature-a", "--path", wt_path.to_str().unwrap()]
        )
        .status
        .success()
    );

    let out = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read_to_string(&marker).unwrap(), "-");
}
