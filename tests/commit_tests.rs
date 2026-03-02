use assert_cmd::Command;
use git2::{Repository, Signature};
use std::fs;
use tempfile::tempdir;

fn gits_cmd() -> Command {
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com");
    cmd
}

fn setup_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let signature = Signature::now("Test User", "test@example.com").unwrap();

    // 1. Initial commit on main
    let main_commit_id = {
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

    // 2. Branch 'feature' on top of main
    {
        let main_commit = repo.find_commit(main_commit_id).unwrap();
        let mut index = repo.index().unwrap();
        fs::write(dir.path().join("feature.txt"), "feature").unwrap();
        index.add_path(std::path::Path::new("feature.txt")).unwrap();
        let oid = index.write_tree().unwrap();
        let tree = repo.find_tree(oid).unwrap();
        repo.commit(
            Some("refs/heads/feature"),
            &signature,
            &signature,
            "feature commit",
            &tree,
            &[&main_commit],
        )
        .unwrap();
    }

    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    (dir, repo)
}

#[test]
fn test_commit_rebases_descendants() {
    let (dir, repo) = setup_repo();
    let signature = Signature::now("Test User", "test@example.com").unwrap();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // feature-a on main
    let a_id = {
        let mut index = repo.index().unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        index.add_path(std::path::Path::new("a.txt")).unwrap();
        let oid = index.write_tree().unwrap();
        let tree = repo.find_tree(oid).unwrap();
        repo.commit(
            Some("refs/heads/feature-a"),
            &signature,
            &signature,
            "commit a",
            &tree,
            &[&main_commit],
        )
        .unwrap()
    };
    let a_commit = repo.find_commit(a_id).unwrap();

    // feature-b on feature-a
    let _b_id = {
        let mut index = repo.index().unwrap();
        fs::write(dir.path().join("b.txt"), "b").unwrap();
        index.add_path(std::path::Path::new("b.txt")).unwrap();
        let oid = index.write_tree().unwrap();
        let tree = repo.find_tree(oid).unwrap();
        repo.commit(
            Some("refs/heads/feature-b"),
            &signature,
            &signature,
            "commit b",
            &tree,
            &[&a_commit],
        )
        .unwrap()
    };

    // Checkout feature-a
    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Stage changes for new commit on feature-a
    fs::write(dir.path().join("a2.txt"), "a2").unwrap();
    let mut git_add = std::process::Command::new("git");
    let out = git_add
        .arg("add")
        .arg("a2.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Run gits commit
    let mut cmd = gits_cmd();
    cmd.arg("commit")
        .arg("-m")
        .arg("new a")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    // Verify feature-a moved
    let new_a_id = repo
        .find_reference("refs/heads/feature-a")
        .unwrap()
        .target()
        .unwrap();
    assert_ne!(new_a_id, a_id);

    // Verify feature-b rebased
    let new_b_id = repo
        .find_reference("refs/heads/feature-b")
        .unwrap()
        .target()
        .unwrap();
    let new_b_commit = repo.find_commit(new_b_id).unwrap();
    assert_eq!(new_b_commit.parent_id(0).unwrap(), new_a_id);
    assert_eq!(new_b_commit.message().unwrap(), "commit b");

    // Verify back on feature-a
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "feature-a");
}

#[test]
fn test_commit_amend_rebases_descendants() {
    let (dir, repo) = setup_repo();
    let signature = Signature::now("Test User", "test@example.com").unwrap();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // feature-a on main
    let a_id = {
        let mut index = repo.index().unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        index.add_path(std::path::Path::new("a.txt")).unwrap();
        let oid = index.write_tree().unwrap();
        let tree = repo.find_tree(oid).unwrap();
        repo.commit(
            Some("refs/heads/feature-a"),
            &signature,
            &signature,
            "commit a",
            &tree,
            &[&main_commit],
        )
        .unwrap()
    };
    let a_commit = repo.find_commit(a_id).unwrap();

    // feature-b on feature-a
    let _b_id = {
        let mut index = repo.index().unwrap();
        fs::write(dir.path().join("b.txt"), "b").unwrap();
        index.add_path(std::path::Path::new("b.txt")).unwrap();
        let oid = index.write_tree().unwrap();
        let tree = repo.find_tree(oid).unwrap();
        repo.commit(
            Some("refs/heads/feature-b"),
            &signature,
            &signature,
            "commit b",
            &tree,
            &[&a_commit],
        )
        .unwrap()
    };

    // Checkout feature-a
    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Run gits commit --amend
    fs::write(dir.path().join("a.txt"), "amended a").unwrap();
    let out = std::process::Command::new("git")
        .arg("add")
        .arg("a.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut cmd = gits_cmd();
    cmd.arg("commit")
        .arg("--amend")
        .arg("--no-edit")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    // Verify feature-a moved
    let new_a_id = repo
        .find_reference("refs/heads/feature-a")
        .unwrap()
        .target()
        .unwrap();
    assert_ne!(new_a_id, a_id);

    // Verify feature-b rebased
    let new_b_id = repo
        .find_reference("refs/heads/feature-b")
        .unwrap()
        .target()
        .unwrap();
    let new_b_commit = repo.find_commit(new_b_id).unwrap();
    assert_eq!(new_b_commit.parent_id(0).unwrap(), new_a_id);
    assert_eq!(new_b_commit.message().unwrap(), "commit b");

    // Verify back on feature-a
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "feature-a");
}

#[test]
fn test_commit_no_changes() {
    let (dir, repo) = setup_repo();
    let signature = Signature::now("Test User", "test@example.com").unwrap();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // feature-a on main
    let a_id = {
        let mut index = repo.index().unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        index.add_path(std::path::Path::new("a.txt")).unwrap();
        let oid = index.write_tree().unwrap();
        let tree = repo.find_tree(oid).unwrap();
        repo.commit(
            Some("refs/heads/feature-a"),
            &signature,
            &signature,
            "commit a",
            &tree,
            &[&main_commit],
        )
        .unwrap()
    };

    // Checkout feature-a
    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Run gits commit without staging anything
    let mut cmd = gits_cmd();
    cmd.arg("commit")
        .arg("-m")
        .arg("nothing")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure();

    // Verify feature-a did NOT move
    let new_a_id = repo
        .find_reference("refs/heads/feature-a")
        .unwrap()
        .target()
        .unwrap();
    assert_eq!(new_a_id, a_id);
}

#[test]
fn test_commit_forked_stack() {
    let (dir, repo) = setup_repo();
    let signature = Signature::now("Test User", "test@example.com").unwrap();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // feature-a on main
    let a_id = {
        let mut index = repo.index().unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        index.add_path(std::path::Path::new("a.txt")).unwrap();
        let oid = index.write_tree().unwrap();
        let tree = repo.find_tree(oid).unwrap();
        repo.commit(
            Some("refs/heads/feature-a"),
            &signature,
            &signature,
            "commit a",
            &tree,
            &[&main_commit],
        )
        .unwrap()
    };
    let a_commit = repo.find_commit(a_id).unwrap();

    // feature-b on feature-a
    let _b_id = {
        let mut index = repo.index().unwrap();
        fs::write(dir.path().join("b.txt"), "b").unwrap();
        index.add_path(std::path::Path::new("b.txt")).unwrap();
        let oid = index.write_tree().unwrap();
        let tree = repo.find_tree(oid).unwrap();
        repo.commit(
            Some("refs/heads/feature-b"),
            &signature,
            &signature,
            "commit b",
            &tree,
            &[&a_commit],
        )
        .unwrap()
    };

    // feature-c on feature-a (fork)
    let _c_id = {
        let mut index = repo.index().unwrap();
        fs::write(dir.path().join("c.txt"), "c").unwrap();
        index.add_path(std::path::Path::new("c.txt")).unwrap();
        let oid = index.write_tree().unwrap();
        let tree = repo.find_tree(oid).unwrap();
        repo.commit(
            Some("refs/heads/feature-c"),
            &signature,
            &signature,
            "commit c",
            &tree,
            &[&a_commit],
        )
        .unwrap()
    };

    // Checkout feature-a
    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Run gits commit
    fs::write(dir.path().join("a2.txt"), "a2").unwrap();
    let mut git_add = std::process::Command::new("git");
    let out = git_add
        .arg("add")
        .arg("a2.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut cmd = gits_cmd();
    cmd.arg("commit")
        .arg("-m")
        .arg("new a")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    let new_a_id = repo
        .find_reference("refs/heads/feature-a")
        .unwrap()
        .target()
        .unwrap();

    // Verify feature-b rebased
    let new_b_id = repo
        .find_reference("refs/heads/feature-b")
        .unwrap()
        .target()
        .unwrap();
    let new_b_commit = repo.find_commit(new_b_id).unwrap();
    assert_eq!(new_b_commit.parent_id(0).unwrap(), new_a_id);

    // Verify feature-c rebased
    let new_c_id = repo
        .find_reference("refs/heads/feature-c")
        .unwrap()
        .target()
        .unwrap();
    let new_c_commit = repo.find_commit(new_c_id).unwrap();
    assert_eq!(new_c_commit.parent_id(0).unwrap(), new_a_id);

    // Verify back on feature-a
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "feature-a");
}

#[test]
fn test_commit_on_main() {
    let (dir, repo) = setup_repo();

    // Checkout main
    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Run gits commit
    fs::write(dir.path().join("main2.txt"), "main2").unwrap();
    let out = std::process::Command::new("git")
        .arg("add")
        .arg("main2.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut cmd = gits_cmd();
    cmd.arg("commit")
        .arg("-m")
        .arg("on main")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    // Verify main moved
    let head = repo.head().unwrap();
    assert_eq!(head.shorthand().unwrap(), "main");
    let commit = head.peel_to_commit().unwrap();
    assert_eq!(commit.message().unwrap(), "on main\n");
}

#[test]
fn test_commit_conflict_and_continue() {
    let (dir, repo) = setup_repo();
    let signature = Signature::now("Test User", "test@example.com").unwrap();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // feature-a on main
    let a_id = {
        let mut index = repo.index().unwrap();
        fs::write(dir.path().join("shared.txt"), "original").unwrap();
        index.add_path(std::path::Path::new("shared.txt")).unwrap();
        let oid = index.write_tree().unwrap();
        let tree = repo.find_tree(oid).unwrap();
        repo.commit(
            Some("refs/heads/feature-a"),
            &signature,
            &signature,
            "commit a",
            &tree,
            &[&main_commit],
        )
        .unwrap()
    };
    let a_commit = repo.find_commit(a_id).unwrap();

    // feature-b on feature-a (will conflict)
    let _b_id = {
        let mut index = repo.index().unwrap();
        fs::write(dir.path().join("shared.txt"), "feature b change").unwrap();
        index.add_path(std::path::Path::new("shared.txt")).unwrap();
        let oid = index.write_tree().unwrap();
        let tree = repo.find_tree(oid).unwrap();
        repo.commit(
            Some("refs/heads/feature-b"),
            &signature,
            &signature,
            "commit b",
            &tree,
            &[&a_commit],
        )
        .unwrap()
    };

    // Checkout feature-a
    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Commit a change on feature-a that conflicts with feature-b
    fs::write(dir.path().join("shared.txt"), "conflicting change").unwrap();
    let out = std::process::Command::new("git")
        .arg("add")
        .arg("shared.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut cmd = gits_cmd();
    cmd.arg("commit")
        .arg("-m")
        .arg("conflicting a")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Resolve conflicts"));

    // Verify rebase state exists
    assert!(dir.path().join(".git/gits_rebase_state.json").exists());

    // Resolve conflict
    fs::write(dir.path().join("shared.txt"), "resolved content").unwrap();
    let out = std::process::Command::new("git")
        .arg("add")
        .arg("shared.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Continue with gits (which will run git rebase --continue for us)
    let mut cmd_cont = gits_cmd();
    cmd_cont
        .arg("continue")
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    // Verify rebase state cleared
    assert!(!dir.path().join(".git/gits_rebase_state.json").exists());

    // Verify feature-b rebased
    let new_a_id = repo
        .find_reference("refs/heads/feature-a")
        .unwrap()
        .target()
        .unwrap();
    let new_b_id = repo
        .find_reference("refs/heads/feature-b")
        .unwrap()
        .target()
        .unwrap();
    assert_eq!(
        repo.find_commit(new_b_id).unwrap().parent_id(0).unwrap(),
        new_a_id
    );

    // Verify back on feature-a
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "feature-a");
}

#[test]
fn test_commit_abort() {
    let (dir, repo) = setup_repo();
    let signature = Signature::now("Test User", "test@example.com").unwrap();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // feature-a on main
    let a_id = {
        let mut index = repo.index().unwrap();
        fs::write(dir.path().join("shared.txt"), "original").unwrap();
        index.add_path(std::path::Path::new("shared.txt")).unwrap();
        let oid = index.write_tree().unwrap();
        let tree = repo.find_tree(oid).unwrap();
        repo.commit(
            Some("refs/heads/feature-a"),
            &signature,
            &signature,
            "commit a",
            &tree,
            &[&main_commit],
        )
        .unwrap()
    };
    let a_commit = repo.find_commit(a_id).unwrap();

    // feature-b on feature-a (will conflict)
    let _b_id = {
        let mut index = repo.index().unwrap();
        fs::write(dir.path().join("shared.txt"), "feature b change").unwrap();
        index.add_path(std::path::Path::new("shared.txt")).unwrap();
        let oid = index.write_tree().unwrap();
        let tree = repo.find_tree(oid).unwrap();
        repo.commit(
            Some("refs/heads/feature-b"),
            &signature,
            &signature,
            "commit b",
            &tree,
            &[&a_commit],
        )
        .unwrap()
    };

    // Checkout feature-a
    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Commit conflicting change
    fs::write(dir.path().join("shared.txt"), "conflict").unwrap();
    let out = std::process::Command::new("git")
        .arg("add")
        .arg("shared.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut cmd = gits_cmd();
    cmd.arg("commit")
        .arg("-m")
        .arg("conflict")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure();

    assert!(dir.path().join(".git/gits_rebase_state.json").exists());

    // Abort
    let mut cmd_abort = gits_cmd();
    cmd_abort
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();

    assert!(!dir.path().join(".git/gits_rebase_state.json").exists());
}

#[test]
fn test_commit_reentry_guard() {
    let (dir, _repo) = setup_repo();
    let state_path = dir.path().join(".git/gits_rebase_state.json");

    // Create the state file to simulate an ongoing operation
    fs::write(&state_path, "{}").unwrap();

    // Attempt to run gits commit
    let mut cmd = gits_cmd();
    cmd.arg("commit")
        .arg("-m")
        .arg("test")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "operation is already in progress",
        ));

    // Verify state file still exists
    assert!(state_path.exists());
}

#[test]
fn test_commit_on_main_rebases_descendant() {
    let (dir, repo) = setup_repo();

    // Verify initial state
    let main = repo.find_branch("main", git2::BranchType::Local).unwrap();
    let main_id = main.get().target().unwrap();
    let feature = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap();
    let feature_id = feature.get().target().unwrap();

    assert!(repo.graph_descendant_of(feature_id, main_id).unwrap());

    // Run gits commit on main
    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("main_new.txt"), "new content").unwrap();
    let out = std::process::Command::new("git")
        .arg("add")
        .arg("main_new.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut cmd = gits_cmd();
    cmd.arg("commit")
        .arg("-m")
        .arg("new main commit")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    // Verify main has a new commit
    let main_new = repo.find_branch("main", git2::BranchType::Local).unwrap();
    let main_new_id = main_new.get().target().unwrap();
    assert_ne!(main_new_id, main_id);

    // Verify feature has been rebased on top of new main
    let feature_new = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap();
    let feature_new_id = feature_new.get().target().unwrap();

    assert!(
        repo.graph_descendant_of(feature_new_id, main_new_id)
            .unwrap(),
        "Feature branch should be a descendant of the new main commit. Feature: {}, Main: {}",
        feature_new_id,
        main_new_id
    );
}

#[test]
fn test_commit_on_main_rebases_multi_level_stack() {
    let (dir, repo) = setup_repo();
    let signature = Signature::now("Test User", "test@example.com").unwrap();

    // 1. Setup main -> feature -> feature2
    let main_id = repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();

    let feature_commit_id = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let feature_commit = repo.find_commit(feature_commit_id).unwrap();

    let feature2_commit_id = {
        let mut index = repo.index().unwrap();
        fs::write(dir.path().join("feature2.txt"), "feature2").unwrap();
        index
            .add_path(std::path::Path::new("feature2.txt"))
            .unwrap();
        let oid = index.write_tree().unwrap();
        let tree = repo.find_tree(oid).unwrap();
        repo.commit(
            None,
            &signature,
            &signature,
            "feature2 commit",
            &tree,
            &[&feature_commit],
        )
        .unwrap()
    };
    let feature2_commit = repo.find_commit(feature2_commit_id).unwrap();
    repo.branch("feature2", &feature2_commit, false).unwrap();

    assert!(
        repo.graph_descendant_of(feature2_commit_id, feature_commit_id)
            .unwrap()
    );
    assert!(
        repo.graph_descendant_of(feature_commit_id, main_id)
            .unwrap()
    );

    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // 2. Run gits commit on main
    fs::write(dir.path().join("main_new_2.txt"), "new content 2").unwrap();
    let out = std::process::Command::new("git")
        .arg("add")
        .arg("main_new_2.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut cmd = gits_cmd();
    cmd.arg("commit")
        .arg("-m")
        .arg("new main commit 2")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    // 3. Verify entire stack is rebased
    let main_new = repo.find_branch("main", git2::BranchType::Local).unwrap();
    let main_new_id = main_new.get().target().unwrap();

    let feature_new = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap();
    let feature_new_id = feature_new.get().target().unwrap();

    let feature2_new = repo
        .find_branch("feature2", git2::BranchType::Local)
        .unwrap();
    let feature2_new_id = feature2_new.get().target().unwrap();

    assert!(
        repo.graph_descendant_of(feature_new_id, main_new_id)
            .unwrap()
    );
    assert!(
        repo.graph_descendant_of(feature2_new_id, feature_new_id)
            .unwrap()
    );
}

#[test]
fn test_commit_failure_is_propagated() {
    let (dir, _repo) = setup_repo();

    // Run gits commit with nothing staged - it should fail and show why
    let mut cmd = gits_cmd();
    cmd.arg("commit")
        .arg("-m")
        .arg("no changes")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure()
        .stdout(predicates::str::contains("nothing to commit"));
}
