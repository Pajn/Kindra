mod common;
use common::{kin_cmd, make_commit, repo_init, run_ok};
use git2::Repository;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn setup_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    // 1. Initial commit on main
    let main_commit_id = make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "initial",
        "initial commit",
        &[],
    );

    // 2. Branch 'feature' on top of main
    {
        let main_commit = repo.find_commit(main_commit_id).unwrap();
        make_commit(
            &repo,
            "refs/heads/feature",
            "feature.txt",
            "feature",
            "feature commit",
            &[&main_commit],
        );
    }

    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    (dir, repo)
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn assert_no_staged_changes(dir: &Path) {
    let staged = git_stdout(dir, &["diff", "--cached", "--name-only"]);
    assert!(
        staged.trim().is_empty(),
        "Expected no staged files, got:\n{}",
        staged
    );
}

fn assert_has_unstaged_file(dir: &Path, file: &str) {
    let unstaged = git_stdout(dir, &["status", "--porcelain"]);
    assert!(
        unstaged
            .lines()
            .any(|line| { line.trim_end().ends_with(file) && line.chars().nth(1) != Some(' ') }),
        "Expected '{}' to be unstaged, got:\n{}",
        file,
        unstaged
    );
}

#[test]
fn test_commit_rebases_descendants() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // feature-a on main
    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "commit a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();

    // feature-b on feature-a
    let _b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "commit b",
        &[&a_commit],
    );

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

    // Run kin commit
    let mut cmd = kin_cmd();
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
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // feature-a on main
    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "commit a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();

    // feature-b on feature-a
    let _b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "commit b",
        &[&a_commit],
    );

    // Checkout feature-a
    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Run kin commit --amend
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

    let mut cmd = kin_cmd();
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
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // feature-a on main
    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "commit a",
        &[&main_commit],
    );

    // Checkout feature-a
    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Run kin commit without staging anything
    let mut cmd = kin_cmd();
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
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // feature-a on main
    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "commit a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();

    // feature-b on feature-a
    let _b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "commit b",
        &[&a_commit],
    );

    // feature-c on feature-a (fork)
    let _c_id = make_commit(
        &repo,
        "refs/heads/feature-c",
        "c.txt",
        "c",
        "commit c",
        &[&a_commit],
    );

    // Checkout feature-a
    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Run kin commit
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

    let mut cmd = kin_cmd();
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

    // Run kin commit
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

    let mut cmd = kin_cmd();
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
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // feature-a on main
    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "shared.txt",
        "original",
        "commit a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();

    // feature-b on feature-a (will conflict)
    let _b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "shared.txt",
        "feature b change",
        "commit b",
        &[&a_commit],
    );

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

    let mut cmd = kin_cmd();
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

    // Continue with kin (which will run git rebase --continue for us)
    let mut cmd_cont = kin_cmd();
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
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // feature-a on main
    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "shared.txt",
        "original",
        "commit a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();

    // feature-b on feature-a (will conflict)
    let _b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "shared.txt",
        "feature b change",
        "commit b",
        &[&a_commit],
    );

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

    let mut cmd = kin_cmd();
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
    let mut cmd_abort = kin_cmd();
    cmd_abort
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();

    assert!(!dir.path().join(".git/gits_rebase_state.json").exists());
}

#[test]
fn test_abort_malformed_state() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "shared.txt",
        "original",
        "commit a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();
    make_commit(
        &repo,
        "refs/heads/feature-b",
        "shared.txt",
        "feature b change",
        "commit b",
        &[&a_commit],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("shared.txt"), "conflict").unwrap();
    run_ok("git", &["add", "shared.txt"], dir.path());

    let mut cmd = kin_cmd();
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

    let state_path = dir.path().join(".git/gits_rebase_state.json");
    assert!(state_path.exists());
    fs::write(&state_path, "{ malformed json").unwrap();

    let mut cmd_abort = kin_cmd();
    cmd_abort
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .failure();

    assert!(
        state_path.exists(),
        "Malformed state should be preserved when abort fails to parse it"
    );
}

#[test]
fn test_abort_uses_exact_stash_message_match() {
    let (dir, _repo) = setup_repo();

    run_ok("git", &["checkout", "-f", "main"], dir.path());

    fs::write(dir.path().join("file.txt"), "stash one").unwrap();
    run_ok(
        "git",
        &["stash", "push", "-m", "kin-commit-on-1-1"],
        dir.path(),
    );

    fs::write(dir.path().join("file.txt"), "stash ten").unwrap();
    run_ok(
        "git",
        &["stash", "push", "-m", "kin-commit-on-1-10"],
        dir.path(),
    );

    let state_path = dir.path().join(".git/gits_rebase_state.json");
    fs::write(
        &state_path,
        r#"{
  "operation": "Commit",
  "original_branch": "main",
  "target_branch": "main",
  "remaining_branches": [],
  "in_progress_branch": null,
  "parent_id_map": {},
  "parent_name_map": {},
  "stash_ref": "kin-commit-on-1-1",
  "unstage_on_restore": false
}"#,
    )
    .unwrap();

    let mut cmd_abort = kin_cmd();
    cmd_abort
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();

    let stash_list = git_stdout(dir.path(), &["stash", "list"]);
    let messages: Vec<&str> = stash_list
        .lines()
        .filter_map(|line| line.rsplit_once(": ").map(|(_, message)| message.trim()))
        .collect();
    assert!(
        messages.contains(&"kin-commit-on-1-10"),
        "Expected later stash to remain, got:\n{}",
        stash_list
    );
    assert!(
        !messages.contains(&"kin-commit-on-1-1"),
        "Expected exact stash to be removed, got:\n{}",
        stash_list
    );
}

#[test]
fn test_commit_reentry_guard() {
    let (dir, _repo) = setup_repo();
    let state_path = dir.path().join(".git/gits_rebase_state.json");

    // Create the state file to simulate an ongoing operation
    fs::write(&state_path, "{}").unwrap();

    // Attempt to run kin commit
    let mut cmd = kin_cmd();
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

    // Run kin commit on main
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

    let mut cmd = kin_cmd();
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

    let feature2_commit_id = make_commit(
        &repo,
        "refs/heads/feature2",
        "feature2.txt",
        "feature2",
        "feature2 commit",
        &[&feature_commit],
    );

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

    // 2. Run kin commit on main
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

    let mut cmd = kin_cmd();
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

    // Run kin commit with nothing staged - it should fail and show why
    let mut cmd = kin_cmd();
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

#[test]
fn test_commit_on_branch_in_stack_restores_original_and_unstages() {
    let (dir, repo) = setup_repo();

    let feature_id = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let feature_commit = repo.find_commit(feature_id).unwrap();
    let _feature_b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "feature-b.txt",
        "feature-b",
        "feature-b commit",
        &[&feature_commit],
    );

    repo.set_head("refs/heads/feature-b").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("shared.txt"), "commit on feature").unwrap();
    run_ok("git", &["add", "shared.txt"], dir.path());
    fs::write(dir.path().join("scratch.txt"), "keep me unstaged").unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--on")
        .arg("feature")
        .arg("-m")
        .arg("commit on feature")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "feature-b");

    let new_feature_id = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    assert_ne!(new_feature_id, feature_id);

    let new_feature_b_id = repo
        .find_branch("feature-b", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let new_feature_b_commit = repo.find_commit(new_feature_b_id).unwrap();
    assert_eq!(new_feature_b_commit.parent_id(0).unwrap(), new_feature_id);

    assert_eq!(
        fs::read_to_string(dir.path().join("scratch.txt")).unwrap(),
        "keep me unstaged"
    );
    assert_has_unstaged_file(dir.path(), "scratch.txt");
    assert_no_staged_changes(dir.path());
}

#[test]
fn test_commit_on_branch_in_stack_three_level_restores_original_and_unstages() {
    let (dir, repo) = setup_repo();

    let feature_id = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let feature_commit = repo.find_commit(feature_id).unwrap();
    let feature_b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "feature-b.txt",
        "feature-b",
        "feature-b commit",
        &[&feature_commit],
    );
    let feature_b_commit = repo.find_commit(feature_b_id).unwrap();
    let _feature_c_id = make_commit(
        &repo,
        "refs/heads/feature-c",
        "feature-c.txt",
        "feature-c",
        "feature-c commit",
        &[&feature_b_commit],
    );

    repo.set_head("refs/heads/feature-c").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("shared.txt"), "commit on feature").unwrap();
    run_ok("git", &["add", "shared.txt"], dir.path());
    fs::write(dir.path().join("scratch.txt"), "keep me unstaged").unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--on")
        .arg("feature")
        .arg("-m")
        .arg("commit on feature")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "feature-c");

    let new_feature_id = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    assert_ne!(new_feature_id, feature_id);

    let new_feature_b_id = repo
        .find_branch("feature-b", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let new_feature_b_commit = repo.find_commit(new_feature_b_id).unwrap();
    assert_eq!(new_feature_b_commit.parent_id(0).unwrap(), new_feature_id);

    let new_feature_c_id = repo
        .find_branch("feature-c", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let new_feature_c_commit = repo.find_commit(new_feature_c_id).unwrap();
    assert_eq!(new_feature_c_commit.parent_id(0).unwrap(), new_feature_b_id);

    assert_eq!(
        fs::read_to_string(dir.path().join("scratch.txt")).unwrap(),
        "keep me unstaged"
    );
    assert_has_unstaged_file(dir.path(), "scratch.txt");
    assert_no_staged_changes(dir.path());
}

#[test]
fn test_commit_on_without_argument_uses_interactive_selection() {
    let (dir, repo) = setup_repo();
    let main_before = repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();

    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("interactive.txt"), "interactive").unwrap();
    run_ok("git", &["add", "interactive.txt"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--on=")
        .arg("-m")
        .arg("interactive target")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success()
        .stdout(predicates::str::contains("auto-selecting first option"));

    let main_after = repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    assert_ne!(main_after, main_before);
}

#[test]
fn test_commit_on_requires_branch_when_followed_by_flag() {
    let (dir, _repo) = setup_repo();

    fs::write(dir.path().join("interactive-error.txt"), "interactive").unwrap();
    run_ok("git", &["add", "interactive-error.txt"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--on")
        .arg("-m")
        .arg("should fail")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "provide a branch name or use '--on=' for interactive selection",
        ));
}

#[test]
fn test_commit_on_other_stack_default_just_commits() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let root_id = make_commit(&repo, "refs/heads/main", "root.txt", "root", "root", &[]);
    let root = repo.find_commit(root_id).unwrap();

    let s1a_id = make_commit(&repo, "refs/heads/s1-a", "s1.txt", "s1-a", "s1-a", &[&root]);
    let s1a = repo.find_commit(s1a_id).unwrap();
    let s1b_id = make_commit(&repo, "refs/heads/s1-b", "s1b.txt", "s1-b", "s1-b", &[&s1a]);
    let s1b = repo.find_commit(s1b_id).unwrap();

    let s2a_id = make_commit(&repo, "refs/heads/s2-a", "s2.txt", "s2-a", "s2-a", &[&root]);
    let s2a = repo.find_commit(s2a_id).unwrap();
    let s2b_id = make_commit(&repo, "refs/heads/s2-b", "s2b.txt", "s2-b", "s2-b", &[&s2a]);

    repo.set_head("refs/heads/s1-b").unwrap();
    repo.checkout_tree(
        s1b.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    fs::write(dir.path().join("cross.txt"), "cross stack commit").unwrap();
    run_ok("git", &["add", "cross.txt"], dir.path());
    fs::write(dir.path().join("scratch.txt"), "scratch").unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--on")
        .arg("s2-a")
        .arg("-m")
        .arg("cross stack commit")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success()
        .stdout(predicates::str::contains("auto-denying"));

    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "s1-b");

    let s2a_new = repo
        .find_branch("s2-a", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    assert_ne!(s2a_new, s2a_id);

    let s2b_new = repo
        .find_branch("s2-b", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    assert_eq!(s2b_new, s2b_id, "s2-b should not be rebased by default");

    assert_eq!(
        fs::read_to_string(dir.path().join("scratch.txt")).unwrap(),
        "scratch"
    );
    assert_has_unstaged_file(dir.path(), "scratch.txt");
    assert_no_staged_changes(dir.path());
}

#[test]
fn test_commit_on_conflict_and_continue_restores_original_context() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "shared.txt",
        "base",
        "feature-a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();
    let _b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "shared.txt",
        "feature-b change",
        "feature-b",
        &[&a_commit],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(
        dir.path().join("shared.txt"),
        "feature-a conflicting change",
    )
    .unwrap();
    run_ok("git", &["add", "shared.txt"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--on")
        .arg("feature-a")
        .arg("-m")
        .arg("feature-a conflict")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Resolve conflicts"));

    assert!(dir.path().join(".git/gits_rebase_state.json").exists());

    fs::write(dir.path().join("shared.txt"), "resolved").unwrap();
    run_ok("git", &["add", "shared.txt"], dir.path());

    let mut cmd_cont = kin_cmd();
    cmd_cont
        .arg("continue")
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    assert!(!dir.path().join(".git/gits_rebase_state.json").exists());
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "feature-a");
}

#[test]
fn test_commit_on_conflict_and_abort_restores_original_context() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "shared.txt",
        "base",
        "feature-a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();
    let _b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "shared.txt",
        "feature-b change",
        "feature-b",
        &[&a_commit],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(
        dir.path().join("shared.txt"),
        "feature-a conflicting change",
    )
    .unwrap();
    run_ok("git", &["add", "shared.txt"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--on")
        .arg("feature-a")
        .arg("-m")
        .arg("feature-a conflict")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure();

    assert!(dir.path().join(".git/gits_rebase_state.json").exists());

    let mut cmd_abort = kin_cmd();
    cmd_abort
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();

    assert!(!dir.path().join(".git/gits_rebase_state.json").exists());
    assert!(!dir.path().join(".git/rebase-merge").exists());
    assert!(!dir.path().join(".git/rebase-apply").exists());
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "feature-a");
}

#[test]
fn test_rebase_loop_skips_resumed_and_subsequent_done_branches() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // Setup stack: main -> a -> b
    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "commit a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();
    let b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b-unique",
        "commit b",
        &[&a_commit],
    );

    // 1. Move main
    run_ok("git", &["checkout", "-f", "main"], dir.path());
    fs::write(dir.path().join("main_new.txt"), "new").unwrap();
    run_ok("git", &["add", "main_new.txt"], dir.path());
    run_ok("git", &["commit", "-m", "new main"], dir.path());
    let _new_main_id = repo.revparse_single("main").unwrap().id();

    // 2. Manually rebase a but NOT b.
    // Use git branch -f to move feature-a without moving b.
    run_ok("git", &["checkout", "-f", "main"], dir.path());
    fs::write(dir.path().join("a_new.txt"), "a_new").unwrap();
    run_ok("git", &["add", "a_new.txt"], dir.path());
    run_ok("git", &["commit", "-m", "new a"], dir.path());
    let new_a_id = repo.revparse_single("HEAD").unwrap().id();
    run_ok(
        "git",
        &["branch", "-f", "feature-a", &new_a_id.to_string()],
        dir.path(),
    );

    // Now feature-a is at new_a_id.
    // feature-b is still at b_id (which builds on OLD a_id).
    // So feature-b is NOT a descendant of new_a_id.
    assert!(!repo.graph_descendant_of(b_id, new_a_id).unwrap());

    // 3. Setup state: resuming a (which is done), b is next (not done)
    let state_path = dir.path().join(".git/gits_rebase_state.json");
    fs::write(
        &state_path,
        format!(
            r#"{{
  "operation": "Move",
  "original_branch": "feature-a",
  "target_branch": "main",
  "remaining_branches": ["feature-a", "feature-b"],
  "in_progress_branch": "feature-a",
  "parent_id_map": {{
    "feature-a": "{main_id}",
    "feature-b": "{a_id}"
  }},
  "parent_name_map": {{
    "feature-b": "feature-a"
  }},
  "caller_branch": null,
  "stash_ref": null,
  "unstage_on_restore": false
}}"#,
            main_id = main_id,
            a_id = a_id
        ),
    )
    .unwrap();

    // 4. Run continue. a should be skipped, b should be rebased.
    // Note: feature-a is "done" because its tip is new_a_id, and its parent in state is main_id (oops, should be new_main_id).
    // Wait, if target_branch is "main", and main is now new_main_id.
    // Is new_a_id a descendant of new_main_id? YES. So a is done.
    // feature-b is still at b_id (descendant of OLD a_id).
    // Its new_base is "feature-a" (new_a_id).
    // Is b_id a descendant of new_a_id? NO.
    // So b should be rebased.
    let mut cmd = kin_cmd();
    cmd.arg("continue")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "Branch feature-a already rebased.",
        ))
        .stdout(predicates::str::contains("Rebasing feature-b..."));

    assert!(!state_path.exists());
    let new_b_id = repo.revparse_single("feature-b").unwrap().id();
    assert!(repo.graph_descendant_of(new_b_id, new_a_id).unwrap());
}

#[test]
fn test_commit_on_checkout_conflict_restores_original_context() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "shared.txt",
        "feature-a base",
        "feature-a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();
    let _b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "shared.txt",
        "feature-b base",
        "feature-b",
        &[&a_commit],
    );

    repo.set_head("refs/heads/feature-b").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(
        dir.path().join("shared.txt"),
        "local staged change that blocks checkout",
    )
    .unwrap();
    run_ok("git", &["add", "shared.txt"], dir.path());
    fs::write(dir.path().join("scratch.txt"), "scratch").unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--on")
        .arg("feature-a")
        .arg("-m")
        .arg("should fail before commit")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "git checkout failed for branch 'feature-a'",
        ));

    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "feature-b");
    assert!(dir.path().join(".git/gits_rebase_state.json").exists());

    // Abort should clean up the state and restore context
    let mut abort_cmd = kin_cmd();
    abort_cmd
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();

    assert!(!dir.path().join(".git/gits_rebase_state.json").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("scratch.txt")).unwrap(),
        "scratch"
    );
    assert_has_unstaged_file(dir.path(), "scratch.txt");
    assert_no_staged_changes(dir.path());
}
