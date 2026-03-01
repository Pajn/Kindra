#![allow(deprecated)]
use git2::{Repository, Signature};
use std::fs;
use std::io::{BufRead, BufReader};
use std::process::Stdio;
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
        )
        .unwrap()
    };

    let first_commit_id = parent_id;

    // Create a stack of 3 commits
    for i in 1..=3 {
        let tree_oid = {
            let mut index = repo.index().unwrap();
            fs::write(
                dir.path().join(format!("file{}.txt", i)),
                format!("content {}", i),
            )
            .unwrap();
            index
                .add_path(std::path::Path::new(&format!("file{}.txt", i)))
                .unwrap();
            index.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_oid).unwrap();
        let parent = repo.find_commit(parent_id).unwrap();
        parent_id = repo
            .commit(
                None,
                &signature,
                &signature,
                &format!("commit {}", i),
                &tree,
                &[&parent],
            )
            .unwrap();
    }

    // Detach HEAD before moving main
    repo.set_head_detached(parent_id).unwrap();

    {
        // Reset main to the first commit
        let first_commit = repo.find_commit(first_commit_id).unwrap();
        repo.branch("main", &first_commit, true).unwrap();
    }

    {
        // Clean up working directory to avoid checkout conflicts
        let head_commit = repo.find_commit(parent_id).unwrap();
        repo.checkout_tree(
            head_commit.as_object(),
            Some(git2::build::CheckoutBuilder::new().force()),
        )
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

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
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

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
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

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
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

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
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

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
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

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
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

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
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

    // Branch at c2 and HEAD
    {
        let c2 = repo.find_commit(c2_id).unwrap();
        let head = repo.find_commit(head_id).unwrap();
        repo.branch("fork-a", &head, false).unwrap();
        repo.branch("fork-b", &c2, false).unwrap();

        // Current branch is 'base' at c1
        let c1 = repo.find_commit(c1_id).unwrap();
        repo.branch("base", &c1, false).unwrap();

        // Force checkout to c1 to be clean
        repo.checkout_tree(
            c1.as_object(),
            Some(git2::build::CheckoutBuilder::new().force()),
        )
        .unwrap();
    }
    repo.set_head("refs/heads/base").unwrap();

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.arg("checkout")
        .arg("up")
        .current_dir(dir.path())
        .write_stdin("\n")
        .assert()
        .success();

    let new_head = repo.head().unwrap().shorthand().unwrap().to_string();
    assert!(new_head == "fork-a" || new_head == "fork-b");
}

#[test]
fn test_checkout_top_fork() {
    let (dir, repo) = setup_repo();

    // Create two tips
    {
        let c2_id = repo.revparse_single("HEAD~1").unwrap().id();
        let head_id = repo.head().unwrap().peel_to_commit().unwrap().id();
        let c2 = repo.find_commit(c2_id).unwrap();
        let head = repo.find_commit(head_id).unwrap();
        repo.branch("tip-a", &head, false).unwrap();
        repo.branch("tip-b", &c2, false).unwrap();

        // Ensure working directory is clean for checkout
        repo.checkout_tree(
            head.as_object(),
            Some(git2::build::CheckoutBuilder::new().force()),
        )
        .unwrap();
    }

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.arg("checkout")
        .arg("top")
        .current_dir(dir.path())
        .write_stdin("\n") // Select first option
        .assert()
        .success();

    let new_head = repo.head().unwrap().shorthand().unwrap().to_string();
    assert!(new_head == "tip-a" || new_head == "tip-b");
}

#[test]
fn test_split_fork_selection() {
    let (dir, repo) = setup_repo();

    // Create two tips
    {
        let c2_id = repo.revparse_single("HEAD~1").unwrap().id();
        let head_id = repo.head().unwrap().peel_to_commit().unwrap().id();
        let c2 = repo.find_commit(c2_id).unwrap();
        let head = repo.find_commit(head_id).unwrap();
        repo.branch("path-a", &head, false).unwrap();
        repo.branch("path-b", &c2, false).unwrap();
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

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.arg("split")
        .current_dir(dir.path())
        .env("EDITOR", &editor_script)
        .write_stdin("\n")
        .assert()
        .success();
}

#[test]
#[allow(clippy::zombie_processes)]
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

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.arg("checkout")
        .arg("--all")
        .current_dir(dir.path())
        .env("TERM", "dumb")
        .env("INQUIRE_SKIP_TTY_CHECK", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn gits");

    let stdout = child.stdout.take().expect("Failed to open stdout");
    let stderr = child.stderr.take().expect("Failed to open stderr");

    let (tx, rx) = std::sync::mpsc::channel();

    let tx_stdout = tx.clone();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            if let Ok(line) = line
                && line.contains("Select branch to checkout")
            {
                let _ = tx_stdout.send(true);
                return;
            }
        }
    });

    let tx_stderr = tx;
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            if let Ok(line) = line
                && line.contains("Select branch to checkout")
            {
                let _ = tx_stderr.send(true);
                return;
            }
        }
    });

    let found_menu = rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok();

    let _ = child.kill();
    assert!(found_menu, "Interactive menu did not appear");
}

#[test]
#[allow(clippy::zombie_processes)]
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

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.arg("checkout")
        .arg("--all")
        .current_dir(dir.path())
        .env("TERM", "dumb")
        .env("INQUIRE_SKIP_TTY_CHECK", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn gits");

    let stdout = child.stdout.take().expect("Failed to open stdout");
    let stderr = child.stderr.take().expect("Failed to open stderr");

    let (tx, rx) = std::sync::mpsc::channel();

    let tx_stdout = tx.clone();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            if let Ok(line) = line
                && line.contains("Select branch to checkout")
            {
                let _ = tx_stdout.send(true);
                return;
            }
        }
    });

    let tx_stderr = tx;
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            if let Ok(line) = line
                && line.contains("Select branch to checkout")
            {
                let _ = tx_stderr.send(true);
                return;
            }
        }
    });

    let found_menu = rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok();

    let _ = child.kill();
    assert!(
        found_menu,
        "Interactive menu did not appear in detached HEAD state"
    );
}
