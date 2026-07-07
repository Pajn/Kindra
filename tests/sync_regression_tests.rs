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

/// When the tips point at *different* commits (a real fork above a shared
/// branch), the choice matters and sync must still prompt. In a non-interactive
/// run that prompt surfaces as a hard error rather than silently picking a side.
#[test]
fn sync_still_prompts_when_tips_diverge() {
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
    cmd.arg("sync")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("Multiple stack tips found"));
}
