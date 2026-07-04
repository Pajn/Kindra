mod common;

use common::{kin_cmd, repo_init, run_ok};
use git2::{BranchType, Repository};
use kindra::rebase_utils::{Operation, RebaseState};
use predicates::prelude::*;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

/// Build `main <- feature-a <- feature-b`, then advance `main` by one commit so
/// a subsequent `kin sync` has work to do. Each commit touches a distinct file
/// so the rebase never conflicts. Leaves HEAD on `feature-b`.
fn setup_stack_with_advanced_main(root: &Path) {
    init_with_base_commit(root);

    run_ok("git", &["checkout", "-b", "feature-a"], root);
    fs::write(root.join("a.txt"), "a").unwrap();
    run_ok("git", &["add", "."], root);
    run_ok("git", &["commit", "-m", "a1"], root);

    run_ok("git", &["checkout", "-b", "feature-b"], root);
    fs::write(root.join("b.txt"), "b").unwrap();
    run_ok("git", &["add", "."], root);
    run_ok("git", &["commit", "-m", "b1"], root);

    run_ok("git", &["checkout", "main"], root);
    fs::write(root.join("m.txt"), "m").unwrap();
    run_ok("git", &["add", "."], root);
    run_ok("git", &["commit", "-m", "advance main"], root);

    run_ok("git", &["checkout", "feature-b"], root);
}

fn branch_oid(root: &Path, name: &str) -> Option<String> {
    let repo = Repository::open(root).unwrap();
    repo.find_branch(name, BranchType::Local)
        .ok()
        .and_then(|b| b.get().target())
        .map(|oid| oid.to_string())
}

#[test]
fn undo_reverts_move_and_redo_reapplies() {
    // Round-trips undo/redo through a non-sync operation, exercising the restore
    // engine for the Move operation (shared by `move` and `restack`).
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_with_base_commit(root);

    run_ok("git", &["checkout", "-b", "feature-a"], root);
    fs::write(root.join("a.txt"), "a").unwrap();
    run_ok("git", &["add", "."], root);
    run_ok("git", &["commit", "-m", "a1"], root);

    run_ok("git", &["checkout", "-b", "feature-b"], root);
    fs::write(root.join("b.txt"), "b").unwrap();
    run_ok("git", &["add", "."], root);
    run_ok("git", &["commit", "-m", "b1"], root);

    // Move feature-b off feature-a and onto main directly.
    let pre_b = branch_oid(root, "feature-b").unwrap();
    kin_cmd()
        .current_dir(root)
        .args(["move", "--onto", "main"])
        .assert()
        .success();
    let post_b = branch_oid(root, "feature-b").unwrap();
    assert_ne!(
        pre_b, post_b,
        "move should have rebased feature-b onto main"
    );

    kin_cmd().current_dir(root).arg("undo").assert().success();
    assert_eq!(
        branch_oid(root, "feature-b").as_deref(),
        Some(pre_b.as_str())
    );

    kin_cmd().current_dir(root).arg("redo").assert().success();
    assert_eq!(
        branch_oid(root, "feature-b").as_deref(),
        Some(post_b.as_str())
    );
}

#[test]
fn undo_reverts_sync_and_redo_reapplies() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    setup_stack_with_advanced_main(root);

    let pre_a = branch_oid(root, "feature-a").unwrap();
    let pre_b = branch_oid(root, "feature-b").unwrap();

    kin_cmd().current_dir(root).arg("sync").assert().success();

    let post_a = branch_oid(root, "feature-a").unwrap();
    let post_b = branch_oid(root, "feature-b").unwrap();
    assert_ne!(pre_a, post_a, "sync should have rebased feature-a");
    assert_ne!(pre_b, post_b, "sync should have rebased feature-b");

    // Undo returns both branches to their pre-sync tips.
    kin_cmd().current_dir(root).arg("undo").assert().success();
    assert_eq!(
        branch_oid(root, "feature-a").as_deref(),
        Some(pre_a.as_str())
    );
    assert_eq!(
        branch_oid(root, "feature-b").as_deref(),
        Some(pre_b.as_str())
    );

    // Redo reapplies the exact post-sync tips.
    kin_cmd().current_dir(root).arg("redo").assert().success();
    assert_eq!(
        branch_oid(root, "feature-a").as_deref(),
        Some(post_a.as_str())
    );
    assert_eq!(
        branch_oid(root, "feature-b").as_deref(),
        Some(post_b.as_str())
    );
}

#[test]
fn new_operation_clears_redo_history() {
    // Documented invariant: running a new stack-rewriting operation clears the
    // redo history (finalize drains entries past the cursor). After an undo, a
    // fresh operation must therefore make `redo` a no-op.
    let dir = tempdir().unwrap();
    let root = dir.path();
    setup_stack_with_advanced_main(root);

    // First operation, then undo it so a redo is available.
    kin_cmd().current_dir(root).arg("sync").assert().success();
    kin_cmd().current_dir(root).arg("undo").assert().success();

    // A new recorded operation must truncate the redo tail. `main` is still
    // advanced after undo (undo only restores branch tips), so sync has work.
    kin_cmd().current_dir(root).arg("sync").assert().success();

    kin_cmd()
        .current_dir(root)
        .arg("redo")
        .assert()
        .success()
        .stdout(predicate::str::contains("Nothing to redo."));
}

#[test]
fn undo_survives_aggressive_gc() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    setup_stack_with_advanced_main(root);

    kin_cmd().current_dir(root).arg("sync").assert().success();
    let post_b = branch_oid(root, "feature-b").unwrap();

    kin_cmd().current_dir(root).arg("undo").assert().success();

    // The post-sync commits are now only reachable via the anchor refs.
    run_ok("git", &["reflog", "expire", "--expire=now", "--all"], root);
    run_ok("git", &["gc", "--prune=now"], root);

    // Redo still works because the anchors kept the objects alive.
    kin_cmd().current_dir(root).arg("redo").assert().success();
    assert_eq!(
        branch_oid(root, "feature-b").as_deref(),
        Some(post_b.as_str())
    );
}

#[test]
fn undo_refuses_when_branch_drifted() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    setup_stack_with_advanced_main(root);

    kin_cmd().current_dir(root).arg("sync").assert().success();

    // Add a commit after the sync: the stack has drifted.
    run_ok("git", &["commit", "--allow-empty", "-m", "drift"], root);
    let drifted = branch_oid(root, "feature-b").unwrap();

    kin_cmd()
        .current_dir(root)
        .arg("undo")
        .assert()
        .failure()
        .stderr(predicate::str::contains("changed since the operation"));

    // The drift commit is untouched.
    assert_eq!(
        branch_oid(root, "feature-b").as_deref(),
        Some(drifted.as_str())
    );

    // --force overrides the guard.
    kin_cmd()
        .current_dir(root)
        .args(["undo", "--force"])
        .assert()
        .success();
    assert_ne!(
        branch_oid(root, "feature-b").as_deref(),
        Some(drifted.as_str())
    );
}

#[test]
fn undo_recreates_merged_branch_deleted_by_sync() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_with_base_commit(root);

    run_ok("git", &["checkout", "-b", "feature-a"], root);
    fs::write(root.join("a.txt"), "a").unwrap();
    run_ok("git", &["add", "."], root);
    run_ok("git", &["commit", "-m", "a1"], root);

    // Merge feature-a into main so sync treats it as merged and deletes it.
    run_ok("git", &["checkout", "main"], root);
    run_ok(
        "git",
        &["merge", "--no-ff", "feature-a", "-m", "merge a"],
        root,
    );

    // A child branch keeps the stack non-empty.
    run_ok("git", &["checkout", "-b", "feature-b", "feature-a"], root);
    fs::write(root.join("b.txt"), "b").unwrap();
    run_ok("git", &["add", "."], root);
    run_ok("git", &["commit", "-m", "b1"], root);

    let pre_a = branch_oid(root, "feature-a").unwrap();

    kin_cmd().current_dir(root).arg("sync").assert().success();
    assert!(
        branch_oid(root, "feature-a").is_none(),
        "sync should have deleted the merged branch"
    );

    kin_cmd().current_dir(root).arg("undo").assert().success();
    assert_eq!(
        branch_oid(root, "feature-a").as_deref(),
        Some(pre_a.as_str()),
        "undo should recreate the deleted branch at its old tip"
    );
}

#[test]
fn abort_leaves_no_undo_entry() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_with_base_commit(root);

    run_ok("git", &["checkout", "-b", "feature-x"], root);
    fs::write(root.join("shared.txt"), "x").unwrap();
    run_ok("git", &["add", "."], root);
    run_ok("git", &["commit", "-m", "x"], root);

    // Conflicting change on main so sync stops on a conflict.
    run_ok("git", &["checkout", "main"], root);
    fs::write(root.join("shared.txt"), "m").unwrap();
    run_ok("git", &["add", "."], root);
    run_ok("git", &["commit", "-m", "m"], root);
    run_ok("git", &["checkout", "feature-x"], root);

    kin_cmd().current_dir(root).arg("sync").assert().failure();
    kin_cmd().current_dir(root).arg("abort").assert().success();

    // Nothing recorded, because abort restored the pre-operation refs.
    kin_cmd()
        .current_dir(root)
        .arg("reflog")
        .assert()
        .success()
        .stdout(predicate::str::contains("No operations recorded yet."));
}

#[test]
fn reflog_lists_recorded_operations() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    setup_stack_with_advanced_main(root);

    kin_cmd().current_dir(root).arg("sync").assert().success();

    kin_cmd()
        .current_dir(root)
        .arg("reflog")
        .assert()
        .success()
        .stdout(predicate::str::contains("sync"))
        .stdout(predicate::str::contains("rebased"));
}

#[test]
fn reflog_refuses_when_operation_in_progress() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    setup_stack_with_advanced_main(root);

    // Simulate an operation in progress plus a pending oplog snapshot.
    let git_dir = root.join(".git");
    fs::write(git_dir.join("kindra_run_state.json"), "{}").unwrap();
    let pending = git_dir.join("kindra_oplog_pending.json");
    fs::write(&pending, "pending-snapshot").unwrap();

    // reflog must refuse rather than finalize the half-applied state.
    kin_cmd()
        .current_dir(root)
        .arg("reflog")
        .assert()
        .failure()
        .stderr(predicate::str::contains("operation is in progress"));

    // The pending snapshot is untouched (not consumed into a bogus entry).
    assert!(
        pending.exists(),
        "pending snapshot must survive a refused reflog"
    );
}

#[test]
fn sync_up_to_date_leaves_no_pending_snapshot() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_with_base_commit(root);

    // On the base branch with nothing to do, sync takes the synchronous
    // (non-rebase) path.
    kin_cmd().current_dir(root).arg("sync").assert().success();

    // The pending snapshot begun by sync must be finalized/dropped here, not
    // left dangling to fold later manual work into a bogus "sync" entry.
    assert!(
        !pending_snapshot_exists(root),
        "sync must not leave a dangling oplog pending snapshot"
    );
}

/// The pending snapshot lives at this path while an operation is mid-flight.
fn pending_snapshot_exists(root: &Path) -> bool {
    root.join(".git/kindra_oplog_pending.json").exists()
}

/// A single-commit `main`-only repo: the minimal setup for the no-op / early
/// exit paths that must not leave a pending snapshot behind.
fn init_with_base_commit(root: &Path) {
    repo_init(root);
    fs::write(root.join("base.txt"), "base").unwrap();
    run_ok("git", &["add", "."], root);
    run_ok("git", &["commit", "-m", "base"], root);
}

#[test]
fn reorder_settles_snapshot_on_success() {
    // A real reorder runs `begin` and rewrites branch tips, so the guard must
    // settle the snapshot into a finalized (undoable) entry rather than leave a
    // dangling pending file. Driving an actual reorder is what exercises the
    // guard — a path that bails before `begin` could never leave one anyway.
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_with_base_commit(root);

    for (branch, file) in [
        ("feature-a", "a.txt"),
        ("feature-b", "b.txt"),
        ("feature-c", "c.txt"),
    ] {
        run_ok("git", &["checkout", "-b", branch], root);
        fs::write(root.join(file), file).unwrap();
        run_ok("git", &["add", "."], root);
        run_ok("git", &["commit", "-m", branch], root);
    }
    run_ok("git", &["checkout", "-f", "feature-a"], root);

    // Reorder the stack to main -> feature-c -> feature-a -> feature-b.
    let editor = root.join("reorder-editor.sh");
    fs::write(
        &editor,
        "#!/bin/sh\n{\n  echo \"branch feature-c parent main\"\n  echo \"branch feature-a\"\n  echo \"branch feature-b\"\n} > \"$1\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
    }

    kin_cmd()
        .current_dir(root)
        .arg("reorder")
        .env("GIT_EDITOR", &editor)
        .assert()
        .success();

    assert!(
        !pending_snapshot_exists(root),
        "a successful reorder must settle its snapshot, not leave it pending"
    );
    // Settled means finalized into an undoable entry, not vacuously absent.
    kin_cmd().current_dir(root).arg("undo").assert().success();
}

#[test]
fn restack_settles_snapshot_on_success() {
    // A real restack (a floating child reparented onto an amended main) runs
    // `begin` and rewrites a tip, so the guard must settle the snapshot into a
    // finalized (undoable) entry rather than leave a dangling pending file.
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_with_base_commit(root);

    // feature branches off main, then main is amended so feature floats.
    run_ok("git", &["checkout", "-b", "feature"], root);
    fs::write(root.join("f.txt"), "f").unwrap();
    run_ok("git", &["add", "."], root);
    run_ok("git", &["commit", "-m", "feature"], root);

    run_ok("git", &["checkout", "main"], root);
    fs::write(root.join("base.txt"), "base-amended").unwrap();
    run_ok("git", &["add", "."], root);
    run_ok("git", &["commit", "--amend", "-m", "base"], root);

    kin_cmd()
        .current_dir(root)
        .arg("restack")
        .assert()
        .success();

    assert!(
        !pending_snapshot_exists(root),
        "a successful restack must settle its snapshot, not leave it pending"
    );
    // Settled means finalized into an undoable entry, not vacuously absent.
    kin_cmd().current_dir(root).arg("undo").assert().success();
}

#[test]
fn split_no_op_leaves_no_pending_snapshot() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_with_base_commit(root);

    // On the base branch with no stack, split begins its snapshot and then hits
    // the "no commits to manage" no-op: the guard must drop the snapshot on exit.
    kin_cmd()
        .current_dir(root)
        .arg("split")
        .assert()
        .success()
        .stdout(predicate::str::contains("No commits to manage"));
    assert!(
        !pending_snapshot_exists(root),
        "a no-op split must not leave a pending snapshot"
    );
}

#[test]
fn move_settles_snapshot_on_success() {
    // A real move runs `begin` and rewrites a tip, so the guard must settle the
    // snapshot into a finalized (undoable) entry rather than leave a dangling
    // pending file.
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_with_base_commit(root);

    run_ok("git", &["checkout", "-b", "feature-a"], root);
    fs::write(root.join("a.txt"), "a").unwrap();
    run_ok("git", &["add", "."], root);
    run_ok("git", &["commit", "-m", "a1"], root);

    run_ok("git", &["checkout", "-b", "feature-b"], root);
    fs::write(root.join("b.txt"), "b").unwrap();
    run_ok("git", &["add", "."], root);
    run_ok("git", &["commit", "-m", "b1"], root);

    // Move feature-b off feature-a directly onto main.
    kin_cmd()
        .current_dir(root)
        .args(["move", "--onto", "main"])
        .assert()
        .success();

    assert!(
        !pending_snapshot_exists(root),
        "a successful move must settle its snapshot, not leave it pending"
    );
    // Settled means finalized into an undoable entry, not vacuously absent.
    kin_cmd().current_dir(root).arg("undo").assert().success();
}

#[test]
fn sync_error_after_begin_leaves_no_pending_snapshot() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    setup_stack_with_advanced_main(root);

    // Check out a stack branch in a second worktree so sync's `check_worktrees`
    // preflight fails — an error path *after* `begin` has taken the snapshot.
    let wt = tempdir().unwrap();
    run_ok(
        "git",
        &["worktree", "add", wt.path().to_str().unwrap(), "feature-a"],
        root,
    );

    kin_cmd().current_dir(root).arg("sync").assert().failure();

    // The Drop guard must settle (drop) the snapshot even though sync failed
    // after taking it.
    assert!(
        !pending_snapshot_exists(root),
        "sync failing after begin must not leave a pending snapshot"
    );
    // And nothing was recorded, since no branch actually moved.
    kin_cmd()
        .current_dir(root)
        .arg("reflog")
        .assert()
        .success()
        .stdout(predicate::str::contains("No operations recorded yet."));
}

#[test]
fn sync_delete_hint_uses_full_oid() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_with_base_commit(root);

    run_ok("git", &["checkout", "-b", "feature-a"], root);
    fs::write(root.join("a.txt"), "a").unwrap();
    run_ok("git", &["add", "."], root);
    run_ok("git", &["commit", "-m", "a1"], root);

    // Merge feature-a into main so sync treats it as merged and deletes it.
    run_ok("git", &["checkout", "main"], root);
    run_ok(
        "git",
        &["merge", "--no-ff", "feature-a", "-m", "merge a"],
        root,
    );

    // A child branch keeps the stack non-empty so sync runs to the delete step.
    run_ok("git", &["checkout", "-b", "feature-b", "feature-a"], root);
    fs::write(root.join("b.txt"), "b").unwrap();
    run_ok("git", &["add", "."], root);
    run_ok("git", &["commit", "-m", "b1"], root);

    let pre_a = branch_oid(root, "feature-a").unwrap();
    assert_eq!(pre_a.len(), 40, "full OID should be 40 hex chars");

    let output = kin_cmd().current_dir(root).arg("sync").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // The recovery command must use the full, unambiguous OID.
    assert!(
        stdout.contains(&format!("git branch feature-a {pre_a}")),
        "restore hint must use the full OID; got:\n{stdout}"
    );
}

#[test]
fn redo_refuses_to_clobber_untracked_without_force() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    setup_stack_with_advanced_main(root);

    kin_cmd().current_dir(root).arg("sync").assert().success();
    let post_b = branch_oid(root, "feature-b").unwrap();

    // Back to the pre-sync state; the working tree no longer contains m.txt.
    kin_cmd().current_dir(root).arg("undo").assert().success();

    // An untracked file obstructs the commit redo would check out (it tracks
    // m.txt). `working_tree_dirty` ignores untracked files, so only a non-force
    // checkout protects it.
    fs::write(root.join("m.txt"), "untracked-precious").unwrap();

    kin_cmd().current_dir(root).arg("redo").assert().failure();
    assert_eq!(
        fs::read_to_string(root.join("m.txt")).unwrap(),
        "untracked-precious",
        "redo without --force must not clobber the untracked file"
    );

    // --force is the explicit opt-in to discard the obstruction.
    kin_cmd()
        .current_dir(root)
        .args(["redo", "--force"])
        .assert()
        .success();
    assert_eq!(
        branch_oid(root, "feature-b").as_deref(),
        Some(post_b.as_str())
    );
    assert_eq!(fs::read_to_string(root.join("m.txt")).unwrap(), "m");
}

#[test]
fn undo_handles_prefix_conflicting_branch_refs() {
    // A recorded operation that renames `foo` -> `foo/bar` produces a change set
    // where one ref must be deleted and a prefix-conflicting one created. Git
    // stores refs as files, so `refs/heads/foo` and `refs/heads/foo/bar` cannot
    // coexist: restore must delete before it creates or it hits a lock conflict.
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_with_base_commit(root);

    // `foo` one commit ahead of main; HEAD stays on main so neither rename side
    // is the checked-out branch.
    run_ok("git", &["checkout", "-b", "foo"], root);
    fs::write(root.join("foo.txt"), "foo").unwrap();
    run_ok("git", &["add", "."], root);
    run_ok("git", &["commit", "-m", "foo1"], root);
    run_ok("git", &["checkout", "main"], root);
    let foo_tip = branch_oid(root, "foo").unwrap();

    // Record a rename as a real oplog entry: snapshot, mutate refs, then let the
    // guard settle on drop (recording the entry) at the end of the block.
    {
        let repo = Repository::open(root).unwrap();
        let _snapshot = kindra::oplog::begin(&repo, "split").unwrap();
        run_ok("git", &["branch", "-D", "foo"], root);
        run_ok("git", &["branch", "foo/bar", &foo_tip], root);
    }

    // Undo must recreate `foo` and delete the prefix-conflicting `foo/bar`.
    kin_cmd().current_dir(root).arg("undo").assert().success();
    assert_eq!(branch_oid(root, "foo").as_deref(), Some(foo_tip.as_str()));
    assert!(branch_oid(root, "foo/bar").is_none());

    // Redo reapplies the rename, exercising the delete-first order the other way.
    kin_cmd().current_dir(root).arg("redo").assert().success();
    assert!(branch_oid(root, "foo").is_none());
    assert_eq!(
        branch_oid(root, "foo/bar").as_deref(),
        Some(foo_tip.as_str())
    );
}

#[test]
fn abort_with_divergent_state_finalizes_oplog_for_recovery() {
    // When abort clears divergent Kindra state WITHOUT restoring refs, the
    // operation's effects are still live, so its pending oplog snapshot must be
    // finalized (recorded, undoable) rather than discarded.
    let dir = tempdir().unwrap();
    let root = dir.path();
    init_with_base_commit(root);

    // Snapshot via a real begin, then advance the tip so finalize sees a change.
    // Hold the guard for the rest of the test so the pending snapshot survives to
    // the `abort` below, simulating a process that paused mid-operation.
    let repo = Repository::open(root).unwrap();
    let _guard = kindra::oplog::begin(&repo, "sync").unwrap();
    run_ok("git", &["commit", "--allow-empty", "-m", "advance"], root);

    // A divergent saved rebase state (all-zeros owned_tip_map can't match the
    // repo), so abort clears Kindra state without restoring refs. Build it from
    // the real `RebaseState` type so the fixture tracks the schema instead of a
    // hand-copied JSON snapshot.
    let state = RebaseState {
        operation: Operation::Commit,
        original_branch: "main".to_string(),
        target_branch: "main".to_string(),
        owned_tip_map: HashMap::from([(
            "main".to_string(),
            "0000000000000000000000000000000000000000".to_string(),
        )]),
        caller_branch: None,
        remaining_branches: Vec::new(),
        in_progress_branch: None,
        parent_id_map: HashMap::new(),
        parent_name_map: HashMap::new(),
        new_base_map: HashMap::new(),
        original_commit_count_map: HashMap::new(),
        original_tip_map: HashMap::new(),
        stash_ref: None,
        unstage_on_restore: false,
        autostash: false,
        cleanup_merged_branches: Vec::new(),
        cleanup_checkout_fallback: None,
    };
    fs::write(
        root.join(".git/kindra_rebase_state.json"),
        serde_json::to_string_pretty(&state).unwrap(),
    )
    .unwrap();

    kin_cmd().current_dir(root).arg("abort").assert().success();

    // reflog must show the recorded op — discard would have wiped it.
    let out = kin_cmd().current_dir(root).arg("reflog").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("sync"),
        "abort on divergent state must finalize (not discard) the oplog. Got:\n{}",
        stdout
    );
}
