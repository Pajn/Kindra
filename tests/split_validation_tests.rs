mod common;

use common::{gits_cmd, make_commit, repo_init};
use git2::Repository;
use std::fs;
use tempfile::tempdir;

fn setup_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());
    repo.set_head("refs/heads/main").unwrap();

    let parent_id = make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "initial",
        "initial commit",
        &[],
    );

    let first_commit_id = parent_id;
    let mut current_parent_id = parent_id;

    // Create a stack of 3 commits
    for i in 1..=3 {
        let parent = repo.find_commit(current_parent_id).unwrap();
        current_parent_id = make_commit(
            &repo,
            "HEAD",
            &format!("file{}.txt", i),
            &format!("content {}", i),
            &format!("commit {}", i),
            &[&parent],
        );
    }

    // Detach HEAD
    repo.set_head_detached(current_parent_id).unwrap();

    // Reset main
    {
        let first_commit = repo.find_commit(first_commit_id).unwrap();
        repo.branch("main", &first_commit, true).unwrap();
    }

    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    (dir, repo)
}

#[test]
fn test_split_preflight_failure_prevents_deletion() {
    let (dir, repo) = setup_repo();

    // 1. Create a branch that should be deleted if split succeeds
    {
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("to-delete", &head, false).unwrap();
    }
    repo.set_head("refs/heads/to-delete").unwrap();

    // 2. Prepare editor script that:
    //    - Removes "to-delete" branch
    //    - Adds an invalid branch name that will cause find_branch or similar to fail in pre-flight
    //      (or we can use a duplicate branch name which split() catches, but we want to hit apply_split)
    // Actually, if we use a branch name that already exists but is unsafe and auto-deny, it skips.
    // If we want it to FAIL, we need an Err.

    // Let's try to make revparse_single fail in apply_split by some trick?
    // Hard.

    // What if we use a branch name that is definitely invalid and see if git2 errors?
    // Git branch names cannot contain '..'
    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
# Remove to-delete
perl -i -pe 's/.*branch to-delete.*
?//g' "$file"
# Add invalid branch name
perl -i -pe 's/(commit 1)/$1
branch inv..alid/' "$file"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&editor_script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&editor_script, perms).unwrap();
    }

    let mut cmd = gits_cmd();
    let output = cmd
        .arg("split")
        .current_dir(dir.path())
        .env("EDITOR", &editor_script)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "split should fail on invalid branch name"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("Stderr: {}", stderr);

    // 3. Verify that "to-delete" STILL EXISTS
    // If the pre-flight worked, it should have failed before deleting "to-delete".
    assert!(
        repo.find_branch("to-delete", git2::BranchType::Local)
            .is_ok(),
        "Branch 'to-delete' should still exist because split failed in pre-flight"
    );
}

#[test]
fn test_split_skip_unsafe_branch() {
    let (dir, repo) = setup_repo();

    // Create a branch outside the stack
    {
        let main = repo.find_branch("main", git2::BranchType::Local).unwrap();
        let main_commit = main.get().peel_to_commit().unwrap();
        repo.branch("outside", &main_commit, false).unwrap();
    }

    // Create a branch in the stack
    {
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("in-stack", &head, false).unwrap();
    }
    repo.set_head("refs/heads/in-stack").unwrap();

    // Editor script: move "in-stack" to "outside" (which exists and is unsafe)
    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
perl -i -pe 's/branch in-stack/branch outside/' "$file"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&editor_script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&editor_script, perms).unwrap();
    }

    // Run in non-interactive mode (should auto-deny and skip)
    let mut cmd = gits_cmd();
    cmd.arg("split")
        .current_dir(dir.path())
        .env("EDITOR", &editor_script)
        .env("TERM", "dumb")
        .assert()
        .success()
        .stdout(predicates::str::contains("Skipping branch 'outside'"));

    // Verify "outside" still points to main (not the new commit)
    let outside = repo
        .find_branch("outside", git2::BranchType::Local)
        .unwrap();
    let target = outside.get().target().unwrap();
    let commit = repo.find_commit(target).unwrap();
    assert_eq!(commit.summary().unwrap(), "initial commit");

    // Verify "in-stack" was DELETED (because it was removed from the editor)
    assert!(
        repo.find_branch("in-stack", git2::BranchType::Local)
            .is_err()
    );
}
