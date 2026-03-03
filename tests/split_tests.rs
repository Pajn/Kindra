mod common;

use common::{gits_cmd, make_commit};
use git2::{Repository, Signature};
use std::fs;
use tempfile::tempdir;

fn setup_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
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
            "HEAD", // commit to HEAD (detached later)
            &format!("file{}.txt", i),
            &format!("content {}", i),
            &format!("commit {}", i),
            &[&parent],
        );
    }

    // Detach HEAD before moving main
    repo.set_head_detached(current_parent_id).unwrap();

    {
        // Reset main to the first commit
        let first_commit = repo.find_commit(first_commit_id).unwrap();
        repo.branch("main", &first_commit, true).unwrap();
    }

    {
        // Clean up working directory to avoid checkout conflicts
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
    }

    (dir, repo)
}

#[test]
fn test_split_move_branch() {
    let (dir, repo) = setup_repo();

    // Create an initial branch at the tip
    {
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature-x", &head, false).unwrap();
    }
    repo.set_head("refs/heads/feature-x").unwrap();

    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
perl -i -pe 's/.*branch feature-x.*\n?//g' "$file"
perl -i -pe 's/(commit 2)/$1\nbranch feature-x/' "$file"
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
    cmd.arg("split")
        .current_dir(dir.path())
        .env("EDITOR", &editor_script)
        .assert()
        .success();

    // Verify branch moved
    let branch = repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap();
    let target = branch.get().target().unwrap();
    let commit = repo.find_commit(target).unwrap();
    assert_eq!(commit.summary().unwrap(), "commit 2");
}

#[test]
fn test_split_create_delete_branch() {
    let (dir, repo) = setup_repo();

    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
perl -i -pe 's/(commit 1)/$1\nbranch new-feat/' "$file"
perl -i -pe 's/(commit 3)/$1\nbranch another-feat/' "$file"
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
    cmd.arg("split")
        .current_dir(dir.path())
        .env("EDITOR", &editor_script)
        .assert()
        .success();

    assert!(
        repo.find_branch("new-feat", git2::BranchType::Local)
            .is_ok()
    );
    assert!(
        repo.find_branch("another-feat", git2::BranchType::Local)
            .is_ok()
    );

    // Now delete 'new-feat'
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
perl -i -pe 's/.*branch new-feat.*\n?//g' "$file"
"#,
    )
    .unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("split")
        .current_dir(dir.path())
        .env("EDITOR", &editor_script)
        .assert()
        .success();

    assert!(
        repo.find_branch("new-feat", git2::BranchType::Local)
            .is_err()
    );
    assert!(
        repo.find_branch("another-feat", git2::BranchType::Local)
            .is_ok()
    );
}

#[test]
fn test_split_error_on_commit_mod() {
    let (dir, _repo) = setup_repo();

    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
perl -i -pe 's/^[0-9a-f]{7}/deadbee/' "$file"
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
    cmd.arg("split")
        .current_dir(dir.path())
        .env("EDITOR", &editor_script)
        .assert()
        .failure()
        .stderr(predicates::str::contains("modified or moved"));
}

#[test]
fn test_split_detach_head_on_delete() {
    let (dir, repo) = setup_repo();

    {
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("current", &head, false).unwrap();
    }
    repo.set_head("refs/heads/current").unwrap();

    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
perl -i -pe 's/.*branch current.*\n?//g' "$file"
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
    cmd.arg("split")
        .current_dir(dir.path())
        .env("EDITOR", &editor_script)
        .assert()
        .success();

    assert!(repo.head_detached().unwrap());
    assert!(
        repo.find_branch("current", git2::BranchType::Local)
            .is_err()
    );
}

#[test]
fn test_push_multiple_remotes_no_origin_error() {
    let (dir, repo) = setup_repo();

    // Setup two remotes, neither is origin
    repo.remote("remote1", "http://example.com/r1.git").unwrap();
    repo.remote("remote2", "http://example.com/r2.git").unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("push")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "'origin' remote not found and multiple remotes exist",
        ));
}

#[test]
fn test_push_no_remotes_error() {
    let (dir, _repo) = setup_repo();
    // No remotes by default from setup_repo (except if we added any)

    let mut cmd = gits_cmd();
    cmd.arg("push")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("No remotes configured"));
}

#[test]
fn test_checkout_up_fork() {
    let (dir, repo) = setup_repo();

    // c1 is an ancestor.
    // We want to be on a branch at c1, and have two successors.
    let c1_id = repo.revparse_single("HEAD~2").unwrap().id();
    let c2_id = repo.revparse_single("HEAD~1").unwrap().id();
    let head_id = repo.head().unwrap().peel_to_commit().unwrap().id();

    // Create two independent paths from c1
    {
        let c1 = repo.find_commit(c1_id).unwrap();
        let c2 = repo.find_commit(c2_id).unwrap();
        let head = repo.find_commit(head_id).unwrap();

        // fork-a is head (descendant of head_id)
        repo.branch("fork-a", &head, false).unwrap();

        // fork-b is a NEW commit from c1
        let tree = c2.tree().unwrap();
        let sig = Signature::now("Test User", "test@example.com").unwrap();
        let fork_b_id = repo
            .commit(None, &sig, &sig, "fork-b commit", &tree, &[&c1])
            .unwrap();
        let fork_b = repo.find_commit(fork_b_id).unwrap();
        repo.branch("fork-b", &fork_b, false).unwrap();

        // Current branch is 'base' at c1
        repo.branch("base", &c1, false).unwrap();

        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
    }
    repo.set_head("refs/heads/base").unwrap();
    fs::remove_file(dir.path().join("file.txt")).unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("checkout")
        .arg("up")
        .current_dir(dir.path())
        .env("TERM", "dumb")
        .assert()
        .success()
        .stdout(predicates::str::contains("auto-selecting first option"));

    let new_head = repo.head().unwrap().shorthand().unwrap().to_string();
    assert!(
        new_head == "fork-a" || new_head == "fork-b",
        "Expected fork-a or fork-b, but got: {}",
        new_head
    );
}

#[test]
fn test_checkout_top_fork() {
    let (dir, repo) = setup_repo();

    // Create two tips
    {
        let c1_id = repo.revparse_single("HEAD~2").unwrap().id();
        let c2_id = repo.revparse_single("HEAD~1").unwrap().id();
        let head_id = repo.head().unwrap().peel_to_commit().unwrap().id();
        let c1 = repo.find_commit(c1_id).unwrap();
        let c2 = repo.find_commit(c2_id).unwrap();
        let head = repo.find_commit(head_id).unwrap();

        // tip-a is head
        repo.branch("tip-a", &head, false).unwrap();

        // tip-b is a NEW commit from c1
        let tree = c2.tree().unwrap();
        let sig = Signature::now("Test User", "test@example.com").unwrap();
        let tip_b_id = repo
            .commit(None, &sig, &sig, "tip-b commit", &tree, &[&c1])
            .unwrap();
        let tip_b = repo.find_commit(tip_b_id).unwrap();
        repo.branch("tip-b", &tip_b, false).unwrap();

        // Ensure working directory is clean
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();

        // Current branch is 'base' at c1
        repo.branch("base", &c1, false).unwrap();
    }
    repo.set_head("refs/heads/base").unwrap();
    fs::remove_file(dir.path().join("file.txt")).unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("checkout")
        .arg("top")
        .current_dir(dir.path())
        .env("TERM", "dumb")
        .assert()
        .success()
        .stdout(predicates::str::contains("auto-selecting first option"));

    let new_head = repo.head().unwrap().shorthand().unwrap().to_string();
    assert!(
        new_head == "tip-a" || new_head == "tip-b",
        "Expected tip-a or tip-b, but got: {}",
        new_head
    );
}

#[test]
fn test_split_fork_selection() {
    let (dir, repo) = setup_repo();

    // Create two tips
    {
        let c1_id = repo.revparse_single("HEAD~2").unwrap().id();
        let c2_id = repo.revparse_single("HEAD~1").unwrap().id();
        let head_id = repo.head().unwrap().peel_to_commit().unwrap().id();
        let head = repo.find_commit(head_id).unwrap();
        let c2 = repo.find_commit(c2_id).unwrap();
        let c1 = repo.find_commit(c1_id).unwrap();

        // path-a is head
        repo.branch("path-a", &head, false).unwrap();

        // path-b is a NEW commit from c1
        let tree = c2.tree().unwrap();
        let sig = Signature::now("Test User", "test@example.com").unwrap();
        let path_b_id = repo
            .commit(None, &sig, &sig, "path-b commit", &tree, &[&c1])
            .unwrap();
        let path_b = repo.find_commit(path_b_id).unwrap();
        repo.branch("path-b", &path_b, false).unwrap();

        // Ensure we are at base (c1) to see both tips
        repo.set_head_detached(c1.id()).unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
    }

    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
exit 0
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
    cmd.arg("split")
        .current_dir(dir.path())
        .env("EDITOR", &editor_script)
        .env("TERM", "dumb")
        .assert()
        .success()
        .stdout(predicates::str::contains("auto-selecting first option"));
}

#[test]
fn test_checkout_all_works_without_main() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let signature = Signature::now("Test User", "test@example.com").unwrap();

    fs::write(dir.path().join("file.txt"), "initial").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("file.txt")).unwrap();
    let oid = index.write_tree().unwrap();
    let tree = repo.find_tree(oid).unwrap();
    repo.commit(
        Some("refs/heads/trunk"),
        &signature,
        &signature,
        "initial commit",
        &tree,
        &[],
    )
    .unwrap();

    repo.set_head("refs/heads/trunk").unwrap();
    fs::remove_file(dir.path().join("file.txt")).unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("checkout")
        .arg("--all")
        .current_dir(dir.path())
        .env("TERM", "dumb")
        .assert()
        .success()
        .stdout(predicates::str::contains("auto-selecting first option"));

    let new_head = repo.head().unwrap().shorthand().unwrap().to_string();
    assert_eq!(new_head, "trunk");
}

#[test]
fn test_checkout_all_detached_no_main() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let signature = Signature::now("Test User", "test@example.com").unwrap();

    fs::write(dir.path().join("file.txt"), "initial").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("file.txt")).unwrap();
    let oid = index.write_tree().unwrap();
    let tree = repo.find_tree(oid).unwrap();
    let commit_id = repo
        .commit(
            Some("refs/heads/trunk"),
            &signature,
            &signature,
            "initial commit",
            &tree,
            &[],
        )
        .unwrap();

    // Detach HEAD
    repo.set_head_detached(commit_id).unwrap();
    fs::remove_file(dir.path().join("file.txt")).unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("checkout")
        .arg("--all")
        .current_dir(dir.path())
        .env("TERM", "dumb")
        .assert()
        .success()
        .stdout(predicates::str::contains("auto-selecting first option"));

    let new_head = repo.head().unwrap().shorthand().unwrap().to_string();
    assert_eq!(new_head, "trunk");
}

#[test]
fn test_split_invalid_edit_validation() {
    let (dir, _repo) = setup_repo();

    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
# Put a branch at the very top of the file, before any commits
echo "branch invalid-move" > "$file.tmp"
cat "$file" >> "$file.tmp"
mv "$file.tmp" "$file"
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
    cmd.arg("split")
        .current_dir(dir.path())
        .env("EDITOR", &editor_script)
        .assert()
        .failure()
        .stderr(predicates::str::contains("must follow a commit line"));

    // Verify state file does NOT exist
    assert!(!dir.path().join(".git/gits_rebase_state.json").exists());
}
