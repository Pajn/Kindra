mod common;

use common::{kin_cmd, make_commit, repo_init, run_ok};
use git2::{BranchType, Repository};
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

#[test]
fn sync_aborts_deletions_if_fallback_checkout_is_blocked() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&base],
    );
    let _a = repo.find_commit(a_id).unwrap();

    // Merge feature-a into main
    run_ok("git", &["checkout", "-f", "main"], dir.path());
    run_ok(
        "git",
        &["merge", "--ff-only", &a_id.to_string()],
        dir.path(),
    );

    // Go back to feature-a
    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    // Create a commit on main that adds a file
    run_ok("git", &["checkout", "-f", "main"], dir.path());
    fs::write(dir.path().join("main_only.txt"), "main content").unwrap();
    run_ok("git", &["add", "main_only.txt"], dir.path());
    run_ok("git", &["commit", "-m", "add main_only"], dir.path());

    // Go back to feature-a
    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    // Create a dirty worktree by adding an untracked file that conflicts with the one in 'main'
    fs::write(dir.path().join("main_only.txt"), "dirty content").unwrap();

    // Now 'kin sync' will try to delete 'feature-a', which is the current branch.
    // It should first try to checkout 'main'.
    // 'git checkout main' should fail because main_only.txt would be overwritten.

    let mut cmd = kin_cmd();
    cmd.arg("sync")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("fallback git checkout failed"));

    // Verify feature-a was NOT deleted.
    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo.find_branch("feature-a", BranchType::Local).is_ok());
}

#[test]
fn sync_no_delete_with_open_worktree_does_not_fail() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&base],
    );
    let a = repo.find_commit(a_id).unwrap();

    let _b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feature b",
        &[&a],
    );

    // Merge feature-a into main
    run_ok("git", &["checkout", "-f", "main"], dir.path());
    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());

    // Create a worktree for feature-a
    let wt_dir = tempdir().unwrap();
    run_ok(
        "git",
        &[
            "worktree",
            "add",
            wt_dir.path().to_str().unwrap(),
            "feature-a",
        ],
        dir.path(),
    );

    // Checkout feature-b in the main worktree
    run_ok("git", &["checkout", "-f", "feature-b"], dir.path());

    // 'kin sync --no-delete' should NOT fail even though feature-a is checked out elsewhere,
    // because we are not going to delete it.
    let mut cmd = kin_cmd();
    cmd.arg("sync")
        .arg("--no-delete")
        .current_dir(dir.path())
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo.find_branch("feature-a", BranchType::Local).is_ok());
}

#[test]
fn sync_onto_remote_tracking_ref_does_not_delete_local_base() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let remote_dir = dir.path().join("remote.git");
    fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    run_ok("git", &["push", "-u", "origin", "main:main"], dir.path());

    let _feature_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&repo.find_commit(base_id).unwrap()],
    );

    // Advance main on remote
    let remote_worktree = tempdir().unwrap();
    run_ok(
        "git",
        &[
            "clone",
            remote_dir.to_str().unwrap(),
            remote_worktree.path().to_str().unwrap(),
        ],
        dir.path(),
    );
    run_ok("git", &["checkout", "main"], remote_worktree.path());
    fs::write(remote_worktree.path().join("remote.txt"), "remote").unwrap();
    run_ok("git", &["add", "remote.txt"], remote_worktree.path());
    run_ok(
        "git",
        &["commit", "-m", "remote advanced"],
        remote_worktree.path(),
    );
    run_ok("git", &["push", "origin", "main"], remote_worktree.path());

    // Local 'main' is still at base_id. 'origin/main' is ahead.
    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    // 'kin sync' should rebase feature-a onto origin/main.
    // It should NOT delete local 'main' branch, even though 'main' is an ancestor of 'origin/main'.
    let mut cmd = kin_cmd();
    cmd.arg("sync").current_dir(dir.path()).assert().success();

    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo.find_branch("main", BranchType::Local).is_ok());
    assert!(repo.find_branch("feature-a", BranchType::Local).is_ok());

    let feature_a_tip = repo.revparse_single("feature-a").unwrap().id();
    let origin_main_tip = repo.revparse_single("origin/main").unwrap().id();
    assert!(
        repo.graph_descendant_of(feature_a_tip, origin_main_tip)
            .unwrap()
    );
}

#[test]
fn sync_does_not_treat_rename_only_branch_as_integrated_when_target_keeps_source_path() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    run_ok("git", &["config", "diff.renames", "true"], dir.path());

    let remote_dir = dir.path().join("remote.git");
    fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "old.txt",
        "payload",
        "base commit",
        &[],
    );
    run_ok("git", &["push", "-u", "origin", "main:main"], dir.path());

    run_ok("git", &["checkout", "-b", "feature-a"], dir.path());
    run_ok("git", &["mv", "old.txt", "new.txt"], dir.path());
    run_ok("git", &["commit", "-m", "rename old to new"], dir.path());

    let remote_worktree = tempdir().unwrap();
    run_ok(
        "git",
        &[
            "clone",
            remote_dir.to_str().unwrap(),
            remote_worktree.path().to_str().unwrap(),
        ],
        dir.path(),
    );
    run_ok("git", &["checkout", "main"], remote_worktree.path());
    run_ok(
        "git",
        &["reset", "--hard", &base_id.to_string()],
        remote_worktree.path(),
    );
    fs::write(remote_worktree.path().join("new.txt"), "payload").unwrap();
    run_ok("git", &["add", "new.txt"], remote_worktree.path());
    run_ok(
        "git",
        &["commit", "-m", "add matching new path"],
        remote_worktree.path(),
    );
    run_ok("git", &["push", "origin", "main"], remote_worktree.path());

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("sync").current_dir(dir.path()).assert().success();

    let repo = Repository::open(dir.path()).unwrap();
    assert!(
        repo.find_branch("feature-a", BranchType::Local).is_ok(),
        "rename-only branch should not be treated as integrated and deleted"
    );

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    assert!(
        !dir.path().join("old.txt").exists(),
        "sync should preserve the source-path deletion from the rename commit"
    );
    assert!(
        dir.path().join("new.txt").exists(),
        "sync should preserve the destination path from the rename commit"
    );
}

/// Two branches parked on the same commit are both reported as stack tips, but
/// the choice between them is immaterial: `git rebase --update-refs` rewrites
/// that commit once and carries every co-located ref along. Sync must skip the
/// "Multiple stack tips" prompt here (proven by success in a non-interactive
/// run, where an unavoidable prompt would hard-error) and move both refs.
#[test]
fn sync_skips_tip_prompt_when_tips_share_a_commit() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let remote_dir = dir.path().join("remote.git");
    fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    run_ok("git", &["push", "-u", "origin", "main:main"], dir.path());

    // `analyze` carries one commit; `perf` is parked on the exact same commit.
    make_commit(
        &repo,
        "refs/heads/analyze",
        "a.txt",
        "a",
        "feature work",
        &[&repo.find_commit(base_id).unwrap()],
    );
    run_ok("git", &["branch", "perf", "analyze"], dir.path());

    // Advance origin/main so sync has real work to do (a rebase, not a no-op).
    let remote_worktree = tempdir().unwrap();
    run_ok(
        "git",
        &[
            "clone",
            remote_dir.to_str().unwrap(),
            remote_worktree.path().to_str().unwrap(),
        ],
        dir.path(),
    );
    run_ok("git", &["checkout", "main"], remote_worktree.path());
    fs::write(remote_worktree.path().join("remote.txt"), "remote").unwrap();
    run_ok("git", &["add", "remote.txt"], remote_worktree.path());
    run_ok(
        "git",
        &["commit", "-m", "remote advanced"],
        remote_worktree.path(),
    );
    run_ok("git", &["push", "origin", "main"], remote_worktree.path());

    run_ok("git", &["checkout", "-f", "analyze"], dir.path());

    // Non-interactive: a genuinely required prompt would fail the command.
    let mut cmd = kin_cmd();
    cmd.arg("sync").current_dir(dir.path()).assert().success();

    let repo = Repository::open(dir.path()).unwrap();
    let origin_main = repo.revparse_single("origin/main").unwrap().id();
    let analyze_tip = repo.revparse_single("analyze").unwrap().id();
    let perf_tip = repo.revparse_single("perf").unwrap().id();

    assert!(
        repo.graph_descendant_of(analyze_tip, origin_main).unwrap(),
        "analyze should be rebased onto the advanced origin/main"
    );
    assert!(
        repo.graph_descendant_of(perf_tip, origin_main).unwrap(),
        "perf should be carried onto origin/main by --update-refs"
    );
    assert_eq!(
        analyze_tip, perf_tip,
        "co-located tips must remain co-located after the sync"
    );
}

/// A real fork is synced in full without requiring an interactive tip choice.
#[test]
fn sync_rebases_all_divergent_tips() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let remote_dir = dir.path().join("remote.git");
    fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    run_ok("git", &["push", "-u", "origin", "main:main"], dir.path());

    // A shared branch `mid`, then two diverging children on distinct commits.
    let mid_id = make_commit(
        &repo,
        "refs/heads/mid",
        "mid.txt",
        "mid",
        "mid",
        &[&repo.find_commit(base_id).unwrap()],
    );
    make_commit(
        &repo,
        "refs/heads/analyze",
        "a.txt",
        "a",
        "analyze work",
        &[&repo.find_commit(mid_id).unwrap()],
    );
    make_commit(
        &repo,
        "refs/heads/perf",
        "p.txt",
        "p",
        "perf work",
        &[&repo.find_commit(mid_id).unwrap()],
    );

    // Advance origin/main so sync gets past preflight to tip selection.
    let remote_worktree = tempdir().unwrap();
    run_ok(
        "git",
        &[
            "clone",
            remote_dir.to_str().unwrap(),
            remote_worktree.path().to_str().unwrap(),
        ],
        dir.path(),
    );
    run_ok("git", &["checkout", "main"], remote_worktree.path());
    fs::write(remote_worktree.path().join("remote.txt"), "remote").unwrap();
    run_ok("git", &["add", "remote.txt"], remote_worktree.path());
    run_ok(
        "git",
        &["commit", "-m", "remote advanced"],
        remote_worktree.path(),
    );
    run_ok("git", &["push", "origin", "main"], remote_worktree.path());

    // Sit on the shared branch so both children are seen as divergent tips.
    run_ok("git", &["checkout", "-f", "mid"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("sync").current_dir(dir.path()).assert().success();
    let main = repo.revparse_single("origin/main").unwrap().id();
    let mid = repo.revparse_single("mid").unwrap().id();
    assert!(repo.graph_descendant_of(mid, main).unwrap());
    for name in ["analyze", "perf"] {
        let tip = repo.revparse_single(name).unwrap().id();
        assert_eq!(repo.find_commit(tip).unwrap().parent_id(0).unwrap(), mid);
    }
    assert_eq!(repo.head().unwrap().shorthand(), Some("mid"));
}

#[test]
fn sync_restores_lower_branch_after_rebasing_stack() {
    check_sync_checkout(false, false);
}

#[test]
fn sync_continue_restores_lower_branch_after_conflict() {
    check_sync_checkout(true, false);
}

#[test]
fn sync_uses_base_when_original_lower_branch_is_deleted() {
    check_sync_checkout(false, true);
}

fn check_sync_checkout(conflict: bool, merged: bool) {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());
    make_commit(
        &repo,
        "refs/heads/main",
        "shared.txt",
        "base\n",
        "base",
        &[],
    );
    run_ok("git", &["checkout", "-b", "lower"], dir.path());
    fs::write(dir.path().join("shared.txt"), "lower\n").unwrap();
    run_ok("git", &["commit", "-am", "lower"], dir.path());
    run_ok("git", &["checkout", "-b", "upper"], dir.path());
    fs::write(dir.path().join("upper.txt"), "upper\n").unwrap();
    run_ok("git", &["add", "upper.txt"], dir.path());
    run_ok("git", &["commit", "-m", "upper"], dir.path());
    let old_upper = repo.revparse_single("upper").unwrap().id();
    run_ok("git", &["checkout", "main"], dir.path());
    if merged {
        run_ok("git", &["merge", "--ff-only", "lower"], dir.path());
    }
    let path = if conflict { "shared.txt" } else { "main.txt" };
    fs::write(dir.path().join(path), "upstream\n").unwrap();
    run_ok("git", &["add", path], dir.path());
    run_ok("git", &["commit", "-m", "advance main"], dir.path());
    run_ok("git", &["checkout", "lower"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("sync").current_dir(dir.path());
    if conflict {
        cmd.assert()
            .failure()
            .stderr(predicate::str::contains("Resolve conflicts"));
        assert!(dir.path().join(".git/kindra_rebase_state.json").exists());
        fs::write(dir.path().join("shared.txt"), "resolved\n").unwrap();
        run_ok("git", &["add", "shared.txt"], dir.path());
        kin_cmd()
            .arg("continue")
            .current_dir(dir.path())
            .assert()
            .success();
    } else {
        cmd.assert().success();
    }

    let repo = Repository::open(dir.path()).unwrap();
    assert_eq!(
        repo.head().unwrap().shorthand(),
        Some(if merged { "main" } else { "lower" })
    );
    let upper = repo.revparse_single("upper").unwrap().id();
    let main = repo.revparse_single("main").unwrap().id();
    assert_ne!(upper, old_upper);
    assert!(repo.graph_descendant_of(upper, main).unwrap());
    if merged {
        assert!(repo.find_branch("lower", BranchType::Local).is_err());
    } else {
        let lower = repo.revparse_single("lower").unwrap().id();
        assert!(repo.graph_descendant_of(lower, main).unwrap());
        assert!(repo.graph_descendant_of(upper, lower).unwrap());
    }
    assert!(!dir.path().join(".git/kindra_rebase_state.json").exists());
}

#[test]
fn sync_manual_continue_preserves_checkout_recovery_after_status() {
    check_sync_recovery_after_branch_switch(true);
}

#[test]
fn sync_autostash_handles_dirty_path_changed_on_tip() {
    check_sync_recovery_after_branch_switch(false);
}

fn sync_recovery_repo(manual_continue: bool) -> tempfile::TempDir {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());
    make_commit(
        &repo,
        "refs/heads/main",
        "shared.txt",
        "base\n",
        "base",
        &[],
    );
    run_ok("git", &["checkout", "-b", "lower"], dir.path());
    fs::write(dir.path().join("shared.txt"), "lower\n").unwrap();
    run_ok("git", &["commit", "-am", "lower"], dir.path());
    run_ok("git", &["checkout", "-b", "upper"], dir.path());
    let upper_path = if manual_continue {
        "upper.txt"
    } else {
        "shared.txt"
    };
    fs::write(dir.path().join(upper_path), "upper\n").unwrap();
    run_ok("git", &["add", upper_path], dir.path());
    run_ok("git", &["commit", "-m", "upper"], dir.path());
    run_ok("git", &["checkout", "main"], dir.path());
    let upstream_path = if manual_continue {
        "shared.txt"
    } else {
        "main.txt"
    };
    fs::write(dir.path().join(upstream_path), "upstream\n").unwrap();
    run_ok("git", &["add", upstream_path], dir.path());
    run_ok("git", &["commit", "-m", "advance main"], dir.path());
    run_ok("git", &["checkout", "lower"], dir.path());

    dir
}

fn check_sync_recovery_after_branch_switch(manual_continue: bool) {
    let dir = sync_recovery_repo(manual_continue);
    if manual_continue {
        kin_cmd()
            .args(["sync", "--no-delete"])
            .current_dir(dir.path())
            .assert()
            .failure();
        fs::write(dir.path().join("shared.txt"), "resolved\n").unwrap();
        run_ok("git", &["add", "shared.txt"], dir.path());
        run_ok(
            "git",
            &["-c", "core.editor=true", "rebase", "--continue"],
            dir.path(),
        );
        kin_cmd()
            .arg("status")
            .current_dir(dir.path())
            .assert()
            .success();
        kin_cmd()
            .arg("continue")
            .current_dir(dir.path())
            .assert()
            .success();
    } else {
        fs::write(dir.path().join("shared.txt"), "dirty\n").unwrap();
        kin_cmd()
            .args(["sync", "--autostash"])
            .current_dir(dir.path())
            .assert()
            .success();
        assert_eq!(
            fs::read_to_string(dir.path().join("shared.txt")).unwrap(),
            "dirty\n"
        );
    }
    let repo = Repository::open(dir.path()).unwrap();
    assert_eq!(repo.head().unwrap().shorthand(), Some("lower"));
    assert!(!dir.path().join(".git/kindra_rebase_state.json").exists());
}

#[test]
fn sync_autostash_restored_on_caller_after_conflict_continue() {
    check_sync_autostash_conflict_recovery(false);
}

#[test]
fn sync_autostash_restored_on_caller_after_conflict_abort() {
    check_sync_autostash_conflict_recovery(true);
}

fn check_sync_autostash_conflict_recovery(abort: bool) {
    let dir = sync_recovery_repo(true);
    fs::write(dir.path().join("shared.txt"), "dirty\n").unwrap();
    run_ok("git", &["add", "shared.txt"], dir.path());
    kin_cmd()
        .args(["sync", "--autostash"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("Resolve conflicts"));
    let repo = Repository::open(dir.path()).unwrap();
    let state = kindra::rebase_utils::load_state(&repo).unwrap();
    assert!(state.stash_ref.is_some());
    if !abort {
        fs::write(dir.path().join("shared.txt"), "lower\n").unwrap();
        run_ok("git", &["add", "shared.txt"], dir.path());
    }
    kin_cmd()
        .arg(if abort { "abort" } else { "continue" })
        .current_dir(dir.path())
        .assert()
        .success();
    assert_eq!(repo.head().unwrap().shorthand(), Some("lower"));
    assert_eq!(
        fs::read_to_string(dir.path().join("shared.txt")).unwrap(),
        "dirty\n"
    );
    assert!(
        repo.status_file(std::path::Path::new("shared.txt"))
            .unwrap()
            .is_index_modified()
    );
    assert!(!dir.path().join(".git/kindra_rebase_state.json").exists());
    assert!(repo.find_reference("refs/stash").is_err());
}

#[test]
fn sync_autostash_can_abort_after_tip_checkout_is_blocked() {
    let dir = sync_recovery_repo(true);
    fs::write(dir.path().join("shared.txt"), "dirty\n").unwrap();
    // An untracked file blocks checkout even after tracked edits are stashed.
    fs::write(dir.path().join("upper.txt"), "untracked\n").unwrap();
    kin_cmd()
        .args(["sync", "--autostash"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("git checkout failed"));
    let repo = Repository::open(dir.path()).unwrap();
    assert!(
        kindra::rebase_utils::load_state(&repo)
            .unwrap()
            .stash_ref
            .is_some()
    );
    kin_cmd()
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();
    assert_eq!(repo.head().unwrap().shorthand(), Some("lower"));
    assert_eq!(
        fs::read_to_string(dir.path().join("shared.txt")).unwrap(),
        "dirty\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("upper.txt")).unwrap(),
        "untracked\n"
    );
    assert!(!dir.path().join(".git/kindra_rebase_state.json").exists());
    assert!(repo.find_reference("refs/stash").is_err());
}

#[test]
fn sync_no_delete_stash_conflict_preserves_state_until_resolved() {
    let dir = sync_recovery_repo(true);
    fs::write(dir.path().join("shared.txt"), "dirty\n").unwrap();
    kin_cmd()
        .args(["sync", "--no-delete", "--autostash"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("Resolve conflicts"));

    // Resolve the rebase differently from the original lower branch so that
    // restoring its saved working-tree edits conflicts after the rebase ends.
    fs::write(dir.path().join("shared.txt"), "rebased\n").unwrap();
    run_ok("git", &["add", "shared.txt"], dir.path());
    kin_cmd()
        .arg("continue")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Restoring the set-aside changes hit conflicts",
        ));

    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo.index().unwrap().has_conflicts());
    assert_eq!(repo.head().unwrap().shorthand(), Some("lower"));
    let state = kindra::rebase_utils::load_state(&repo).unwrap();
    assert!(state.stash_ref.is_none(), "stash must not be applied twice");
    assert!(state.cleanup_merged_branches.is_empty());
    let state_path = dir.path().join(".git/kindra_rebase_state.json");
    kin_cmd()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .success();
    assert!(
        state_path.exists(),
        "status must retain recovery state while stash conflicts remain"
    );
    assert!(repo.index().unwrap().has_conflicts());

    fs::write(dir.path().join("shared.txt"), "resolved edits\n").unwrap();
    run_ok("git", &["add", "shared.txt"], dir.path());
    kin_cmd()
        .arg("continue")
        .current_dir(dir.path())
        .assert()
        .success();
    assert!(!state_path.exists());
    let repo = Repository::open(dir.path()).unwrap();
    assert!(!repo.index().unwrap().has_conflicts());
    assert_eq!(repo.head().unwrap().shorthand(), Some("lower"));
    assert_eq!(
        fs::read_to_string(dir.path().join("shared.txt")).unwrap(),
        "resolved edits\n"
    );
}

#[test]
fn sync_tree_continue_after_later_sibling_conflict() {
    check_tree_recovery(false, false);
}

#[test]
fn sync_tree_abort_restores_already_rebased_branches() {
    check_tree_recovery(true, false);
}

#[test]
fn sync_tree_manual_abort_keeps_pending_work() {
    check_tree_recovery(false, true);
}

fn check_tree_recovery(abort: bool, manual_abort: bool) {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());
    let base = make_commit(
        &Repository::open(dir.path()).unwrap(),
        "refs/heads/main",
        "shared.txt",
        "base\n",
        "base",
        &[],
    );
    let mut tips = std::collections::HashMap::new();
    for (name, parent, path) in [
        ("root", "main", "root.txt"),
        ("a", "root", "a.txt"),
        ("a-child", "a", "child.txt"),
        ("z", "root", "shared.txt"),
        ("z-child", "z", "z-child.txt"),
    ] {
        run_ok("git", &["checkout", "-b", name, parent], dir.path());
        fs::write(dir.path().join(path), "feature\n").unwrap();
        run_ok("git", &["add", path], dir.path());
        run_ok("git", &["commit", "-m", name], dir.path());
        tips.insert(name, repo.revparse_single(name).unwrap().id());
    }
    run_ok("git", &["checkout", "main"], dir.path());
    fs::write(dir.path().join("shared.txt"), "upstream\n").unwrap();
    run_ok("git", &["commit", "-am", "advance"], dir.path());
    let main = repo.revparse_single("main").unwrap().id();
    // Start at a leaf: discovery must include cousins, but exclude independent stacks.
    run_ok(
        "git",
        &["checkout", "-b", "unrelated", &base.to_string()],
        dir.path(),
    );
    fs::write(dir.path().join("other.txt"), "other\n").unwrap();
    run_ok("git", &["add", "other.txt"], dir.path());
    run_ok("git", &["commit", "-m", "other"], dir.path());
    let unrelated = repo.revparse_single("unrelated").unwrap().id();
    run_ok("git", &["checkout", "-f", "a-child"], dir.path());
    let result = kin_cmd()
        .args(["sync", "--no-delete"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("Resolve conflicts"));
    assert_ne!(
        repo.revparse_single("root").unwrap().id(),
        tips["root"],
        "{:?}",
        result.get_output()
    );
    if manual_abort {
        let rebased_root = repo.revparse_single("root").unwrap().id();
        run_ok("git", &["rebase", "--abort"], dir.path());
        assert_eq!(repo.revparse_single("root").unwrap().id(), rebased_root);
        kin_cmd()
            .arg("status")
            .current_dir(dir.path())
            .assert()
            .success();
        assert!(dir.path().join(".git/kindra_rebase_state.json").exists());
        kin_cmd()
            .arg("continue")
            .current_dir(dir.path())
            .assert()
            .failure()
            .stderr(predicate::str::contains("Resolve conflicts"));
    }
    if !abort {
        fs::write(dir.path().join("shared.txt"), "resolved\n").unwrap();
        run_ok("git", &["add", "shared.txt"], dir.path());
    }
    kin_cmd()
        .arg(if abort { "abort" } else { "continue" })
        .current_dir(dir.path())
        .assert()
        .success();
    assert_eq!(repo.head().unwrap().shorthand(), Some("a-child"));
    assert_eq!(repo.revparse_single("unrelated").unwrap().id(), unrelated);
    if abort {
        for (name, tip) in tips {
            assert_eq!(repo.revparse_single(name).unwrap().id(), tip);
        }
    } else {
        for (name, parent) in [
            ("root", "main"),
            ("a", "root"),
            ("a-child", "a"),
            ("z", "root"),
            ("z-child", "z"),
        ] {
            let tip = repo.revparse_single(name).unwrap().id();
            assert!(repo.graph_descendant_of(tip, main).unwrap());
            assert_eq!(
                repo.find_commit(tip).unwrap().parent_id(0).unwrap(),
                repo.revparse_single(parent).unwrap().id(),
                "branch {name} should be on {parent}"
            );
        }
    }
    assert!(!dir.path().join(".git/kindra_rebase_state.json").exists());
}

#[test]
fn sync_tree_deletes_squashed_root_and_restores_staged_changes() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());
    make_commit(&repo, "refs/heads/main", "base.txt", "base\n", "base", &[]);
    for (name, parent) in [("root", "main"), ("left", "root"), ("right", "root")] {
        run_ok("git", &["checkout", "-b", name, parent], dir.path());
        fs::write(dir.path().join(format!("{name}.txt")), name).unwrap();
        run_ok("git", &["add", "."], dir.path());
        run_ok("git", &["commit", "-m", name], dir.path());
    }
    run_ok("git", &["checkout", "main"], dir.path());
    run_ok("git", &["merge", "--squash", "root"], dir.path());
    run_ok("git", &["commit", "-m", "squashed root"], dir.path());
    run_ok("git", &["checkout", "root"], dir.path());
    fs::write(dir.path().join("base.txt"), "dirty\n").unwrap();
    run_ok("git", &["add", "base.txt"], dir.path());
    kin_cmd()
        .args(["sync", "--autostash"])
        .current_dir(dir.path())
        .assert()
        .success();
    assert!(repo.find_branch("root", BranchType::Local).is_err());
    assert_eq!(repo.head().unwrap().shorthand(), Some("main"));
    assert_eq!(
        fs::read_to_string(dir.path().join("base.txt")).unwrap(),
        "dirty\n"
    );
    assert!(
        Repository::open(dir.path())
            .unwrap()
            .status_file(std::path::Path::new("base.txt"))
            .unwrap()
            .is_index_modified()
    );
    let main = repo.revparse_single("main").unwrap().id();
    for name in ["left", "right"] {
        let tip = repo.revparse_single(name).unwrap().id();
        assert_eq!(repo.find_commit(tip).unwrap().parent_id(0).unwrap(), main);
    }
}

#[test]
fn sync_tree_cleanup_excludes_branches_retained_by_the_plan() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());
    let base = make_commit(
        &repo,
        "refs/heads/main",
        "shared.txt",
        "base\n",
        "base",
        &[],
    );
    run_ok("git", &["checkout", "-b", "a-shared"], dir.path());
    fs::write(dir.path().join("shared.txt"), "feature\n").unwrap();
    run_ok("git", &["commit", "-am", "shared"], dir.path());
    run_ok("git", &["checkout", "-b", "b-combined", "main"], dir.path());
    fs::write(dir.path().join("shared.txt"), "feature\n").unwrap();
    fs::write(dir.path().join("b.txt"), "b\n").unwrap();
    run_ok("git", &["add", "."], dir.path());
    run_ok("git", &["commit", "-m", "combined"], dir.path());
    run_ok(
        "git",
        &["checkout", "-b", "joined", "b-combined"],
        dir.path(),
    );
    run_ok(
        "git",
        &["merge", "--no-ff", "a-shared", "-m", "join"],
        dir.path(),
    );
    run_ok("git", &["checkout", "main"], dir.path());
    run_ok(
        "git",
        &["cherry-pick", "--no-commit", "a-shared"],
        dir.path(),
    );
    run_ok("git", &["commit", "-m", "integrate shared"], dir.path());
    fs::write(dir.path().join("b.txt"), "b\n").unwrap();
    run_ok("git", &["add", "b.txt"], dir.path());
    run_ok(
        "git",
        &["commit", "-m", "integrate b separately"],
        dir.path(),
    );
    fs::write(dir.path().join("shared.txt"), "upstream\n").unwrap();
    run_ok("git", &["commit", "-am", "evolve shared"], dir.path());
    let branches = ["a-shared", "b-combined", "joined"]
        .iter()
        .map(|name| kindra::stack::StackBranch {
            name: name.to_string(),
            id: repo.revparse_single(name).unwrap().id(),
        })
        .collect::<Vec<_>>();
    // The combined patch is retained when examined on its own, but the joined
    // lineage can classify it as integrated relative to its shared sibling.
    assert!(
        kindra::stack::find_sync_boundary(&repo, "b-combined", "main", &branches)
            .unwrap()
            .old_base
            .is_some()
    );
    assert!(
        kindra::stack::find_sync_boundary(&repo, "joined", "main", &branches)
            .unwrap()
            .merged_branches
            .contains(&"b-combined".to_string())
    );
    let plan = kindra::stack::plan_tree_sync(&repo, &branches, "main", base).unwrap();
    assert!(plan.remaining.contains(&"b-combined".to_string()));
    assert!(
        plan.remaining
            .iter()
            .all(|branch| !plan.merged.contains(branch))
    );
}
