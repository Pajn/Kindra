mod common;

use common::{current_branch, kin_cmd, run_ok, setup_repo};
use std::process::Command;

fn branch_exists(cwd: &std::path::Path, name: &str) -> bool {
    let output = Command::new("git")
        .args(["branch", "--list", name])
        .current_dir(cwd)
        .output()
        .unwrap();
    !String::from_utf8_lossy(&output.stdout).trim().is_empty()
}

#[test]
fn renames_current_branch_with_single_arg() {
    let dir = setup_repo();
    // HEAD is on feature-b.
    kin_cmd()
        .current_dir(dir.path())
        .args(["rename", "feature-b-renamed"])
        .assert()
        .success();

    assert_eq!(current_branch(dir.path()), "feature-b-renamed");
    assert!(!branch_exists(dir.path(), "feature-b"));
    assert!(branch_exists(dir.path(), "feature-b-renamed"));
}

#[test]
fn renames_named_branch_with_two_args() {
    let dir = setup_repo();
    // Rename a different branch than the one checked out.
    kin_cmd()
        .current_dir(dir.path())
        .args(["rename", "feature-a", "feature-a-renamed"])
        .assert()
        .success();

    // HEAD stays on feature-b.
    assert_eq!(current_branch(dir.path()), "feature-b");
    assert!(!branch_exists(dir.path(), "feature-a"));
    assert!(branch_exists(dir.path(), "feature-a-renamed"));
}

#[test]
fn rename_preserves_stack_topology() {
    let dir = setup_repo();
    // feature-a is the parent of feature-b. Renaming it must keep the stack
    // intact so a restack is a no-op rather than reparenting feature-b onto main.
    kin_cmd()
        .current_dir(dir.path())
        .args(["rename", "feature-a", "feature-a-renamed"])
        .assert()
        .success();

    let before = Command::new("git")
        .args(["rev-parse", "feature-b"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    kin_cmd()
        .current_dir(dir.path())
        .args(["restack"])
        .assert()
        .success();

    let after = Command::new("git")
        .args(["rev-parse", "feature-b"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert_eq!(
        String::from_utf8_lossy(&before.stdout),
        String::from_utf8_lossy(&after.stdout),
        "feature-b tip should be unchanged: the stack survived the rename"
    );
}

#[test]
fn refuses_to_rename_upstream_branch() {
    let dir = setup_repo();
    kin_cmd()
        .current_dir(dir.path())
        .args(["rename", "main", "trunk"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("upstream branch"));

    assert!(branch_exists(dir.path(), "main"));
}

#[test]
fn refuses_to_rename_to_remote_only_base_name() {
    // With a remote-only base (origin/main, no local main), renaming a feature
    // branch to `main` would create a local `main` that find_upstream then
    // prefers — hijacking the stack base. The guard must block it.
    let dir = setup_repo();
    let root = dir.path();

    let remote = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    };
    Command::new("git")
        .args(["init", "--bare", "-q"])
        .arg(remote.path())
        .output()
        .unwrap();
    git(&["remote", "add", "origin", remote.path().to_str().unwrap()]);
    git(&["push", "-q", "origin", "main"]);
    // HEAD is on feature-b; drop local main so the base resolves to origin/main.
    git(&["branch", "-D", "main"]);

    kin_cmd()
        .current_dir(root)
        .args(["rename", "feature-b", "main"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("shadow the stack base"));

    assert!(branch_exists(root, "feature-b"));
    assert!(!branch_exists(root, "main"));
}

#[test]
fn refuses_to_rename_pinned_main_worktree_branch() {
    // A non-trunk branch pinned as the managed main worktree branch in
    // .git/kindra.toml can't be renamed: Kindra can't rewrite that pin, so a
    // rename would strand it and the next `kin wt main` would recreate a phantom.
    let dir = setup_repo();
    let root = dir.path();

    Command::new("git")
        .args(["branch", "develop"])
        .current_dir(root)
        .output()
        .unwrap();
    std::fs::write(
        root.join(".git/kindra.toml"),
        "[worktrees.main]\nbranch = \"develop\"\n",
    )
    .unwrap();

    kin_cmd()
        .current_dir(root)
        .args(["rename", "develop", "devx"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "pinned as the managed main worktree branch",
        ));

    assert!(branch_exists(root, "develop"));
    assert!(!branch_exists(root, "devx"));
}

#[test]
fn refuses_to_clobber_existing_branch() {
    let dir = setup_repo();
    kin_cmd()
        .current_dir(dir.path())
        .args(["rename", "feature-a", "feature-b"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("already exists"));

    assert!(branch_exists(dir.path(), "feature-a"));
}

#[test]
fn rename_nonexistent_branch_errors_instead_of_false_success() {
    let dir = setup_repo();
    // old == new but the branch does not exist: must report "not found", not a
    // false "already has that name" success (existence is checked first).
    kin_cmd()
        .current_dir(dir.path())
        .args(["rename", "ghost", "ghost"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not found"));
}

#[test]
fn rename_detached_head_single_arg_errors() {
    let dir = setup_repo();
    Command::new("git")
        .args(["checkout", "--detach"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .args(["rename", "whatever"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("detached HEAD"));
}

#[test]
fn rename_refuses_when_operation_in_progress() {
    let dir = setup_repo();
    std::fs::write(dir.path().join(".git/kindra_run_state.json"), "{}").unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .args(["rename", "feature-a", "feature-a-renamed"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("already in progress"));
    assert!(branch_exists(dir.path(), "feature-a"));
}

#[test]
fn rename_migrates_branch_tracking_config() {
    // The docs and code promise that shelling out to `git branch -m` carries the
    // branch's tracking config (`branch.<name>.*`) across the rename — the whole
    // reason it's used instead of `git2::Branch::rename`, which drops it. Assert
    // it, so a regression to the git2 rename can't pass silently.
    let dir = setup_repo();
    run_ok(
        "git",
        &["config", "branch.feature-a.remote", "origin"],
        dir.path(),
    );
    run_ok(
        "git",
        &["config", "branch.feature-a.merge", "refs/heads/feature-a"],
        dir.path(),
    );

    kin_cmd()
        .current_dir(dir.path())
        .args(["rename", "feature-a", "feature-a-renamed"])
        .assert()
        .success();

    let git_config = |key: &str| -> Option<String> {
        let out = Command::new("git")
            .args(["config", "--get", key])
            .current_dir(dir.path())
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    // The config section moved with the branch...
    assert_eq!(
        git_config("branch.feature-a-renamed.remote").as_deref(),
        Some("origin"),
        "tracking remote must migrate to the new branch name"
    );
    assert!(
        git_config("branch.feature-a-renamed.merge").is_some(),
        "tracking merge ref must migrate to the new branch name"
    );
    // ...and nothing is left behind under the old name.
    assert!(
        git_config("branch.feature-a.remote").is_none(),
        "old tracking config must not linger after the rename"
    );
    assert!(git_config("branch.feature-a.merge").is_none());
}
