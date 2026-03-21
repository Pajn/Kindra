mod common;

use common::{kin_cmd, make_commit, repo_init, run_ok};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_sync_aborts_when_branch_checked_out_in_other_worktree() {
    let dir = TempDir::new().unwrap();
    let repo = repo_init(dir.path());
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Test User").unwrap();
    config.set_str("user.email", "test@example.com").unwrap();

    // Setup: main -> feature-a -> feature-b
    let main_id = make_commit(&repo, "refs/heads/main", "main.txt", "main", "initial", &[]);
    let main_commit = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature-a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feature-b",
        &[&a_commit],
    );

    // Create a second worktree and checkout feature-a there
    let wt_dir = TempDir::new().unwrap();
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

    // Run kin sync on feature-b (in the main worktree)
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let output = kin_cmd()
        .arg("sync")
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("feature-a is checked out in"));
    assert!(stderr.contains(
        "aborting as a full rebase can not be completed. Use --force to ignore this check."
    ));

    // Verify it passes with --force
    let output = kin_cmd()
        .arg("sync")
        .arg("--force")
        .current_dir(dir.path())
        .output()
        .unwrap();

    // It might still fail if git rebase itself fails due to worktree, but the GITS check should be bypassed.
    // Actually, git rebase --update-refs might not care if it's not checking out feature-a,
    // but kin sync might try to checkout tips.

    // In this case, kin sync will try to checkout feature-b (already on it).
    // Then it runs git rebase --update-refs.

    // If it passed the kin check, it means --force worked.
    assert!(
        !String::from_utf8_lossy(&output.stderr)
            .contains("aborting as a full rebase can not be completed")
    );
}

#[test]
fn test_move_aborts_when_branch_checked_out_in_other_worktree() {
    let dir = TempDir::new().unwrap();
    let repo = repo_init(dir.path());
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Test User").unwrap();
    config.set_str("user.email", "test@example.com").unwrap();

    // Setup: main -> feature-a -> feature-b
    let main_id = make_commit(&repo, "refs/heads/main", "main.txt", "main", "initial", &[]);
    let main_commit = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature-a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feature-b",
        &[&a_commit],
    );

    // Target branch
    make_commit(
        &repo,
        "refs/heads/target",
        "target.txt",
        "target",
        "target",
        &[&main_commit],
    );

    // Create a second worktree and checkout feature-b there
    let wt_dir = TempDir::new().unwrap();
    run_ok(
        "git",
        &[
            "worktree",
            "add",
            wt_dir.path().to_str().unwrap(),
            "feature-b",
        ],
        dir.path(),
    );

    // Run kin move on feature-a (in the main worktree)
    run_ok("git", &["checkout", "feature-a"], dir.path());

    let output = kin_cmd()
        .arg("move")
        .arg("--onto")
        .arg("target")
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("feature-b is checked out in"));

    // Verify it passes with --force
    let output = kin_cmd()
        .arg("move")
        .arg("--onto")
        .arg("target")
        .arg("--force")
        .current_dir(dir.path())
        .output()
        .unwrap();

    // It might still fail at the git rebase step because git itself refuses to rebase a branch checked out elsewhere.
    let stderr_force = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr_force.contains("aborting as a full rebase can not be completed"));
}

#[test]
fn test_move_onto_descendant_aborts_when_checked_out_in_other_worktree() {
    let dir = TempDir::new().unwrap();
    let repo = repo_init(dir.path());
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Test User").unwrap();
    config.set_str("user.email", "test@example.com").unwrap();

    let main_id = make_commit(&repo, "refs/heads/main", "main.txt", "main", "initial", &[]);
    let main_commit = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature-a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();

    let b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feature-b",
        &[&a_commit],
    );
    let b_commit = repo.find_commit(b_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-c",
        "c.txt",
        "c",
        "feature-c",
        &[&b_commit],
    );

    let wt_dir = TempDir::new().unwrap();
    run_ok(
        "git",
        &[
            "worktree",
            "add",
            wt_dir.path().to_str().unwrap(),
            "feature-c",
        ],
        dir.path(),
    );

    run_ok("git", &["checkout", "feature-a"], dir.path());

    let output = kin_cmd()
        .arg("move")
        .arg("--onto")
        .arg("feature-c")
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("feature-c is checked out in"));

    let output = kin_cmd()
        .arg("move")
        .arg("--onto")
        .arg("feature-c")
        .arg("--force")
        .current_dir(dir.path())
        .output()
        .unwrap();

    let stderr_force = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr_force.contains("aborting as a full rebase can not be completed"));
}

#[test]
fn test_commit_on_aborts_when_branch_checked_out_in_other_worktree() {
    let dir = TempDir::new().unwrap();
    let repo = repo_init(dir.path());
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Test User").unwrap();
    config.set_str("user.email", "test@example.com").unwrap();

    // Setup: main -> feature-a -> feature-b
    let main_id = make_commit(&repo, "refs/heads/main", "main.txt", "main", "initial", &[]);
    let main_commit = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature-a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feature-b",
        &[&a_commit],
    );

    // Create a second worktree and checkout feature-b there
    let wt_dir = TempDir::new().unwrap();
    run_ok(
        "git",
        &[
            "worktree",
            "add",
            wt_dir.path().to_str().unwrap(),
            "feature-b",
        ],
        dir.path(),
    );

    // Run kin commit --on feature-a (in the main worktree)
    run_ok("git", &["checkout", "feature-a"], dir.path());
    fs::write(dir.path().join("a.txt"), "modified a").unwrap();
    run_ok("git", &["add", "a.txt"], dir.path());

    let output = kin_cmd()
        .arg("commit")
        .arg("-m")
        .arg("msg")
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("feature-b is checked out in"));

    // Verify it passes with --force
    let output = kin_cmd()
        .arg("commit")
        .arg("--force")
        .arg("-m")
        .arg("msg")
        .current_dir(dir.path())
        .output()
        .unwrap();

    let stderr_force = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr_force.contains("aborting as a full rebase can not be completed"));
}

#[test]
fn test_commit_on_with_branch_switch_aborts_due_to_worktree_and_restores() {
    let dir = TempDir::new().unwrap();
    let repo = repo_init(dir.path());
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Test User").unwrap();
    config.set_str("user.email", "test@example.com").unwrap();

    // Setup: main -> feature-a -> feature-b -> feature-c
    let main_id = make_commit(&repo, "refs/heads/main", "main.txt", "main", "initial", &[]);
    let main_commit = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_id).unwrap();

    let b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "b",
        &[&a_commit],
    );
    let b_commit = repo.find_commit(b_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-c",
        "c.txt",
        "c",
        "c",
        &[&b_commit],
    );

    // Create a second worktree and checkout feature-c there
    let wt_dir = TempDir::new().unwrap();
    run_ok(
        "git",
        &[
            "worktree",
            "add",
            wt_dir.path().to_str().unwrap(),
            "feature-c",
        ],
        dir.path(),
    );

    // On main worktree, checkout feature-a
    run_ok("git", &["checkout", "feature-a"], dir.path());

    // Make an unstaged change
    fs::write(dir.path().join("unstaged.txt"), "unstaged content").unwrap();

    // Run kin commit --on feature-b
    // This should trigger check_worktrees because feature-b has dependent feature-c in another worktree
    let output = kin_cmd()
        .arg("commit")
        .arg("--on")
        .arg("feature-b")
        .arg("-m")
        .arg("msg")
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("feature-c is checked out in"));

    // Afterwards assert the working branch is still "feature-a" and the unstaged change is present
    let repo = git2::Repository::open(dir.path()).unwrap();
    let head = repo.head().unwrap();
    assert_eq!(head.shorthand().unwrap(), "feature-a");
    assert!(dir.path().join("unstaged.txt").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("unstaged.txt")).unwrap(),
        "unstaged content"
    );
}

#[test]
fn test_commit_force_with_rebase_conflict() {
    let dir = TempDir::new().unwrap();
    let repo = repo_init(dir.path());
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Test User").unwrap();
    config.set_str("user.email", "test@example.com").unwrap();

    // Setup: main -> feature-a -> feature-b using standard git commands
    run_ok("git", &["checkout", "-b", "main"], dir.path());
    fs::write(dir.path().join("file.txt"), "initial").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok("git", &["commit", "-m", "initial"], dir.path());

    run_ok("git", &["checkout", "-b", "feature-a"], dir.path());
    fs::write(dir.path().join("file.txt"), "base").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature-a"], dir.path());

    run_ok("git", &["checkout", "-b", "feature-b"], dir.path());
    fs::write(dir.path().join("file_b.txt"), "b-change").unwrap();
    run_ok("git", &["add", "file_b.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature-b"], dir.path());

    run_ok("git", &["checkout", "main"], dir.path());

    // Create a second worktree and checkout feature-b there
    let wt_dir = TempDir::new().unwrap();
    let wt_path = wt_dir.path().to_str().unwrap().to_string();
    run_ok(
        "git",
        &["worktree", "add", &wt_path, "feature-b"],
        dir.path(),
    );

    // Run kin commit --force on main (in the main worktree)
    // This should trigger a rebase of feature-a and feature-b since main moved
    run_ok("git", &["checkout", "main"], dir.path());
    fs::write(dir.path().join("file.txt"), "main-change").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());

    let output = kin_cmd()
        .arg("commit")
        .arg("-m")
        .arg("new main")
        .arg("--force")
        .current_dir(dir.path())
        .output()
        .unwrap();

    // The kin check should be bypassed, but git rebase should fail due to conflict
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("aborting as a full rebase can not be completed"));

    // Verify it enters a rebase/merge conflict state
    let repo = git2::Repository::open(dir.path()).unwrap();
    assert!(repo.state() != git2::RepositoryState::Clean);

    // Resolve the conflict
    fs::write(dir.path().join("file.txt"), "resolved").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());

    // Remove the other worktree so git rebase can succeed
    run_ok("git", &["worktree", "remove", "-f", &wt_path], dir.path());

    // Continue the rebase
    let res = kin_cmd()
        .arg("continue")
        .env("GIT_EDITOR", "true")
        .current_dir(dir.path())
        .output()
        .unwrap();
    if !res.status.success() {
        eprintln!("kin continue failed!");
        eprintln!("STDOUT: {}", String::from_utf8_lossy(&res.stdout));
        eprintln!("STDERR: {}", String::from_utf8_lossy(&res.stderr));
    }
    assert!(res.status.success());

    // Verify postcondition: repo is clean and kin status shows no operation in progress
    let status_output = kin_cmd()
        .arg("status")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        status_output.status.success(),
        "kin status failed!\nSTDOUT: {}\nSTDERR: {}",
        String::from_utf8_lossy(&status_output.stdout),
        String::from_utf8_lossy(&status_output.stderr)
    );
    let status_stdout = String::from_utf8_lossy(&status_output.stdout);
    assert!(!status_stdout.contains("operation in progress"));
}
