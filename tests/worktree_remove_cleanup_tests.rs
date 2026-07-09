mod common;

use common::{
    branch_exists, kin_cmd, managed_worktree_path, run_ok, setup_worktree_repo, write_repo_config,
};
use std::fs;
use std::path::Path;

fn worktree_git_dir(worktree_path: &Path) -> std::path::PathBuf {
    let dot_git = worktree_path.join(".git");
    if dot_git.is_dir() {
        return dot_git;
    }

    let raw = fs::read_to_string(&dot_git).unwrap();
    let gitdir = raw
        .strip_prefix("gitdir: ")
        .expect("worktree .git file should start with gitdir:")
        .trim();
    let gitdir_path = std::path::PathBuf::from(gitdir);
    if gitdir_path.is_absolute() {
        gitdir_path
    } else {
        worktree_path.join(gitdir_path)
    }
}

#[test]
fn worktree_remove_prompts_by_default_and_removes_with_yes() {
    let dir = setup_worktree_repo();
    let temp_path = dir.path().join(".git/kindra-worktrees/temp/feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );

    let output = kin_cmd()
        .args(["wt", "remove", "feature-a"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("non-interactive: declining"));
    assert!(temp_path.exists());

    let output = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!temp_path.exists());

    // Non-merged temp: remove keeps the branch by default (only deletes if --with-branch or if merged).
    assert!(
        branch_exists(dir.path(), "feature-a"),
        "non-merged branch should remain after plain remove"
    );
}

#[test]
fn worktree_cleanup_removes_merged_temp_worktrees_but_not_persistent_ones() {
    let dir = setup_worktree_repo();
    let main_path = dir.path().join(".git/kindra-worktrees/main");
    let temp_path = dir.path().join(".git/kindra-worktrees/temp/feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "main"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );

    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());

    let output = kin_cmd()
        .args(["wt", "cleanup", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(main_path.exists());
    assert!(!temp_path.exists());

    // By default, cleanup deletes the merged branch for temp worktrees.
    assert!(
        !branch_exists(dir.path(), "feature-a"),
        "merged temp branch should have been deleted by default"
    );
}

#[test]
fn worktree_remove_requires_force_for_dirty_worktrees_even_with_yes() {
    let dir = setup_worktree_repo();
    let temp_path = managed_worktree_path(dir.path(), "temp/feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    fs::write(temp_path.join("dirty.txt"), "dirty").unwrap();

    let output = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Re-run with --force to remove it."));
    assert!(temp_path.exists());

    let output = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes", "--force"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!temp_path.exists());
}

#[test]
fn worktree_remove_requires_force_for_incomplete_git_operations() {
    let dir = setup_worktree_repo();
    let temp_path = managed_worktree_path(dir.path(), "temp/feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );

    fs::write(dir.path().join("file.txt"), "main change").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok("git", &["commit", "-m", "main change"], dir.path());

    fs::write(temp_path.join("file.txt"), "feature change").unwrap();
    run_ok("git", &["add", "file.txt"], &temp_path);
    run_ok("git", &["commit", "-m", "feature change"], &temp_path);

    let merge_output = std::process::Command::new("git")
        .args(["merge", "main"])
        .current_dir(&temp_path)
        .output()
        .unwrap();
    assert!(
        !merge_output.status.success(),
        "expected merge conflict\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&merge_output.stdout),
        String::from_utf8_lossy(&merge_output.stderr),
    );
    assert!(worktree_git_dir(&temp_path).join("MERGE_HEAD").exists());

    let output = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Re-run with --force to remove it."));
    assert!(temp_path.exists());

    let output = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes", "--force"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!temp_path.exists());
}

#[test]
fn worktree_cleanup_yes_skips_dirty_candidates_without_force() {
    let dir = setup_worktree_repo();
    let temp_path = managed_worktree_path(dir.path(), "temp/feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );

    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());
    fs::write(temp_path.join("dirty.txt"), "dirty").unwrap();

    let output = kin_cmd()
        .args(["wt", "cleanup", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Skipping dirty temp worktree 'feature-a'"));
    assert!(stdout.contains("found 1 temp worktree candidate(s), removed 0, skipped 1"));
    assert!(temp_path.exists());

    let output = kin_cmd()
        .args(["wt", "cleanup", "--yes", "--force"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!temp_path.exists());

    // With default behavior, the merged branch was also deleted.
    assert!(!branch_exists(dir.path(), "feature-a"));
}

#[test]
fn worktree_remove_rejects_missing_targets_even_with_yes() {
    let dir = setup_worktree_repo();

    for (target, expected) in [
        ("main", "No managed main worktree exists."),
        ("review", "No managed review worktree exists."),
        ("feature-a", "No worktree found for branch 'feature-a'."),
    ] {
        let output = kin_cmd()
            .args(["wt", "remove", target, "--yes"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(!output.status.success(), "unexpected success for {target}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
    }
}

#[test]
fn cleanup_deletes_branch_by_default_and_keep_branch_leaves_it() {
    let dir = setup_worktree_repo();
    let temp_path = managed_worktree_path(dir.path(), "temp/feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());

    // Default: deletes branch
    let out = kin_cmd()
        .args(["wt", "cleanup", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!temp_path.exists());
    assert!(
        !branch_exists(dir.path(), "feature-a"),
        "branch should be deleted by default on cleanup"
    );

    // Create a fresh branch with a commit (feature-a branch was deleted by previous cleanup)
    run_ok("git", &["checkout", "-b", "feature-b"], dir.path());
    fs::write(dir.path().join("b.txt"), "b").unwrap();
    run_ok("git", &["add", "b.txt"], dir.path());
    run_ok("git", &["commit", "-m", "b work"], dir.path());
    run_ok("git", &["checkout", "main"], dir.path());

    // Now use the new merged temp and test --keep-branch
    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-b"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    run_ok("git", &["merge", "--ff-only", "feature-b"], dir.path());
    let temp_b = managed_worktree_path(dir.path(), "temp/feature-b");

    let out = kin_cmd()
        .args(["wt", "cleanup", "--yes", "--keep-branch"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!temp_b.exists());
    assert!(
        branch_exists(dir.path(), "feature-b"),
        "--keep-branch should have left the branch"
    );
}

#[test]
fn remove_deletes_branch_by_default_only_if_merged() {
    let dir = setup_worktree_repo();

    // Non-merged: --with-branch is required to delete
    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    let out = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(branch_exists(dir.path(), "feature-a"));

    // Make it merged, remove should delete by default now
    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());
    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    let out = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        !branch_exists(dir.path(), "feature-a"),
        "merged branch should be auto-deleted on remove"
    );
}

#[test]
fn remove_with_keep_branch_and_with_branch() {
    let dir = setup_worktree_repo();
    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());
    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );

    // --keep-branch even on merged
    let out = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes", "--keep-branch"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(branch_exists(dir.path(), "feature-a"));

    // recreate and use explicit --with-branch (though default would too)
    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    let out = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes", "--with-branch"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!branch_exists(dir.path(), "feature-a"));
}

#[test]
fn remove_respects_delete_merged_false() {
    let dir = setup_worktree_repo();
    let temp_path = managed_worktree_path(dir.path(), "temp/feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());

    // disable auto branch deletion
    write_repo_config(dir.path(), "[worktrees.temp]\ndelete_merged = false\n");

    let out = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!temp_path.exists());

    // branch should still exist because delete_merged=false
    assert!(
        branch_exists(dir.path(), "feature-a"),
        "branch should remain when delete_merged=false"
    );
}

#[test]
fn remove_refuses_trunk_branch_even_with_with_branch() {
    let dir = setup_worktree_repo();
    let temp_path = managed_worktree_path(dir.path(), "temp/feature-a");

    // set trunk to feature-a so it is the protected trunk branch
    write_repo_config(dir.path(), "[worktrees]\ntrunk = \"feature-a\"\n");

    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );

    // even explicit --with-branch on the trunk branch name must be refused
    let out = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes", "--with-branch"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Refusing to delete the trunk branch 'feature-a'"));

    // the worktree should not have been removed (refusal happens before)
    assert!(temp_path.exists());
}

#[test]
fn remove_with_branch_on_review_deletes_branch() {
    let dir = setup_worktree_repo();
    let review_path = managed_worktree_path(dir.path(), "review");

    assert!(
        kin_cmd()
            .args(["wt", "review", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );

    let out = kin_cmd()
        .args(["wt", "remove", "review", "--yes", "--with-branch"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!review_path.exists());
    assert!(
        !branch_exists(dir.path(), "feature-a"),
        "explicit --with-branch on review should delete the branch"
    );
}

#[test]
fn remove_succeeds_when_branch_tip_unavailable() {
    let dir = setup_worktree_repo();
    let temp_path = managed_worktree_path(dir.path(), "temp/feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());

    // Drop the branch ref while the worktree still exists. Tip capture should
    // fail gracefully and the worktree should still be removed.
    run_ok(
        "git",
        &["update-ref", "-d", "refs/heads/feature-a"],
        dir.path(),
    );

    let out = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes", "--force"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "remove should succeed even when branch tip cannot be captured\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!temp_path.exists());
}

#[test]
fn remove_review_does_not_auto_delete_merged_branch() {
    let dir = setup_worktree_repo();
    let review_path = managed_worktree_path(dir.path(), "review");

    // create review on feature-a, then merge it so it would be candidate for auto-delete
    assert!(
        kin_cmd()
            .args(["wt", "review", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());

    // remove the review worktree (default, no --with-branch)
    let out = kin_cmd()
        .args(["wt", "remove", "review", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!review_path.exists());

    // the branch should remain (review is persistent role, no auto-delete)
    assert!(
        branch_exists(dir.path(), "feature-a"),
        "review worktree remove should not auto-delete merged branch"
    );
}

#[test]
fn remove_with_branch_refuses_when_branch_checked_out_elsewhere() {
    let dir = setup_worktree_repo();
    let temp_path = managed_worktree_path(dir.path(), "temp/feature-a");
    let other_path = dir.path().join("other-feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    run_ok(
        "git",
        &[
            "worktree",
            "add",
            "--force",
            other_path.to_str().unwrap(),
            "feature-a",
        ],
        dir.path(),
    );

    let out = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes", "--with-branch"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("checked out in another worktree"));
    assert!(stderr.contains("Remove or switch that worktree"));
    assert!(!stderr.contains("Re-run with --force"));
    assert!(temp_path.exists());

    let out = kin_cmd()
        .args([
            "wt",
            "remove",
            "feature-a",
            "--yes",
            "--with-branch",
            "--force",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Remove or switch that worktree"));
    assert!(temp_path.exists());

    run_ok(
        "git",
        &[
            "worktree",
            "remove",
            "--force",
            other_path.to_str().unwrap(),
        ],
        dir.path(),
    );
}

#[test]
fn remove_auto_delete_skips_branch_when_checked_out_elsewhere() {
    let dir = setup_worktree_repo();
    let temp_path = managed_worktree_path(dir.path(), "temp/feature-a");
    let other_path = dir.path().join("other-feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());
    run_ok(
        "git",
        &[
            "worktree",
            "add",
            "--force",
            other_path.to_str().unwrap(),
            "feature-a",
        ],
        dir.path(),
    );

    let out = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("Skipping branch delete for 'feature-a'"));
    assert!(!temp_path.exists());
    assert!(
        branch_exists(dir.path(), "feature-a"),
        "branch should remain while checked out in another worktree"
    );

    run_ok(
        "git",
        &[
            "worktree",
            "remove",
            "--force",
            other_path.to_str().unwrap(),
        ],
        dir.path(),
    );
}

#[test]
fn cleanup_skips_branch_delete_when_branch_checked_out_elsewhere() {
    let dir = setup_worktree_repo();
    let temp_path = managed_worktree_path(dir.path(), "temp/feature-a");
    let other_path = dir.path().join("other-feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());
    run_ok(
        "git",
        &[
            "worktree",
            "add",
            "--force",
            other_path.to_str().unwrap(),
            "feature-a",
        ],
        dir.path(),
    );

    let out = kin_cmd()
        .args(["wt", "cleanup", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("checked out elsewhere"));
    assert!(combined.contains("Skipping branch delete for 'feature-a'"));
    assert!(!temp_path.exists());
    assert!(
        branch_exists(dir.path(), "feature-a"),
        "branch should remain while checked out in another worktree"
    );

    run_ok(
        "git",
        &[
            "worktree",
            "remove",
            "--force",
            other_path.to_str().unwrap(),
        ],
        dir.path(),
    );
}

#[test]
fn remove_keep_branch_does_not_emit_cross_worktree_skip_message() {
    let dir = setup_worktree_repo();

    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());

    let out = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes", "--keep-branch"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !combined.contains("Skipping branch delete"),
        "keep-branch removes should not emit branch-delete skip messages:\n{combined}"
    );
    assert!(branch_exists(dir.path(), "feature-a"));
}
