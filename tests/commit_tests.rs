mod common;
use common::{kin_cmd, make_commit, repo_init, run_ok};
use git2::Repository;
use kindra::rebase_utils::{Operation, RebaseState, save_state};
use std::collections::HashMap;
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

fn write_commit_rebase_state_fixture(repo: &Repository, stash_ref: &str) {
    let main_tip = repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap()
        .to_string();
    let state = RebaseState {
        operation: Operation::Commit,
        original_branch: "main".to_string(),
        target_branch: "main".to_string(),
        caller_branch: None,
        remaining_branches: vec![],
        in_progress_branch: None,
        parent_id_map: HashMap::new(),
        parent_name_map: HashMap::new(),
        new_base_map: HashMap::new(),
        original_commit_count_map: HashMap::new(),
        original_tip_map: HashMap::new(),
        owned_tip_map: HashMap::from([("main".to_string(), main_tip)]),
        stash_ref: Some(stash_ref.to_string()),
        stash_apply_index: false,
        preserve_content_on_abort: false,
        suppress_editor: false,
        unstage_on_restore: false,
        autostash: false,
        cleanup_merged_branches: Vec::new(),
        cleanup_checkout_fallback: None,
    };

    save_state(repo, &state).unwrap();
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
        .env("GIT_EDITOR", "true")
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
        .env("GIT_EDITOR", "true")
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
fn test_commit_short_i_is_forwarded_to_git() {
    let (dir, repo) = setup_repo();
    let feature_before = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();

    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("feature.txt"), "updated feature").unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("-i")
        .arg("feature.txt")
        .arg("-m")
        .arg("include tracked change")
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    let feature_after = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    assert_ne!(feature_after, feature_before);
    let feature_commit = repo.find_commit(feature_after).unwrap();
    assert_eq!(feature_commit.summary().unwrap(), "include tracked change");
    assert_eq!(feature_commit.parent_id(0).unwrap(), feature_before);
}

#[test]
fn test_commit_global_flag_after_subcommand_not_forwarded_to_git() {
    // Regression: `--yes` placed after `commit` is swallowed by clap's
    // trailing_var_arg. It must be stripped before invoking `git commit`
    // (which rejects `--yes`), not forwarded — otherwise the command fails.
    let (dir, repo) = setup_repo();
    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    let feature_before = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();

    fs::write(dir.path().join("feature.txt"), "amended feature").unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--amend")
        .arg("-a")
        .arg("--no-edit")
        .arg("--yes")
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    // The amend actually ran: the tip was rewritten and `--no-edit` kept the
    // original message (so `--no-edit` reached git, but `--yes` did not).
    let feature_after = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    assert_ne!(feature_after, feature_before);
    let feature_commit = repo.find_commit(feature_after).unwrap();
    assert_eq!(feature_commit.summary().unwrap(), "feature commit");
}

#[test]
fn test_commit_interactive_with_forwarded_short_i_uses_git_include() {
    let (dir, repo) = setup_repo();
    let feature_before = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();

    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(
        dir.path().join("feature.txt"),
        "updated feature via interactive -i",
    )
    .unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--interactive")
        .arg("-i")
        .arg("feature.txt")
        .arg("-m")
        .arg("interactive include tracked change")
        .current_dir(dir.path())
        .env("KIN_TEST_SELECTION", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    let feature_after = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    assert_ne!(feature_after, feature_before);
    let feature_commit = repo.find_commit(feature_after).unwrap();
    assert_eq!(
        feature_commit.summary().unwrap(),
        "interactive include tracked change"
    );
}

#[test]
fn test_commit_interactive_without_terminal_errors() {
    // The amend picker (`--interactive`) needs a real answer: with no terminal
    // and no scripted selection it must fail loudly rather than guess a commit.
    let (dir, repo) = setup_repo();
    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    kin_cmd()
        .arg("commit")
        .arg("--interactive")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Cannot pick a commit to amend without a terminal.",
        ));
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
        .env("GIT_EDITOR", "true")
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
        .env("GIT_EDITOR", "true")
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
        .env("GIT_EDITOR", "true")
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
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Resolve conflicts"));

    // Verify rebase state exists
    assert!(dir.path().join(".git/kindra_rebase_state.json").exists());

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
    assert!(!dir.path().join(".git/kindra_rebase_state.json").exists());

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
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure();

    assert!(dir.path().join(".git/kindra_rebase_state.json").exists());

    // Abort
    let mut cmd_abort = kin_cmd();
    cmd_abort
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();

    assert!(!dir.path().join(".git/kindra_rebase_state.json").exists());
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
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure();

    let state_path = dir.path().join(".git/kindra_rebase_state.json");
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
fn test_status_malformed_state_reports_error() {
    let dir = tempdir().unwrap();
    repo_init(dir.path());
    let state_path = dir.path().join(".git/kindra_rebase_state.json");
    fs::write(&state_path, "{ malformed json").unwrap();
    let state_before = fs::read_to_string(&state_path).unwrap();

    kin_cmd()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .failure();

    let state_after = fs::read_to_string(&state_path).unwrap();
    assert_eq!(state_after, state_before);
    assert!(
        state_path.exists(),
        "Malformed state should be preserved when status fails to parse it"
    );
}

#[test]
fn test_continue_malformed_state_does_not_advance_native_rebase() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let base_id = make_commit(&repo, "refs/heads/main", "file.txt", "base", "base", &[]);
    let base = repo.find_commit(base_id).unwrap();

    let feature_id = make_commit(
        &repo,
        "refs/heads/feature",
        "file.txt",
        "feature",
        "feature",
        &[&base],
    );
    let feature = repo.find_commit(feature_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "main",
        "main",
        &[&base],
    );

    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_tree(
        feature.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    let rebase = std::process::Command::new("git")
        .arg("rebase")
        .arg("main")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        !rebase.status.success(),
        "native rebase should stop for a conflict"
    );

    fs::write(dir.path().join("file.txt"), "resolved").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());

    let state_path = dir.path().join(".git/kindra_rebase_state.json");
    fs::write(&state_path, "{ malformed json").unwrap();

    kin_cmd()
        .arg("continue")
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .assert()
        .failure();

    assert!(
        dir.path().join(".git/rebase-merge").exists()
            || dir.path().join(".git/rebase-apply").exists(),
        "native rebase should remain in progress"
    );
    assert!(state_path.exists());
}

#[test]
fn test_continue_mismatched_parseable_state_does_not_advance_native_rebase() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let base_id = make_commit(&repo, "refs/heads/main", "file.txt", "base", "base", &[]);
    let base = repo.find_commit(base_id).unwrap();

    let feature_id = make_commit(
        &repo,
        "refs/heads/feature",
        "file.txt",
        "feature",
        "feature",
        &[&base],
    );
    let feature = repo.find_commit(feature_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "main",
        "main",
        &[&base],
    );

    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_tree(
        feature.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    let rebase = std::process::Command::new("git")
        .arg("rebase")
        .arg("main")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        !rebase.status.success(),
        "native rebase should stop for a conflict"
    );

    fs::write(dir.path().join("file.txt"), "resolved").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());

    let state_path = dir.path().join(".git/kindra_rebase_state.json");
    fs::write(
        &state_path,
        format!(
            r#"{{
  "operation": "Move",
  "original_branch": "other-feature",
  "target_branch": "main",
  "remaining_branches": ["other-feature"],
  "in_progress_branch": "other-feature",
  "parent_id_map": {{"other-feature": "{}"}},
  "parent_name_map": {{}},
  "new_base_map": {{}},
  "original_commit_count_map": {{}},
  "original_tip_map": {{}},
  "owned_tip_map": {{}},
  "stash_ref": null,
  "unstage_on_restore": false,
  "autostash": false,
  "cleanup_merged_branches": [],
  "cleanup_checkout_fallback": null
}}"#,
            base_id
        ),
    )
    .unwrap();

    kin_cmd()
        .arg("continue")
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Active git rebase does not match saved Kindra rebase state",
        ));

    assert!(
        dir.path().join(".git/rebase-merge").exists()
            || dir.path().join(".git/rebase-apply").exists(),
        "native rebase should remain in progress"
    );
    assert!(
        state_path.exists(),
        "mismatched Kindra state should remain without advancing the native rebase"
    );
}

#[test]
fn test_abort_uses_exact_stash_message_match() {
    let (dir, repo) = setup_repo();

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

    write_commit_rebase_state_fixture(&repo, "kin-commit-on-1-1");

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
fn test_abort_preserves_stash_when_owned_tip_map_mismatches() {
    let (dir, _repo) = setup_repo();

    run_ok("git", &["checkout", "-f", "main"], dir.path());

    fs::write(dir.path().join("file.txt"), "stash one").unwrap();
    run_ok(
        "git",
        &["stash", "push", "-m", "kin-commit-on-1-1"],
        dir.path(),
    );

    let state_path = dir.path().join(".git/kindra_rebase_state.json");
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
  "owned_tip_map": {
    "main": "0000000000000000000000000000000000000000"
  },
  "stash_ref": "kin-commit-on-1-1",
  "unstage_on_restore": false
}"#,
    )
    .unwrap();

    kin_cmd()
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("kin-commit-on-1-1"));

    let stash_list = git_stdout(dir.path(), &["stash", "list"]);
    assert!(
        stash_list.contains("kin-commit-on-1-1"),
        "Expected stash to remain when owned_tip_map does not match, got:\n{}",
        stash_list
    );
    assert!(
        !state_path.exists(),
        "State file should be cleared after abort"
    );
}

#[test]
fn test_abort_preserves_stash_for_legacy_state_without_owned_tip_map() {
    let (dir, _repo) = setup_repo();

    run_ok("git", &["checkout", "-f", "main"], dir.path());

    fs::write(dir.path().join("file.txt"), "stash one").unwrap();
    run_ok(
        "git",
        &["stash", "push", "-m", "kin-commit-on-1-1"],
        dir.path(),
    );

    let state_path = dir.path().join(".git/kindra_rebase_state.json");
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

    kin_cmd()
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("kin-commit-on-1-1"));

    let stash_list = git_stdout(dir.path(), &["stash", "list"]);
    assert!(
        stash_list.contains("kin-commit-on-1-1"),
        "Expected stash to remain for legacy state without owned_tip_map, got:\n{}",
        stash_list
    );
    assert!(
        !state_path.exists(),
        "State file should be cleared after abort"
    );
}

#[test]
fn test_commit_reentry_guard() {
    let (dir, _repo) = setup_repo();
    let state_path = dir.path().join(".git/kindra_rebase_state.json");

    // Create the state file to simulate an ongoing operation
    fs::write(&state_path, "{}").unwrap();

    // Attempt to run kin commit
    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("-m")
        .arg("test")
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
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
        .env("GIT_EDITOR", "true")
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
        .env("GIT_EDITOR", "true")
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
        .env("GIT_EDITOR", "true")
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
        .env("GIT_EDITOR", "true")
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
        .env("GIT_EDITOR", "true")
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
        .env("GIT_EDITOR", "true")
        .env("KIN_TEST_SELECTIONS", "0")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "test override: auto-selecting option",
        ));

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
        .env("GIT_EDITOR", "true")
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
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success()
        .stdout(predicates::str::contains("non-interactive: declining"));

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
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Resolve conflicts"));

    assert!(dir.path().join(".git/kindra_rebase_state.json").exists());

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

    assert!(!dir.path().join(".git/kindra_rebase_state.json").exists());
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
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure();

    assert!(dir.path().join(".git/kindra_rebase_state.json").exists());

    let mut cmd_abort = kin_cmd();
    cmd_abort
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();

    assert!(!dir.path().join(".git/kindra_rebase_state.json").exists());
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
    let state_path = dir.path().join(".git/kindra_rebase_state.json");
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
        .env("GIT_EDITOR", "true")
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
        .env("GIT_EDITOR", "true")
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
    assert!(dir.path().join(".git/kindra_rebase_state.json").exists());

    // Abort should clean up the state and restore context
    let mut abort_cmd = kin_cmd();
    abort_cmd
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();

    assert!(!dir.path().join(".git/kindra_rebase_state.json").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("scratch.txt")).unwrap(),
        "scratch"
    );
    assert_has_unstaged_file(dir.path(), "scratch.txt");
    assert_no_staged_changes(dir.path());
}

#[test]
fn test_commit_on_branch_amend() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

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
        "b",
        "commit b",
        &[&a_commit],
    );

    repo.set_head("refs/heads/feature-b").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("a.txt"), "amended a").unwrap();
    run_ok("git", &["add", "a.txt"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--on=feature-a")
        .arg("--amend")
        .arg("--no-edit")
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
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
    assert_ne!(new_a_id, a_id);
    let new_b_id = repo
        .find_reference("refs/heads/feature-b")
        .unwrap()
        .target()
        .unwrap();
    assert_ne!(new_b_id, b_id);
    let new_b_commit = repo.find_commit(new_b_id).unwrap();
    assert_eq!(new_b_commit.parent_id(0).unwrap(), new_a_id);
}

#[test]
fn test_commit_interactive_amend_tip() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let a1_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a1.txt",
        "a1",
        "commit a1",
        &[&main_commit],
    );
    let a1_commit = repo.find_commit(a1_id).unwrap();
    let a2_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a2.txt",
        "a2",
        "commit a2",
        &[&a1_commit],
    );
    let a2_commit = repo.find_commit(a2_id).unwrap();
    let a3_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a3.txt",
        "a3",
        "commit a3",
        &[&a2_commit],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("a3.txt"), "amended a3").unwrap();
    run_ok("git", &["add", "a3.txt"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--interactive")
        .current_dir(dir.path())
        .env("KIN_TEST_SELECTION", "0")
        .env("GIT_EDITOR", "true")
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
    assert_ne!(new_a_id, a3_id);
    let new_a_commit = repo.find_commit(new_a_id).unwrap();
    assert_eq!(new_a_commit.message().unwrap(), "commit a3\n");
    assert_eq!(new_a_commit.parent_id(0).unwrap(), a2_id);
}

#[test]
fn test_commit_interactive_pick_current_tip_folds_without_reword() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let a1_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a1.txt",
        "a1",
        "commit a1",
        &[&main_commit],
    );
    let a1_commit = repo.find_commit(a1_id).unwrap();
    let a2_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a2.txt",
        "a2",
        "commit a2",
        &[&a1_commit],
    );
    let a2_commit = repo.find_commit(a2_id).unwrap();
    let a3_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a3.txt",
        "a3",
        "commit a3",
        &[&a2_commit],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Stage a change; picking the current tip folds it in without rewording.
    // `false` as the editor would fail if a reword editor were (wrongly) opened.
    fs::write(dir.path().join("a3.txt"), "a3-fixed").unwrap();
    run_ok("git", &["add", "a3.txt"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--interactive")
        .current_dir(dir.path())
        .env("KIN_TEST_SELECTION", "0") // a3 = current tip
        .env("GIT_EDITOR", "false")
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
    assert_ne!(new_a_id, a3_id);
    let new_a_commit = repo.find_commit(new_a_id).unwrap();
    // Folded, message unchanged (no reword).
    assert_eq!(new_a_commit.summary().unwrap(), "commit a3");
    assert_eq!(new_a_commit.parent_id(0).unwrap(), a2_id);
    assert_eq!(blob_text(&repo, new_a_id, "a3.txt"), "a3-fixed");
}

#[test]
fn test_commit_interactive_amend_tip_with_pathspec_separator() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let a1_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a1.txt",
        "a1",
        "commit a1",
        &[&main_commit],
    );
    let a1_commit = repo.find_commit(a1_id).unwrap();
    let a2_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a2.txt",
        "a2",
        "commit a2",
        &[&a1_commit],
    );
    let a2_commit = repo.find_commit(a2_id).unwrap();
    let a3_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a3.txt",
        "a3",
        "commit a3",
        &[&a2_commit],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("a3.txt"), "amended a3").unwrap();
    run_ok("git", &["add", "a3.txt"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--interactive")
        .arg("--")
        .arg("a3.txt")
        .current_dir(dir.path())
        .env("KIN_TEST_SELECTION", "0")
        .env("GIT_EDITOR", "true")
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
    assert_ne!(new_a_id, a3_id);
    let new_a_commit = repo.find_commit(new_a_id).unwrap();
    assert_eq!(new_a_commit.message().unwrap(), "commit a3\n");
    assert_eq!(new_a_commit.parent_id(0).unwrap(), a2_id);
}

#[test]
fn test_commit_interactive_fixup_no_autostash_unwinds_pre_start_rebase_failure() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let a1_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a1.txt",
        "a1",
        "commit a1",
        &[&main_commit],
    );
    let a1_commit = repo.find_commit(a1_id).unwrap();
    let a2_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a2.txt",
        "a2",
        "commit a2",
        &[&a1_commit],
    );
    let a2_commit = repo.find_commit(a2_id).unwrap();
    let _a3_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a3.txt",
        "a3",
        "commit a3",
        &[&a2_commit],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("a2.txt"), "fixed a2").unwrap();
    run_ok("git", &["add", "a2.txt"], dir.path());
    fs::write(dir.path().join("file.txt"), "dirty tracked change").unwrap();
    assert_has_unstaged_file(dir.path(), "file.txt");

    let pre_rebase_hook = dir.path().join(".git/hooks/pre-rebase");
    fs::write(&pre_rebase_hook, "#!/bin/sh\nexit 1\n").unwrap();
    run_ok("chmod", &["+x", ".git/hooks/pre-rebase"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--interactive")
        .arg("--no-autostash")
        .arg("-m")
        .arg("fix a2")
        .current_dir(dir.path())
        .env("KIN_TEST_SELECTION", "1")
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure()
        .stderr(predicates::str::contains("rebase --autosquash failed"));

    let state_path = dir.path().join(".git/kindra_rebase_state.json");
    let state_content = fs::read_to_string(&state_path).unwrap();
    assert!(
        state_content.contains("\"in_progress_branch\": null"),
        "Pre-start autosquash failure should not persist an in-progress branch, got: {state_content}"
    );
    assert!(
        state_content.contains("\"stash_ref\": null"),
        "Pre-start autosquash failure should unwind the temporary stash, got: {state_content}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("file.txt")).unwrap(),
        "dirty tracked change"
    );
    assert_has_unstaged_file(dir.path(), "file.txt");
}

#[test]
fn test_commit_interactive_fixup_intermediate() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let a1_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a1.txt",
        "a1",
        "commit a1",
        &[&main_commit],
    );
    let a1_commit = repo.find_commit(a1_id).unwrap();
    let a2_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a2.txt",
        "a2",
        "commit a2",
        &[&a1_commit],
    );
    let a2_commit = repo.find_commit(a2_id).unwrap();
    let _a3_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a3.txt",
        "a3",
        "commit a3",
        &[&a2_commit],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("a2.txt"), "fixed a2").unwrap();
    run_ok("git", &["add", "a2.txt"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--interactive")
        .current_dir(dir.path())
        .env("KIN_TEST_SELECTION", "1") // select a2 (index 1 if newest is 0)
        .env("GIT_EDITOR", "true")
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

    // Check if the intermediate was fixed up by verifying the messages of the 3 commits
    let tip_commit = repo.find_commit(new_a_id).unwrap();
    assert_eq!(tip_commit.summary().unwrap(), "commit a3");

    let a2_new_commit = tip_commit.parent(0).unwrap();
    assert_eq!(a2_new_commit.summary().unwrap(), "commit a2"); // Autosquash squashes fixup into a2

    let a1_new_commit = a2_new_commit.parent(0).unwrap();
    assert_eq!(a1_new_commit.summary().unwrap(), "commit a1");
    assert_eq!(a1_new_commit.id(), a1_id); // a1 should be unchanged
}

#[test]
fn test_commit_interactive_fixup_commit_failure_does_not_persist_state() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let a1_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a1.txt",
        "a1",
        "commit a1",
        &[&main_commit],
    );
    let a1_commit = repo.find_commit(a1_id).unwrap();
    let a2_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a2.txt",
        "a2",
        "commit a2",
        &[&a1_commit],
    );
    let a2_commit = repo.find_commit(a2_id).unwrap();
    let _a3_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a3.txt",
        "a3",
        "commit a3",
        &[&a2_commit],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("a2.txt"), "fixed a2").unwrap();
    run_ok("git", &["add", "a2.txt"], dir.path());

    let hook_path = dir.path().join(".git/hooks/pre-commit");
    fs::write(&hook_path, "#!/bin/sh\nexit 1\n").unwrap();
    run_ok("chmod", &["+x", ".git/hooks/pre-commit"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--interactive")
        .current_dir(dir.path())
        .env("KIN_TEST_SELECTION", "1")
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure()
        .stderr(predicates::str::contains("git commit failed"));

    assert!(!dir.path().join(".git/kindra_rebase_state.json").exists());
}

#[test]
fn test_commit_interactive_fixup_conflict_and_continue() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // Create a branch with commits a1, a2, a3
    let a1_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "shared.txt",
        "a1 content",
        "commit a1",
        &[&main_commit],
    );
    let a1_commit = repo.find_commit(a1_id).unwrap();
    let a2_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a2.txt",
        "a2",
        "commit a2",
        &[&a1_commit],
    );
    let a2_commit = repo.find_commit(a2_id).unwrap();
    let _a3_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a3.txt",
        "a3",
        "commit a3",
        &[&a2_commit],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Create a conflicting change to a1's file that will conflict during autosquash rebase
    // First, modify a1 on main to create a conflict
    run_ok("git", &["checkout", "-f", "main"], dir.path());
    fs::write(dir.path().join("shared.txt"), "conflicting main change").unwrap();
    run_ok("git", &["add", "shared.txt"], dir.path());
    run_ok(
        "git",
        &["commit", "-m", "conflicting main commit"],
        dir.path(),
    );

    // Rebase feature-a onto new main to propagate the conflict state
    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    let rebase_result = std::process::Command::new("git")
        .args(["rebase", "main"])
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .unwrap();

    // Resolve the rebase conflict
    if !rebase_result.status.success() {
        fs::write(dir.path().join("shared.txt"), "resolved content").unwrap();
        run_ok("git", &["add", "shared.txt"], dir.path());
        let continue_status = std::process::Command::new("git")
            .args(["rebase", "--continue"])
            .current_dir(dir.path())
            .env("GIT_EDITOR", "true")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .unwrap();
        assert!(continue_status.success());
    }

    // Now create a fixup for a2 using interactive selection
    fs::write(dir.path().join("a2.txt"), "fixed a2").unwrap();
    run_ok("git", &["add", "a2.txt"], dir.path());

    // Make another conflicting change that will trigger during autosquash
    let old_a2_id = repo.revparse_single("feature-a~1").unwrap().id();
    let old_a2_commit = repo.find_commit(old_a2_id).unwrap();

    // Modify the file that a2 touches to ensure autosquash rebase will have a conflict
    run_ok(
        "git",
        &["checkout", "-f", &old_a2_commit.id().to_string()],
        dir.path(),
    );
    fs::write(dir.path().join("a2_v2.txt"), "conflicting a2 change").unwrap();
    run_ok("git", &["add", "a2_v2.txt"], dir.path());
    run_ok("git", &["commit", "-m", "conflict commit"], dir.path());
    let conflict_id = repo.revparse_single("HEAD").unwrap().id();

    // Force update a1 to include this conflict
    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    run_ok(
        "git",
        &["reset", "--hard", &conflict_id.to_string()],
        dir.path(),
    );

    // Re-add a2 and a3
    let new_a2_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a2_v2.txt",
        "a2",
        "commit a2",
        &[&repo.find_commit(conflict_id).unwrap()],
    );
    let new_a2_commit = repo.find_commit(new_a2_id).unwrap();
    make_commit(
        &repo,
        "refs/heads/feature-a",
        "a2_v2.txt",
        "a2 after a3",
        "commit a3",
        &[&new_a2_commit],
    );

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    // Stage a change and create a fixup for a2
    fs::write(dir.path().join("a2_v2.txt"), "fixed a2 v2").unwrap();
    run_ok("git", &["add", "a2_v2.txt"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--interactive")
        .current_dir(dir.path())
        .env("KIN_TEST_SELECTION", "1") // select a2
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure()
        .stderr(predicates::str::contains("rebase --autosquash failed"));

    // Verify the repo is in an interactive rebase state
    assert!(dir.path().join(".git/kindra_rebase_state.json").exists());
    assert!(
        dir.path().join(".git/rebase-merge").exists()
            || dir.path().join(".git/rebase-apply").exists()
    );

    // Resolve the conflict
    // The conflict file will vary based on git's rebase, so we just resolve generically
    let conflict_files = git_stdout(dir.path(), &["diff", "--name-only", "--diff-filter=U"]);
    for file in conflict_files.lines() {
        let file = file.trim();
        if !file.is_empty() {
            let resolved = if file == "a2_v2.txt" {
                "fixed a2 v2 after a3"
            } else {
                "resolved autosquash conflict"
            };
            fs::write(dir.path().join(file), resolved).unwrap();
            run_ok("git", &["add", file], dir.path());
        }
    }

    // Continue the rebase, resolving any follow-up conflicts introduced by autosquash replay.
    let mut continued = false;
    for _ in 0..3 {
        let output = kin_cmd()
            .arg("continue")
            .current_dir(dir.path())
            .env("GIT_EDITOR", "true")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .unwrap();
        if output.status.success() {
            continued = true;
            break;
        }

        let conflict_files = git_stdout(dir.path(), &["diff", "--name-only", "--diff-filter=U"]);
        assert!(
            !conflict_files.trim().is_empty(),
            "kin continue failed without unresolved conflicts.\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        for file in conflict_files.lines() {
            let file = file.trim();
            if !file.is_empty() {
                let resolved = if file == "a2_v2.txt" {
                    "fixed a2 v2 after a3"
                } else {
                    "resolved autosquash conflict"
                };
                fs::write(dir.path().join(file), resolved).unwrap();
                run_ok("git", &["add", file], dir.path());
            }
        }
    }
    assert!(
        continued,
        "kin continue did not finish the autosquash rebase"
    );

    // Verify state cleared
    assert!(!dir.path().join(".git/kindra_rebase_state.json").exists());
    assert!(!dir.path().join(".git/rebase-merge").exists());
    assert!(!dir.path().join(".git/rebase-apply").exists());

    // Verify the autosquash completed and no standalone fixup commit remains.
    let final_tip = repo.revparse_single("feature-a").unwrap().id();
    let mut commit = repo.find_commit(final_tip).unwrap();
    let mut summaries = vec![commit.summary().unwrap_or("").to_string()];
    while commit.parent_count() > 0 {
        commit = commit.parent(0).unwrap();
        summaries.push(commit.summary().unwrap_or("").to_string());
    }
    assert!(summaries.iter().any(|summary| summary == "commit a2"));
    assert!(
        !summaries
            .iter()
            .any(|summary| summary.starts_with("fixup! "))
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("a2_v2.txt")).unwrap(),
        "fixed a2 v2 after a3"
    );
}

#[test]
fn test_commit_interactive_stack_multiple_branches() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let a1_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a1.txt",
        "a1",
        "commit a1",
        &[&main_commit],
    );
    let a1_commit = repo.find_commit(a1_id).unwrap();
    let a2_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a2.txt",
        "a2",
        "commit a2",
        &[&a1_commit],
    );
    let a2_commit = repo.find_commit(a2_id).unwrap();
    let b1_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b1.txt",
        "b1",
        "commit b1",
        &[&a2_commit],
    );

    repo.set_head("refs/heads/feature-b").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("b1.txt"), "amended b1").unwrap();
    run_ok("git", &["add", "b1.txt"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--interactive")
        .current_dir(dir.path())
        .env("KIN_TEST_SELECTION", "0") // b1
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    let new_b_id = repo
        .find_reference("refs/heads/feature-b")
        .unwrap()
        .target()
        .unwrap();
    assert_ne!(new_b_id, b1_id);
    let new_b_commit = repo.find_commit(new_b_id).unwrap();
    assert_eq!(new_b_commit.message().unwrap(), "commit b1\n");
    assert_eq!(new_b_commit.parent_id(0).unwrap(), a2_id); // parent is still a2
}

#[test]
fn test_commit_interactive_picker_keeps_shared_head_tips_selectable() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let shared_tip_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "shared.txt",
        "shared",
        "shared tip",
        &[&main_commit],
    );

    repo.reference("refs/heads/feature-b", shared_tip_id, true, "test")
        .unwrap();

    repo.set_head("refs/heads/feature-b").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    let feature_a_before = repo
        .find_reference("refs/heads/feature-a")
        .unwrap()
        .target()
        .unwrap();
    let feature_b_before = repo
        .find_reference("refs/heads/feature-b")
        .unwrap()
        .target()
        .unwrap();
    assert_eq!(feature_a_before, feature_b_before);

    // Fold a staged change into the selected shared-head commit; both branches
    // share it, so both follow it to the folded version (asserted below), and its
    // message is unchanged (no reword).
    fs::write(dir.path().join("shared.txt"), "shared-fixed").unwrap();
    run_ok("git", &["add", "shared.txt"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--interactive")
        .current_dir(dir.path())
        .env("KIN_TEST_SELECTION", "1")
        .env("GIT_EDITOR", "false")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    let feature_a_after = repo
        .find_reference("refs/heads/feature-a")
        .unwrap()
        .target()
        .unwrap();
    let feature_b_after = repo
        .find_reference("refs/heads/feature-b")
        .unwrap()
        .target()
        .unwrap();

    // The shared-head tip is selectable, and the change folds into that commit
    // with its message unchanged. Both branches share the commit, so both follow
    // it to the folded version.
    assert_ne!(feature_a_after, feature_a_before);
    assert_eq!(feature_a_after, feature_b_after);
    let feature_a_commit = repo.find_commit(feature_a_after).unwrap();
    assert_eq!(feature_a_commit.summary().unwrap(), "shared tip");
    assert_eq!(
        blob_text(&repo, feature_a_after, "shared.txt"),
        "shared-fixed"
    );
}

#[test]
fn test_commit_interactive_stack_select_intermediate_from_child() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let a1_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a1.txt",
        "a1",
        "commit a1",
        &[&main_commit],
    );
    let a1_commit = repo.find_commit(a1_id).unwrap();
    let a2_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a2.txt",
        "a2",
        "commit a2",
        &[&a1_commit],
    );
    let a2_commit = repo.find_commit(a2_id).unwrap();

    let b1_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b1.txt",
        "b1",
        "commit b1",
        &[&a2_commit],
    );
    let b1_commit = repo.find_commit(b1_id).unwrap();
    let _b2_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b2.txt",
        "b2",
        "commit b2",
        &[&b1_commit],
    );

    repo.set_head("refs/heads/feature-b").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("a2.txt"), "fixed a2").unwrap();
    run_ok("git", &["add", "a2.txt"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--interactive")
        .current_dir(dir.path())
        .env("KIN_TEST_SELECTION", "2") // a2
        .env("GIT_EDITOR", "true")
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
    let new_b_id = repo
        .find_reference("refs/heads/feature-b")
        .unwrap()
        .target()
        .unwrap();

    // Feature-a should be updated
    assert_ne!(new_a_id, a2_id);
    let new_a2_commit = repo.find_commit(new_a_id).unwrap();
    assert_eq!(new_a2_commit.summary().unwrap(), "commit a2");
    assert_eq!(new_a2_commit.parent_id(0).unwrap(), a1_id);

    // Feature-b should be rebased on top of new Feature-a
    let tip_commit = repo.find_commit(new_b_id).unwrap();
    assert_eq!(tip_commit.summary().unwrap(), "commit b2");

    let new_b1_commit = tip_commit.parent(0).unwrap();
    assert_eq!(new_b1_commit.summary().unwrap(), "commit b1");
    assert_eq!(new_b1_commit.parent_id(0).unwrap(), new_a_id);
}

#[test]
fn test_commit_blocked_by_stale_run_state() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());
    make_commit(&repo, "refs/heads/main", "file.txt", "x", "initial", &[]);
    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // An interrupted `kin run` left run state behind.
    std::fs::write(
        dir.path().join(".git/kindra_run_state.json"),
        r#"{"target_branches":["main"],"current_index":0,"args":{"command":"false","continue_on_failure":false},"original_branch":"main","original_head_id":"0000000000000000000000000000000000000000","status":"failed"}"#,
    )
    .unwrap();

    // Stage a change so commit would otherwise proceed.
    fs::write(dir.path().join("file2.txt"), "y").unwrap();
    run_ok("git", &["add", "file2.txt"], dir.path());

    kin_cmd()
        .arg("commit")
        .arg("-m")
        .arg("should be blocked")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("already in progress"));

    // The interrupted run state must be left untouched.
    assert!(dir.path().join(".git/kindra_run_state.json").exists());
}

#[test]
fn test_commit_fixup_intermediate_rebases_descendants() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // feature-a: a1 -> a2
    let a1_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a1.txt",
        "a1",
        "commit a1",
        &[&main_commit],
    );
    let a1_commit = repo.find_commit(a1_id).unwrap();
    let a2_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a2.txt",
        "a2",
        "commit a2",
        &[&a1_commit],
    );
    let a2_commit = repo.find_commit(a2_id).unwrap();

    // feature-b on feature-a
    let _b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "commit b",
        &[&a2_commit],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("a1.txt"), "fixed a1").unwrap();
    run_ok("git", &["add", "a1.txt"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--fixup")
        .arg(a1_id.to_string())
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    // feature-a: the fixup squashed into a1 (subject unchanged), a2 preserved.
    let new_a_id = repo
        .find_reference("refs/heads/feature-a")
        .unwrap()
        .target()
        .unwrap();
    let a2_new = repo.find_commit(new_a_id).unwrap();
    assert_eq!(a2_new.summary().unwrap(), "commit a2");
    let a1_new = a2_new.parent(0).unwrap();
    assert_eq!(a1_new.summary().unwrap(), "commit a1");
    assert_ne!(a1_new.id(), a1_id); // a1 was rewritten by the fixup
    // The staged edit was actually folded into a1's blob, and a2 is untouched.
    assert_eq!(blob_text(&repo, a1_new.id(), "a1.txt"), "fixed a1");
    assert_eq!(blob_text(&repo, new_a_id, "a2.txt"), "a2");

    // feature-b rebased onto the new feature-a tip.
    let new_b_id = repo
        .find_reference("refs/heads/feature-b")
        .unwrap()
        .target()
        .unwrap();
    let new_b_commit = repo.find_commit(new_b_id).unwrap();
    assert_eq!(new_b_commit.parent_id(0).unwrap(), new_a_id);
    assert_eq!(new_b_commit.summary().unwrap(), "commit b");
    // The fold propagated down to feature-b, and b's own content is preserved.
    assert_eq!(blob_text(&repo, new_b_id, "a1.txt"), "fixed a1");
    assert_eq!(blob_text(&repo, new_b_id, "b.txt"), "b");
}

#[test]
fn test_commit_fixup_with_dash_a_commits_unstaged_changes() {
    // `--fixup <sha> -a` must not be rejected as "nothing to commit" when the
    // index is empty: `-a` supplies the content by staging tracked changes.
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let a1_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a1.txt",
        "a1",
        "commit a1",
        &[&main_commit],
    );
    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Modify a tracked file but do NOT stage it — only `-a` should pick it up.
    fs::write(dir.path().join("a1.txt"), "fixed a1").unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--fixup")
        .arg(a1_id.to_string())
        .arg("-a")
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    // The change was folded into the tip and the working tree is now clean.
    let new_a = repo
        .find_reference("refs/heads/feature-a")
        .unwrap()
        .target()
        .unwrap();
    assert_ne!(new_a, a1_id, "fixup -a should have rewritten the commit");
    // The unstaged edit `-a` picked up was actually folded into the commit blob.
    assert_eq!(blob_text(&repo, new_a, "a1.txt"), "fixed a1");
    // `git_stdout` asserts the status command itself succeeded before we inspect it.
    let dirty = git_stdout(dir.path(), &["status", "--porcelain"]);
    assert!(
        dirty.trim().is_empty(),
        "`-a` should have committed the change, leaving a clean tree"
    );
}

#[test]
fn test_commit_fixup_autosquash_conflict_with_dependents_can_continue() {
    // An autosquash conflict when the target has dependents must record the
    // in-progress branch so `kin continue` can resume — otherwise continue
    // rejects with "does not match saved state".
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // feature-a: a1 (creates shared.txt) -> a2 (modifies shared.txt).
    let a1_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "shared.txt",
        "line1\n",
        "commit a1",
        &[&main_commit],
    );
    let a1_commit = repo.find_commit(a1_id).unwrap();
    let a2_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "shared.txt",
        "line1\nline2\n",
        "commit a2",
        &[&a1_commit],
    );
    let a2_commit = repo.find_commit(a2_id).unwrap();

    // feature-b depends on feature-a, so the commit will rebase dependents.
    make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "commit b",
        &[&a2_commit],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Fixup a1 with a change to shared.txt that conflicts when a2 replays.
    fs::write(dir.path().join("shared.txt"), "CHANGED\n").unwrap();
    run_ok("git", &["add", "shared.txt"], dir.path());

    let conflict = kin_cmd()
        .arg("commit")
        .arg("--fixup")
        .arg(a1_id.to_string())
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .unwrap();
    assert!(
        !conflict.status.success(),
        "expected the autosquash to stop on a conflict"
    );

    // The fixup squash conflicts on shared.txt, and a2's replay conflicts on it
    // again — two conflict rounds. Resolve each and `kin continue`. The cap is a
    // small safety bound comfortably above those expected rounds, so a regression
    // that never converges fails loudly here instead of looping forever. The point
    // under test is that `kin continue` *resumes* at all — before the fix it
    // refused with "does not match saved state" because in_progress_branch was
    // left unset.
    const MAX_CONTINUE_ROUNDS: usize = 5;
    let mut resumed = false;
    let mut rounds = 0;
    while !resumed && rounds < MAX_CONTINUE_ROUNDS {
        rounds += 1;
        fs::write(dir.path().join("shared.txt"), "resolved\n").unwrap();
        run_ok("git", &["add", "-A"], dir.path());
        let out = kin_cmd()
            .arg("continue")
            .current_dir(dir.path())
            .env("GIT_EDITOR", "true")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .unwrap();
        resumed = out.status.success();
    }
    assert!(
        resumed,
        "kin continue should resolve the fixup/replay conflict sequence within {MAX_CONTINUE_ROUNDS} rounds (took {rounds})"
    );

    // feature-b ends up rebased onto the rewritten feature-a.
    let new_a = repo
        .find_reference("refs/heads/feature-a")
        .unwrap()
        .target()
        .unwrap();
    let new_b = repo
        .find_reference("refs/heads/feature-b")
        .unwrap()
        .target()
        .unwrap();
    let new_b_commit = repo.find_commit(new_b).unwrap();
    assert_eq!(new_b_commit.parent_id(0).unwrap(), new_a);
}

fn blob_text(repo: &git2::Repository, commit: git2::Oid, name: &str) -> String {
    let tree = repo.find_commit(commit).unwrap().tree().unwrap();
    let entry = tree.get_name(name).unwrap();
    let blob = repo
        .find_object(entry.id(), None)
        .unwrap()
        .peel_to_blob()
        .unwrap();
    String::from_utf8(blob.content().to_vec()).unwrap()
}

#[test]
fn test_commit_fixup_from_top_of_stack_folds_into_lower_branch() {
    // The case that previously failed: sit on the TOP of the stack and fix up a
    // commit owned by a lower branch. Option A folds it in place (no checkout of
    // the lower branch) and `--update-refs` moves every branch tip in the range.
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // main <- branch-a(shared="L1") <- branch-b(shared+="L2") <- branch-c(c.txt).
    // An intervening branch (branch-b) modifies the same file the fixup touches,
    // which is exactly what made the old checkout-carry approach hard-fail.
    let a_id = make_commit(
        &repo,
        "refs/heads/branch-a",
        "shared.txt",
        "L1\n",
        "A",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();
    let b_id = make_commit(
        &repo,
        "refs/heads/branch-b",
        "shared.txt",
        "L1\nL2\n",
        "B",
        &[&a_commit],
    );
    let b_commit = repo.find_commit(b_id).unwrap();
    make_commit(
        &repo,
        "refs/heads/branch-c",
        "c.txt",
        "c",
        "C",
        &[&b_commit],
    );

    // Sit on the top branch.
    repo.set_head("refs/heads/branch-c").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Prepend a line (folds into A) — disjoint from B's appended line, so the
    // autosquash 3-way merges cleanly even though B also touched shared.txt.
    fs::write(dir.path().join("shared.txt"), "L0\nL1\nL2\n").unwrap();
    run_ok("git", &["add", "shared.txt"], dir.path());

    kin_cmd()
        .arg("commit")
        .arg("--fixup")
        .arg(a_id.to_string())
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    // Never switched branches: HEAD is still branch-c.
    assert_eq!(
        repo.head().unwrap().shorthand().unwrap(),
        "branch-c",
        "inline fixup must not switch branches"
    );

    // The bottom commit was rewritten, and every branch above it moved.
    let new_a = repo
        .find_reference("refs/heads/branch-a")
        .unwrap()
        .target()
        .unwrap();
    assert_ne!(new_a, a_id, "the fixup target was rewritten");
    let new_c = repo
        .find_reference("refs/heads/branch-c")
        .unwrap()
        .target()
        .unwrap();
    let new_b = repo
        .find_reference("refs/heads/branch-b")
        .unwrap()
        .target()
        .unwrap();
    // Chain is intact: c -> b -> a.
    let c_commit = repo.find_commit(new_c).unwrap();
    assert_eq!(c_commit.parent_id(0).unwrap(), new_b);
    assert_eq!(
        repo.find_commit(new_b).unwrap().parent_id(0).unwrap(),
        new_a
    );
    // The prepend folded into A and B's append both survive at the top.
    assert_eq!(blob_text(&repo, new_c, "shared.txt"), "L0\nL1\nL2\n");
    assert_eq!(blob_text(&repo, new_c, "c.txt"), "c");
    // No fixup! commit leaked into history.
    assert_eq!(c_commit.summary().unwrap(), "C");
}

#[test]
fn test_commit_fixup_restores_autostash_on_single_branch_path() {
    // A fixup with no dependents to restack (autosquash_state_required) that also
    // has unstaged/untracked changes: those are set aside for the autosquash
    // rebase and must be reapplied afterward, with no stash left dangling.
    let (dir, repo) = setup_repo();
    // `feature` has one commit (feature.txt); add a second so there is a lower
    // commit to fix up while sitting on the tip with no dependents above it.
    let c1 = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    run_ok("git", &["checkout", "feature"], dir.path());
    fs::write(dir.path().join("c2.txt"), "c2").unwrap();
    run_ok("git", &["add", "c2.txt"], dir.path());
    run_ok("git", &["commit", "-m", "c2"], dir.path());

    // Staged fix folded into the lower commit...
    fs::write(dir.path().join("feature.txt"), "feature-fixed").unwrap();
    run_ok("git", &["add", "feature.txt"], dir.path());
    // ...plus an unrelated untracked change that must survive the fixup.
    fs::write(dir.path().join("scratch.txt"), "WIP").unwrap();

    kin_cmd()
        .arg("commit")
        .arg("--fixup")
        .arg(c1.to_string())
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    // The fold applied (feature.txt carries the fixed content)...
    assert_eq!(
        fs::read_to_string(dir.path().join("feature.txt")).unwrap(),
        "feature-fixed"
    );
    // ...the autostashed untracked change is restored onto the result...
    assert_eq!(
        fs::read_to_string(dir.path().join("scratch.txt")).unwrap(),
        "WIP",
        "the autostashed change must be reapplied after the fixup"
    );
    // ...no stash lingers, and no operation state is left behind.
    let stash_list = git_stdout(dir.path(), &["stash", "list"]);
    assert!(
        stash_list.trim().is_empty(),
        "the autostash must be dropped, not left dangling. Got:\n{stash_list}"
    );
    assert!(!dir.path().join(".git/kindra_rebase_state.json").exists());
}

#[test]
fn test_commit_fixup_from_middle_restacks_branches_above() {
    // From the MIDDLE of the stack, `--update-refs` moves the tips at/below HEAD,
    // and the branch stacked ABOVE HEAD is restacked onto the new HEAD afterward.
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/branch-a",
        "a.txt",
        "a",
        "A",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();
    let b_id = make_commit(
        &repo,
        "refs/heads/branch-b",
        "b.txt",
        "b",
        "B",
        &[&a_commit],
    );
    let b_commit = repo.find_commit(b_id).unwrap();
    make_commit(
        &repo,
        "refs/heads/branch-c",
        "c.txt",
        "c",
        "C",
        &[&b_commit],
    );

    // Sit on the middle branch.
    repo.set_head("refs/heads/branch-b").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("a.txt"), "a-fixed").unwrap();
    run_ok("git", &["add", "a.txt"], dir.path());

    kin_cmd()
        .arg("commit")
        .arg("--fixup")
        .arg(a_id.to_string())
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "branch-b");

    let new_a = repo
        .find_reference("refs/heads/branch-a")
        .unwrap()
        .target()
        .unwrap();
    let new_b = repo
        .find_reference("refs/heads/branch-b")
        .unwrap()
        .target()
        .unwrap();
    let new_c = repo
        .find_reference("refs/heads/branch-c")
        .unwrap()
        .target()
        .unwrap();
    // Below/at HEAD moved via --update-refs; above-HEAD (branch-c) restacked.
    assert_ne!(new_a, a_id);
    assert_eq!(
        repo.find_commit(new_b).unwrap().parent_id(0).unwrap(),
        new_a
    );
    assert_eq!(
        repo.find_commit(new_c).unwrap().parent_id(0).unwrap(),
        new_b,
        "branch-c (above HEAD) must be restacked onto the rewritten branch-b"
    );
    assert_eq!(blob_text(&repo, new_c, "a.txt"), "a-fixed");
    assert_eq!(blob_text(&repo, new_c, "c.txt"), "c");
}

#[test]
fn test_commit_fixup_abort_after_autosquash_restores_below_head_branch() {
    // The dangerous window: the inline autosquash SUCCEEDS (rewriting the
    // below-HEAD ancestor branch via --update-refs), then the above-HEAD restack
    // hits a conflict. `kin abort` must roll the fold back off the ancestor
    // branch too, not just HEAD's branch and its descendants.
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // branch-a creates shared.txt; branch-b (HEAD) only adds b.txt (so the
    // autosquash into branch-a replays cleanly through branch-b); branch-c
    // modifies shared.txt so restacking it onto the rewritten stack conflicts.
    let a_id = make_commit(
        &repo,
        "refs/heads/branch-a",
        "shared.txt",
        "line1\n",
        "A",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();
    let b_id = make_commit(
        &repo,
        "refs/heads/branch-b",
        "b.txt",
        "b",
        "B",
        &[&a_commit],
    );
    let b_commit = repo.find_commit(b_id).unwrap();
    let c_id = make_commit(
        &repo,
        "refs/heads/branch-c",
        "shared.txt",
        "from-c\n",
        "C",
        &[&b_commit],
    );

    // Sit on the middle branch.
    repo.set_head("refs/heads/branch-b").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("shared.txt"), "CHANGED\n").unwrap();
    run_ok("git", &["add", "shared.txt"], dir.path());

    let out = kin_cmd()
        .arg("commit")
        .arg("--fixup")
        .arg(a_id.to_string())
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected the above-HEAD restack of branch-c to stop on a conflict"
    );

    // The autosquash already completed, so branch-a is at its folded tip here.
    let folded_a = repo
        .find_reference("refs/heads/branch-a")
        .unwrap()
        .target()
        .unwrap();
    assert_ne!(folded_a, a_id, "branch-a should be folded at this point");

    kin_cmd()
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();

    // Every branch the operation touched — including the below-HEAD ancestor
    // branch-a — must be back at its pre-command tip.
    assert_eq!(
        repo.find_reference("refs/heads/branch-a")
            .unwrap()
            .target()
            .unwrap(),
        a_id,
        "abort must restore the below-HEAD ancestor branch, not leave it folded"
    );
    assert_eq!(
        repo.find_reference("refs/heads/branch-b")
            .unwrap()
            .target()
            .unwrap(),
        b_id
    );
    assert_eq!(
        repo.find_reference("refs/heads/branch-c")
            .unwrap()
            .target()
            .unwrap(),
        c_id
    );
    assert_eq!(blob_text(&repo, a_id, "shared.txt"), "line1\n");
}

#[test]
fn test_commit_interactive_fixup_lower_intermediate_from_above() {
    // The interactive equivalent of the top-of-stack fixup: pick an *intermediate*
    // commit that lives on a lower branch, while sitting on a higher branch. This
    // must use the same in-place fold path (no checkout of the lower branch).
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    // branch-a has two commits (a1 intermediate, a2 tip); branch-b sits on top.
    let a1_id = make_commit(
        &repo,
        "refs/heads/branch-a",
        "a1.txt",
        "a1",
        "A1",
        &[&main_commit],
    );
    let a1_commit = repo.find_commit(a1_id).unwrap();
    let a2_id = make_commit(
        &repo,
        "refs/heads/branch-a",
        "a2.txt",
        "a2",
        "A2",
        &[&a1_commit],
    );
    let a2_commit = repo.find_commit(a2_id).unwrap();
    make_commit(
        &repo,
        "refs/heads/branch-b",
        "b.txt",
        "b",
        "B",
        &[&a2_commit],
    );

    repo.set_head("refs/heads/branch-b").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Fold a change to a1's file into a1, chosen interactively from branch-b.
    fs::write(dir.path().join("a1.txt"), "a1-fixed").unwrap();
    run_ok("git", &["add", "a1.txt"], dir.path());

    // Commits newest-first: B(0), A2(1), A1(2) — select the lower intermediate A1.
    kin_cmd()
        .arg("commit")
        .arg("--interactive")
        .current_dir(dir.path())
        .env("KIN_TEST_SELECTION", "2")
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    // No branch switch, a1 rewritten in place, chain intact, change folded.
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "branch-b");
    let new_b = repo
        .find_reference("refs/heads/branch-b")
        .unwrap()
        .target()
        .unwrap();
    let new_a = repo
        .find_reference("refs/heads/branch-a")
        .unwrap()
        .target()
        .unwrap();
    let b_commit = repo.find_commit(new_b).unwrap();
    assert_eq!(b_commit.parent_id(0).unwrap(), new_a);
    assert_eq!(blob_text(&repo, new_b, "a1.txt"), "a1-fixed");
    assert_eq!(blob_text(&repo, new_b, "b.txt"), "b");
}

#[test]
fn test_commit_interactive_lower_tip_folds_without_reword() {
    // Selecting a lower branch's *tip* interactively behaves like any other pick:
    // it folds the staged changes in place (no reword, no editor, no checkout).
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/branch-a",
        "a.txt",
        "a",
        "orig A",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();
    make_commit(
        &repo,
        "refs/heads/branch-b",
        "b.txt",
        "b",
        "B",
        &[&a_commit],
    );
    repo.set_head("refs/heads/branch-b").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Stage a change to branch-a's file, then pick branch-a's tip. `false` as the
    // editor would fail if a reword editor were (wrongly) opened.
    fs::write(dir.path().join("a.txt"), "a-fixed").unwrap();
    run_ok("git", &["add", "a.txt"], dir.path());

    // Commits newest-first: B(0), A(1) — select branch-a's tip A.
    kin_cmd()
        .arg("commit")
        .arg("--interactive")
        .current_dir(dir.path())
        .env("KIN_TEST_SELECTION", "1")
        .env("GIT_EDITOR", "false")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    // No branch switch; change folded into branch-a with its message unchanged.
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "branch-b");
    let new_a = repo
        .find_reference("refs/heads/branch-a")
        .unwrap()
        .target()
        .unwrap();
    assert_ne!(new_a, a_id);
    assert_eq!(
        repo.find_commit(new_a).unwrap().summary().unwrap(),
        "orig A",
        "the message must not be reworded"
    );
    let new_b = repo
        .find_reference("refs/heads/branch-b")
        .unwrap()
        .target()
        .unwrap();
    assert_eq!(
        repo.find_commit(new_b).unwrap().parent_id(0).unwrap(),
        new_a
    );
    assert_eq!(blob_text(&repo, new_b, "a.txt"), "a-fixed");
}

#[test]
fn test_commit_fixup_tip_amends() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let a1_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a1.txt",
        "a1",
        "commit a1",
        &[&main_commit],
    );
    let a1_commit = repo.find_commit(a1_id).unwrap();
    let a2_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a2.txt",
        "a2",
        "commit a2",
        &[&a1_commit],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("a2.txt"), "amended a2").unwrap();
    run_ok("git", &["add", "a2.txt"], dir.path());

    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .arg("--fixup")
        .arg(a2_id.to_string())
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    // Fixing up the tip amends it: subject preserved, parent unchanged, id rewritten.
    let new_a_id = repo
        .find_reference("refs/heads/feature-a")
        .unwrap()
        .target()
        .unwrap();
    assert_ne!(new_a_id, a2_id);
    let new_a_commit = repo.find_commit(new_a_id).unwrap();
    assert_eq!(new_a_commit.summary().unwrap(), "commit a2");
    assert_eq!(new_a_commit.parent_id(0).unwrap(), a1_id);
}

#[test]
fn test_commit_fixup_sha_outside_stack_errors() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-a",
        "a1.txt",
        "a1",
        "commit a1",
        &[&main_commit],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    fs::write(dir.path().join("a2.txt"), "a2").unwrap();
    run_ok("git", &["add", "a2.txt"], dir.path());

    // The main commit is not part of the current stack (it's the base).
    kin_cmd()
        .arg("commit")
        .arg("--fixup")
        .arg(main_id.to_string())
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("not part of the current stack"));
}

#[test]
fn test_commit_fixup_and_on_are_mutually_exclusive() {
    let (dir, _repo) = setup_repo();

    kin_cmd()
        .arg("commit")
        .arg("--fixup")
        .arg("HEAD")
        .arg("--on")
        .arg("main")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("mutually exclusive"));
}

#[test]
fn test_commit_fixup_requires_argument() {
    let (dir, _repo) = setup_repo();

    kin_cmd()
        .arg("commit")
        .arg("--fixup")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("--fixup requires a commit"));
}

#[test]
fn test_commit_fixup_rejects_empty_argument() {
    let (dir, _repo) = setup_repo();

    // The space-separated form must reject an empty target too, matching the
    // `--fixup=` form, so an empty selector never reaches resolve_fixup_commit.
    kin_cmd()
        .arg("commit")
        .arg("--fixup")
        .arg("")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("--fixup requires a commit"));
}

/// feature-a with commits a1 <- a2 on top of main, checked out. Returns the
/// tip (a2) oid for --fixup targeting.
fn setup_feature_a_two_commits() -> (tempfile::TempDir, Repository, git2::Oid) {
    let (dir, repo) = setup_repo();
    // Scope the borrowed Commit handles so they drop before `repo` is returned.
    let a2_id = {
        let main_id = repo.revparse_single("main").unwrap().id();
        let main_commit = repo.find_commit(main_id).unwrap();
        let a1_id = make_commit(
            &repo,
            "refs/heads/feature-a",
            "a1.txt",
            "a1",
            "commit a1",
            &[&main_commit],
        );
        let a1_commit = repo.find_commit(a1_id).unwrap();
        make_commit(
            &repo,
            "refs/heads/feature-a",
            "a2.txt",
            "a2",
            "commit a2",
            &[&a1_commit],
        )
    };
    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
    (dir, repo, a2_id)
}

#[test]
fn test_commit_fixup_tip_does_not_open_editor() {
    // GIT_EDITOR=false would fail the command if an editor were opened, so
    // success proves `--fixup <tip>` inserts `--no-edit` (non-interactive).
    let (dir, repo, a2_id) = setup_feature_a_two_commits();
    fs::write(dir.path().join("a2.txt"), "amended a2").unwrap();
    run_ok("git", &["add", "a2.txt"], dir.path());

    kin_cmd()
        .arg("commit")
        .arg("--fixup")
        .arg(a2_id.to_string())
        .current_dir(dir.path())
        .env("GIT_EDITOR", "false")
        .assert()
        .success();

    let new_a = repo
        .find_reference("refs/heads/feature-a")
        .unwrap()
        .target()
        .unwrap();
    assert_ne!(new_a, a2_id);
    assert_eq!(
        repo.find_commit(new_a).unwrap().summary().unwrap(),
        "commit a2"
    );
}

#[test]
fn test_commit_fixup_equals_form_amends_tip() {
    // The `--fixup=<sha>` equals form must behave like the space form.
    let (dir, repo, a2_id) = setup_feature_a_two_commits();
    fs::write(dir.path().join("a2.txt"), "amended a2").unwrap();
    run_ok("git", &["add", "a2.txt"], dir.path());

    kin_cmd()
        .arg("commit")
        .arg(format!("--fixup={a2_id}"))
        .current_dir(dir.path())
        .env("GIT_EDITOR", "false")
        .assert()
        .success();

    let new_a = repo
        .find_reference("refs/heads/feature-a")
        .unwrap()
        .target()
        .unwrap();
    assert_ne!(new_a, a2_id);
}

#[test]
fn test_commit_fixup_tip_requires_staged_changes() {
    // Nothing staged: `--fixup <tip>` must fail loudly rather than silently
    // amend a no-op (the inserted `--amend` used to skip this guard).
    let (dir, _repo, a2_id) = setup_feature_a_two_commits();

    kin_cmd()
        .arg("commit")
        .arg("--fixup")
        .arg(a2_id.to_string())
        .current_dir(dir.path())
        .env("GIT_EDITOR", "false")
        .assert()
        .failure()
        .stderr(predicates::str::contains("nothing to commit"));
}

#[test]
fn test_commit_fixup_and_interactive_are_mutually_exclusive() {
    let (dir, _repo) = setup_repo();
    kin_cmd()
        .arg("commit")
        .arg("--fixup")
        .arg("HEAD")
        .arg("--interactive")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("mutually exclusive"));
}

/// A pick whose changes reach the index but whose commit fails (e.g. a
/// transient signing failure) leaves git refusing to continue: "you have
/// staged changes in your working tree". `kin continue` must repair that state
/// and drive the fold to completion.
#[cfg(unix)]
#[test]
fn test_continue_recovers_from_failed_pick_commit_during_fold() {
    let dir = tempdir().unwrap();
    let repo_path = dir.path();
    let repo = repo_init(repo_path);
    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    // An SSH signer that fails on exactly the third signature: the fixup
    // commit signs (1), the autosquash amend signs (2), and the follow-up
    // pick's commit fails (3) — reproducing a mid-fold transient signing
    // failure.
    let sign_dir = repo_path.join(".git/signer");
    fs::create_dir_all(&sign_dir).unwrap();
    let key = sign_dir.join("key");
    run_ok(
        "ssh-keygen",
        &["-q", "-t", "ed25519", "-N", "", "-f", key.to_str().unwrap()],
        repo_path,
    );
    let count_file = sign_dir.join("count");
    let wrapper = sign_dir.join("sshwrap");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ncount=$(cat {count} 2>/dev/null || echo 0)\ncount=$((count+1))\necho $count > {count}\nif [ $count -eq 3 ]; then echo 'sign failure simulated' >&2; exit 1; fi\nexec ssh-keygen \"$@\"\n",
            count = count_file.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    }
    run_ok("git", &["config", "commit.gpgsign", "true"], repo_path);
    run_ok("git", &["config", "gpg.format", "ssh"], repo_path);
    run_ok(
        "git",
        &[
            "config",
            "user.signingkey",
            &format!("{}.pub", key.display()),
        ],
        repo_path,
    );
    run_ok(
        "git",
        &["config", "gpg.ssh.program", wrapper.to_str().unwrap()],
        repo_path,
    );

    // main: base, feat: target (f.txt) then clean-pick (g.txt).
    let main_oid = make_commit(&repo, "HEAD", "f.txt", "a\n", "base", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);
    run_ok("git", &["checkout", "-b", "feat"], repo_path);
    let target_oid = make_commit(
        &repo,
        "HEAD",
        "f.txt",
        "b\n",
        "target",
        &[&repo.find_commit(main_oid).unwrap()],
    );
    make_commit(
        &repo,
        "HEAD",
        "g.txt",
        "g\n",
        "clean-pick",
        &[&repo.find_commit(target_oid).unwrap()],
    );

    // Stage a change for the target commit and fold it. The fold's follow-up
    // pick fails to commit (signature 3), stranding its changes staged.
    fs::write(repo_path.join("f.txt"), "B\n").unwrap();
    run_ok("git", &["add", "f.txt"], repo_path);
    let mut cmd = kin_cmd();
    cmd.current_dir(repo_path)
        .args(["commit", "--fixup", &target_oid.to_string()])
        .assert()
        .failure();
    assert!(
        repo_path.join(".git/rebase-merge").exists(),
        "the fold must be stranded mid-rebase"
    );
    assert!(
        !repo_path.join(".git/rebase-merge/message").exists(),
        "the stall must be the degenerate no-pending-commit state"
    );

    // Signing works again; kin continue must repair the state, commit the
    // stranded pick, skip its empty replay, and finish.
    fs::write(&count_file, "-100\n").unwrap();
    let mut cmd = kin_cmd();
    let output = cmd.current_dir(repo_path).arg("continue").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "continue must recover\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        stdout.contains("Restored the rebase state"),
        "recovery must announce itself\nstdout:\n{}",
        stdout
    );

    // The fold completed: target contains the fixup content, clean-pick sits
    // on top exactly once, nothing is left behind.
    assert!(!repo_path.join(".git/rebase-merge").exists());
    assert!(!repo_path.join(".git/kindra_rebase_state.json").exists());
    let log = std::process::Command::new("git")
        .args(["log", "--format=%s", "main..feat"])
        .current_dir(repo_path)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&log.stdout).trim(),
        "clean-pick\ntarget",
        "the fold must complete with each commit exactly once"
    );
    let folded = std::process::Command::new("git")
        .args(["show", "feat~1:f.txt"])
        .current_dir(repo_path)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&folded.stdout), "B\n");
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_path)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&status.stdout), "");
}

/// A conflicted restore of the set-aside stash at the end of `kin commit --on`
/// must leave the operation resumable and abortable, not clear the state out
/// from under the user.
#[test]
fn test_commit_on_conflicted_stash_restore_stays_resumable() {
    let dir = tempdir().unwrap();
    let repo_path = dir.path();
    let repo = repo_init(repo_path);
    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    let main_oid = make_commit(&repo, "HEAD", "f.txt", "line1\nline2\n", "base", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);
    run_ok("git", &["checkout", "-b", "lower"], repo_path);
    let lower_oid = make_commit(
        &repo,
        "HEAD",
        "l.txt",
        "l",
        "lower work",
        &[&repo.find_commit(main_oid).unwrap()],
    );
    run_ok("git", &["checkout", "-b", "upper"], repo_path);
    make_commit(
        &repo,
        "HEAD",
        "u.txt",
        "u",
        "upper work",
        &[&repo.find_commit(lower_oid).unwrap()],
    );

    // Stage a change destined for 'lower', with an overlapping unstaged edit
    // that gets set aside. After the commit lands and 'upper' is restacked,
    // restoring the set-aside edit conflicts with the committed line.
    fs::write(repo_path.join("f.txt"), "from-commit\nline2\n").unwrap();
    run_ok("git", &["add", "f.txt"], repo_path);
    fs::write(repo_path.join("f.txt"), "unstaged-edit\nline2\n").unwrap();

    let mut cmd = kin_cmd();
    let output = cmd
        .current_dir(repo_path)
        .args(["commit", "--on", "lower", "-m", "folded into lower"])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "the conflicted stash restore must surface as an error\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("kin continue") && stderr.contains("kin abort"),
        "the error must offer both continue and abort\nstderr:\n{}",
        stderr
    );

    // The regression: the saved state must survive so continue/abort work.
    assert!(
        repo_path.join(".git/kindra_rebase_state.json").exists(),
        "state must be preserved after a conflicted stash restore"
    );
    let stashes = std::process::Command::new("git")
        .args(["stash", "list"])
        .current_dir(repo_path)
        .output()
        .unwrap();
    assert!(
        !stashes.stdout.is_empty(),
        "the stash entry must be preserved as a backup"
    );

    // The commit itself landed and upper follows it.
    let lower_tip_msg = std::process::Command::new("git")
        .args(["log", "-1", "--format=%s", "lower"])
        .current_dir(repo_path)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&lower_tip_msg.stdout).trim(),
        "folded into lower"
    );

    // Resolve the conflict markers and finish with kin continue.
    fs::write(repo_path.join("f.txt"), "resolved\nline2\n").unwrap();
    run_ok("git", &["add", "f.txt"], repo_path);
    let mut cmd = kin_cmd();
    cmd.current_dir(repo_path)
        .arg("continue")
        .assert()
        .success();
    assert!(
        !repo_path.join(".git/kindra_rebase_state.json").exists(),
        "continue must complete the operation"
    );
    let head_name = repo.head().unwrap().shorthand().unwrap().to_string();
    assert_eq!(
        head_name, "upper",
        "we must end up back on the caller branch"
    );
}

/// Stage `content` into `file` under `dir`.
fn stage(dir: &Path, file: &str, content: &str) {
    fs::write(dir.join(file), content).unwrap();
    run_ok("git", &["add", file], dir);
}

/// A `kin commit` invocation with the git identity/editor env pinned so it never
/// blocks on an editor.
fn kin_commit(dir: &Path) -> assert_cmd::Command {
    let mut cmd = kin_cmd();
    cmd.arg("commit")
        .current_dir(dir)
        .env("GIT_EDITOR", "true")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com");
    cmd
}

fn checkout(repo: &Repository, branch: &str) {
    repo.set_head(&format!("refs/heads/{branch}")).unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
}

/// `kin commit -b <name>` forks a new branch off the current commit: the new
/// branch carries the commit, the current branch is untouched, and HEAD ends on
/// the new branch — the `git checkout -b && commit` shortcut.
#[test]
fn test_commit_new_branch_forks_from_current() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();

    stage(dir.path(), "x.txt", "x");
    kin_commit(dir.path())
        .arg("-b")
        .arg("topic")
        .arg("-m")
        .arg("add x")
        .assert()
        .success();

    let topic_id = repo.revparse_single("topic").unwrap().id();
    let topic = repo.find_commit(topic_id).unwrap();
    assert_eq!(
        topic.parent_id(0).unwrap(),
        main_id,
        "new branch forks off the current commit"
    );
    assert_eq!(
        repo.revparse_single("main").unwrap().id(),
        main_id,
        "the current branch must not move"
    );
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "topic");
}

/// With no explicit name, the branch is slugified from the commit subject.
#[test]
fn test_commit_new_branch_slugifies_message() {
    let (dir, _repo) = setup_repo();

    stage(dir.path(), "x.txt", "x");
    kin_commit(dir.path())
        .arg("--new-branch")
        .arg("-m")
        .arg("Add cool parser!")
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();
    assert!(
        repo.find_branch("add-cool-parser", git2::BranchType::Local)
            .is_ok(),
        "branch name should be slugified from the subject"
    );
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "add-cool-parser");
}

/// `--insert` splices the new branch into the stack: children of the current
/// branch are restacked onto it, forming current -> new -> children.
#[test]
fn test_commit_new_branch_insert_restacks_children() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "commit a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();
    make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "commit b",
        &[&a_commit],
    );

    checkout(&repo, "feature-a");
    stage(dir.path(), "mid.txt", "mid");
    kin_commit(dir.path())
        .arg("-b")
        .arg("mid")
        .arg("--insert")
        .arg("-m")
        .arg("mid commit")
        .assert()
        .success();

    // feature-a stays put; mid forks off it; feature-b is restacked onto mid.
    assert_eq!(
        repo.revparse_single("feature-a").unwrap().id(),
        a_id,
        "the branch we inserted below must not move"
    );
    let mid_id = repo.revparse_single("mid").unwrap().id();
    let mid = repo.find_commit(mid_id).unwrap();
    assert_eq!(mid.parent_id(0).unwrap(), a_id);

    let new_b_id = repo.revparse_single("feature-b").unwrap().id();
    let new_b = repo.find_commit(new_b_id).unwrap();
    assert_eq!(
        new_b.parent_id(0).unwrap(),
        mid_id,
        "child must be restacked onto the inserted branch"
    );
    assert_eq!(new_b.message().unwrap(), "commit b");
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "mid");
}

/// `--insert` on a branch with no children takes the "nothing to move" early
/// return: it creates the branch with the commit, runs no restack, and leaves no
/// saved operation state.
#[test]
fn test_commit_new_branch_insert_with_no_children_just_commits() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();
    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "commit a",
        &[&main_commit],
    );

    checkout(&repo, "feature-a");
    stage(dir.path(), "mid.txt", "mid");
    kin_commit(dir.path())
        .arg("-b")
        .arg("mid")
        .arg("--insert")
        .arg("-m")
        .arg("mid commit")
        .assert()
        .success();

    let mid_id = repo.revparse_single("mid").unwrap().id();
    assert_eq!(
        repo.find_commit(mid_id).unwrap().parent_id(0).unwrap(),
        a_id,
        "mid forks off the childless branch"
    );
    assert_eq!(
        repo.revparse_single("feature-a").unwrap().id(),
        a_id,
        "feature-a must not move"
    );
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "mid");
    assert!(
        !dir.path().join(".git/kindra_rebase_state.json").exists(),
        "no children means no restack, so no state is saved"
    );
}

/// Set up an `--insert` whose child restack conflicts, and stop mid-rebase.
/// Returns feature-a's commit id (feature-b's original parent).
fn setup_insert_conflict(dir: &Path, repo: &Repository) -> git2::Oid {
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();
    let a_id = make_commit(
        repo,
        "refs/heads/feature-a",
        "shared.txt",
        "original\n",
        "commit a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();
    make_commit(
        repo,
        "refs/heads/feature-b",
        "shared.txt",
        "feature b\n",
        "commit b",
        &[&a_commit],
    );

    checkout(repo, "feature-a");
    // The inserted commit rewrites shared.txt, so restacking feature-b onto it
    // conflicts on that file.
    stage(dir, "shared.txt", "mid change\n");
    kin_commit(dir)
        .arg("-b")
        .arg("mid")
        .arg("--insert")
        .arg("-m")
        .arg("mid commit")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Resolve conflicts"));
    assert!(
        dir.join(".git/kindra_rebase_state.json").exists(),
        "a conflicting insert restack must leave resumable state"
    );
    a_id
}

/// A conflict during the `--insert` restack is resumable: resolve it, `kin
/// continue`, and the child lands on the inserted branch.
#[test]
fn test_commit_new_branch_insert_conflict_recovers_with_continue() {
    let (dir, repo) = setup_repo();
    setup_insert_conflict(dir.path(), &repo);

    fs::write(dir.path().join("shared.txt"), "resolved\n").unwrap();
    run_ok("git", &["add", "shared.txt"], dir.path());
    kin_cmd()
        .arg("continue")
        .current_dir(dir.path())
        .assert()
        .success();

    assert!(!dir.path().join(".git/kindra_rebase_state.json").exists());
    let mid_id = repo.revparse_single("mid").unwrap().id();
    let new_b = repo
        .find_commit(repo.revparse_single("feature-b").unwrap().id())
        .unwrap();
    assert_eq!(
        new_b.parent_id(0).unwrap(),
        mid_id,
        "feature-b must be restacked onto the inserted branch after continue"
    );
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "mid");
}

/// A conflict during the `--insert` restack is abortable: `kin abort` clears the
/// state and restores the child to its pre-insert parent.
#[test]
fn test_commit_new_branch_insert_conflict_recovers_with_abort() {
    let (dir, repo) = setup_repo();
    let a_id = setup_insert_conflict(dir.path(), &repo);

    kin_cmd()
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();

    assert!(!dir.path().join(".git/kindra_rebase_state.json").exists());
    let new_b = repo
        .find_commit(repo.revparse_single("feature-b").unwrap().id())
        .unwrap();
    assert_eq!(
        new_b.parent_id(0).unwrap(),
        a_id,
        "abort must restore feature-b to its pre-insert parent"
    );

    // Only the restack is rolled back: the inserted branch survives with the
    // already-applied commit on top of feature-a, and we end up on it (it is
    // the operation's original_branch).
    assert_eq!(
        repo.head().unwrap().shorthand().unwrap(),
        "mid",
        "abort must leave HEAD on the inserted branch"
    );
    let mid = repo
        .find_commit(repo.revparse_single("mid").unwrap().id())
        .unwrap();
    assert_eq!(
        mid.summary().unwrap(),
        "mid commit",
        "the inserted branch must keep the applied commit"
    );
    assert_eq!(
        mid.parent_id(0).unwrap(),
        a_id,
        "the inserted commit must remain on top of feature-a"
    );
}

/// Without `--insert`, a new branch is a sibling fork: existing children stay on
/// the current branch and are not moved.
#[test]
fn test_commit_new_branch_fork_leaves_children_in_place() {
    let (dir, repo) = setup_repo();
    let main_id = repo.revparse_single("main").unwrap().id();
    let main_commit = repo.find_commit(main_id).unwrap();

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
        "b",
        "commit b",
        &[&a_commit],
    );

    checkout(&repo, "feature-a");
    stage(dir.path(), "sib.txt", "sib");
    kin_commit(dir.path())
        .arg("-b")
        .arg("sibling")
        .arg("-m")
        .arg("sibling commit")
        .assert()
        .success();

    assert_eq!(repo.revparse_single("feature-a").unwrap().id(), a_id);
    assert_eq!(
        repo.revparse_single("feature-b").unwrap().id(),
        b_id,
        "a fork must not restack existing children"
    );
    let sibling_id = repo.revparse_single("sibling").unwrap().id();
    assert_eq!(
        repo.find_commit(sibling_id).unwrap().parent_id(0).unwrap(),
        a_id,
    );
}

/// `--insert` without `--new-branch` is rejected up front.
#[test]
fn test_commit_insert_without_new_branch_errors() {
    let (dir, _repo) = setup_repo();
    stage(dir.path(), "x.txt", "x");
    kin_commit(dir.path())
        .arg("--insert")
        .arg("-m")
        .arg("x")
        .assert()
        .failure()
        .stderr(predicates::str::contains("--insert requires --new-branch"));
}

/// `-b` with neither an explicit name nor a message to slugify is rejected before
/// any branch is created.
#[test]
fn test_commit_new_branch_without_name_or_message_errors() {
    let (dir, repo) = setup_repo();
    stage(dir.path(), "x.txt", "x");
    kin_commit(dir.path())
        .arg("-b")
        .assert()
        .failure()
        .stderr(predicates::str::contains("needs a branch name"));
    // No stray branch was created and HEAD is untouched.
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "main");
}

/// `-b` onto an existing branch name is rejected.
#[test]
fn test_commit_new_branch_existing_name_errors() {
    let (dir, _repo) = setup_repo();
    stage(dir.path(), "x.txt", "x");
    kin_commit(dir.path())
        .arg("-b")
        .arg("feature")
        .arg("-m")
        .arg("x")
        .assert()
        .failure()
        .stderr(predicates::str::contains("already exists"));
}

/// When `git commit` fails after the branch is created, the branch creation is
/// rolled back: HEAD returns to the original branch and the new branch is gone.
#[cfg(unix)]
#[test]
fn test_commit_new_branch_rolls_back_on_failed_commit() {
    use std::os::unix::fs::PermissionsExt;

    let (dir, repo) = setup_repo();
    // A failing pre-commit hook makes `git commit` fail *after* the branch exists.
    let hook = dir.path().join(".git/hooks/pre-commit");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    stage(dir.path(), "x.txt", "x");
    kin_commit(dir.path())
        .arg("-b")
        .arg("topic")
        .arg("-m")
        .arg("x")
        .assert()
        .failure();

    assert!(
        repo.find_branch("topic", git2::BranchType::Local).is_err(),
        "failed commit must not leave the created branch behind"
    );
    assert_eq!(
        repo.head().unwrap().shorthand().unwrap(),
        "main",
        "rollback must return to the original branch"
    );
}

/// When a slugified branch name already exists, the derived name is
/// disambiguated with a numeric suffix (base, base-2, …).
#[test]
fn test_commit_new_branch_slug_collision_gets_numeric_suffix() {
    let (dir, repo) = setup_repo();

    stage(dir.path(), "one.txt", "one");
    kin_commit(dir.path())
        .arg("-b")
        .arg("-m")
        .arg("Shared subject")
        .assert()
        .success();
    assert!(
        repo.find_branch("shared-subject", git2::BranchType::Local)
            .is_ok()
    );

    // A second commit whose subject slugs to the same base must not collide with
    // the first branch; it should be disambiguated to `shared-subject-2`.
    checkout(&repo, "main");
    stage(dir.path(), "two.txt", "two");
    kin_commit(dir.path())
        .arg("-b")
        .arg("-m")
        .arg("Shared subject")
        .assert()
        .success();
    assert!(
        repo.find_branch("shared-subject-2", git2::BranchType::Local)
            .is_ok(),
        "second slug collision should be disambiguated to shared-subject-2"
    );
    assert_eq!(
        repo.head().unwrap().shorthand().unwrap(),
        "shared-subject-2"
    );
}
