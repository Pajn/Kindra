use assert_cmd::Command;
use git2::{Repository, Signature};
use std::fs;
use tempfile::tempdir;

fn setup_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let signature = Signature::now("Test User", "test@example.com").unwrap();

    let mut parent_id = {
        let mut index = repo.index().unwrap();
        fs::write(dir.path().join("file.txt"), "initial").unwrap();
        index.add_path(std::path::Path::new("file.txt")).unwrap();
        let oid = index.write_tree().unwrap();
        let tree = repo.find_tree(oid).unwrap();
        repo.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "initial commit",
            &tree,
            &[],
        ).unwrap()
    };

    let first_commit_id = parent_id;

    // Create a stack of 3 commits
    for i in 1..=3 {
        let tree_oid = {
            let mut index = repo.index().unwrap();
            fs::write(dir.path().join("file.txt"), format!("content {}", i)).unwrap();
            index.add_path(std::path::Path::new("file.txt")).unwrap();
            index.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_oid).unwrap();
        let parent = repo.find_commit(parent_id).unwrap();
        parent_id = repo.commit(
            None,
            &signature,
            &signature,
            &format!("commit {}", i),
            &tree,
            &[&parent],
        ).unwrap();
    }

    // Detach HEAD before moving main
    repo.set_head_detached(parent_id).unwrap();

    {
        // Reset main to the first commit
        let first_commit = repo.find_commit(first_commit_id).unwrap();
        repo.branch("main", &first_commit, true).unwrap();
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
    fs::write(&editor_script, r#"#!/bin/sh
file=$1
sed -i '/branch feature-x/d' "$file"
sed -i '/commit 2/a branch feature-x' "$file"
"#).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&editor_script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&editor_script, perms).unwrap();
    }

    let mut cmd = Command::cargo_bin("gits").unwrap();
    cmd.arg("split")
        .current_dir(dir.path())
        .env("EDITOR", &editor_script)
        .assert()
        .success();

    // Verify branch moved
    let branch = repo.find_branch("feature-x", git2::BranchType::Local).unwrap();
    let target = branch.get().target().unwrap();
    let commit = repo.find_commit(target).unwrap();
    assert_eq!(commit.summary().unwrap(), "commit 2");
}

#[test]
fn test_split_create_delete_branch() {
    let (dir, repo) = setup_repo();
    
    let editor_script = dir.path().join("editor.sh");
    fs::write(&editor_script, r#"#!/bin/sh
file=$1
sed -i '/commit 1/a branch new-feat' "$file"
sed -i '/commit 3/a branch another-feat' "$file"
"#).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&editor_script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&editor_script, perms).unwrap();
    }

    let mut cmd = Command::cargo_bin("gits").unwrap();
    cmd.arg("split")
        .current_dir(dir.path())
        .env("EDITOR", &editor_script)
        .assert()
        .success();

    assert!(repo.find_branch("new-feat", git2::BranchType::Local).is_ok());
    assert!(repo.find_branch("another-feat", git2::BranchType::Local).is_ok());

    // Now delete 'new-feat'
    fs::write(&editor_script, r#"#!/bin/sh
file=$1
sed -i '/branch new-feat/d' "$file"
"#).unwrap();

    let mut cmd = Command::cargo_bin("gits").unwrap();
    cmd.arg("split")
        .current_dir(dir.path())
        .env("EDITOR", &editor_script)
        .assert()
        .success();

    assert!(repo.find_branch("new-feat", git2::BranchType::Local).is_err());
    assert!(repo.find_branch("another-feat", git2::BranchType::Local).is_ok());
}

#[test]
fn test_split_error_on_commit_mod() {
    let (dir, _repo) = setup_repo();
    
    let editor_script = dir.path().join("editor.sh");
    fs::write(&editor_script, r#"#!/bin/sh
file=$1
sed -i 's/^[0-9a-f]\{7\}/deadbee/' "$file"
"#).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&editor_script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&editor_script, perms).unwrap();
    }

    let mut cmd = Command::cargo_bin("gits").unwrap();
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
    fs::write(&editor_script, r#"#!/bin/sh
file=$1
sed -i '/branch current/d' "$file"
"#).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&editor_script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&editor_script, perms).unwrap();
    }

    let mut cmd = Command::cargo_bin("gits").unwrap();
    cmd.arg("split")
        .current_dir(dir.path())
        .env("EDITOR", &editor_script)
        .assert()
        .success();

    assert!(repo.head_detached().unwrap());
    assert!(repo.find_branch("current", git2::BranchType::Local).is_err());
}
