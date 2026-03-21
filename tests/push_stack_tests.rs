//! Integration tests for kin push ensuring it pushes the whole stack.

mod common;
use common::{kin_cmd, make_commit, repo_init, run_ok};
use git2::Repository;
use tempfile::tempdir;

#[test]
fn test_push_entire_stack() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    // 1. Initial commit on main
    let main_commit_id = make_commit(
        &repo,
        "refs/heads/main",
        "main.txt",
        "initial",
        "initial commit",
        &[],
    );
    let main_commit = repo.find_commit(main_commit_id).unwrap();

    // 2. feature-a on top of main
    let a_commit_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feat: a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_commit_id).unwrap();

    // 3. feature-b on top of feature-a
    make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feat: b",
        &[&a_commit],
    );

    // Set up a bare remote
    let remote_dir = tempdir().unwrap();
    run_ok("git", &["init", "--bare"], remote_dir.path());

    run_ok(
        "git",
        &[
            "remote",
            "add",
            "origin",
            remote_dir.path().to_str().unwrap(),
        ],
        dir.path(),
    );

    // Checkout feature-a. If we push from here, it should push feature-b too!
    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Run kin push
    // It will prompt for branches without upstream.
    // We can use a non-interactive way if we set up upstreams manually first,
    // OR we can pipe input.

    // Let's set up upstreams for both to test the "push branches on top of me" logic
    run_ok("git", &["push", "-u", "origin", "main"], dir.path());
    run_ok("git", &["push", "-u", "origin", "feature-a"], dir.path());
    run_ok("git", &["push", "-u", "origin", "feature-b"], dir.path());

    // Now make a new commit on feature-b
    repo.set_head("refs/heads/feature-b").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
    let b_tip = repo.head().unwrap().peel_to_commit().unwrap();
    make_commit(
        &repo,
        "refs/heads/feature-b",
        "b2.txt",
        "b2",
        "feat: b extension",
        &[&b_tip],
    );

    // Go back to feature-a
    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Now run kin push. It should push feature-b even though we are on feature-a
    let output = kin_cmd()
        .arg("push")
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());

    // Check if feature-b was pushed to remote
    let remote_repo = Repository::open(remote_dir.path()).unwrap();
    let remote_b_tip = remote_repo
        .find_reference("refs/heads/feature-b")
        .unwrap()
        .target()
        .unwrap();
    let local_b_tip = repo
        .find_reference("refs/heads/feature-b")
        .unwrap()
        .target()
        .unwrap();

    assert_eq!(
        remote_b_tip, local_b_tip,
        "feature-b was not pushed to remote"
    );
}

#[test]
fn test_push_on_main_pushes_main() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_commit_id = make_commit(
        &repo,
        "refs/heads/main",
        "main.txt",
        "initial",
        "initial commit",
        &[],
    );
    let main_commit = repo.find_commit(main_commit_id).unwrap();

    let remote_dir = tempdir().unwrap();
    run_ok("git", &["init", "--bare"], remote_dir.path());
    run_ok(
        "git",
        &[
            "remote",
            "add",
            "origin",
            remote_dir.path().to_str().unwrap(),
        ],
        dir.path(),
    );

    run_ok("git", &["push", "-u", "origin", "main"], dir.path());

    make_commit(
        &repo,
        "refs/heads/main",
        "main-2.txt",
        "next",
        "main follow-up",
        &[&main_commit],
    );

    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    let output = kin_cmd()
        .arg("push")
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());

    let remote_repo = Repository::open(remote_dir.path()).unwrap();
    let remote_main_tip = remote_repo
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap();
    let local_main_tip = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap();

    assert_eq!(
        remote_main_tip, local_main_tip,
        "main was not pushed to remote"
    );
}

#[test]
fn test_push_on_main_uses_tracked_remote() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_commit_id = make_commit(
        &repo,
        "refs/heads/main",
        "main.txt",
        "initial",
        "initial commit",
        &[],
    );
    let main_commit = repo.find_commit(main_commit_id).unwrap();

    let origin_dir = tempdir().unwrap();
    let upstream_dir = tempdir().unwrap();
    run_ok("git", &["init", "--bare"], origin_dir.path());
    run_ok("git", &["init", "--bare"], upstream_dir.path());
    run_ok(
        "git",
        &[
            "remote",
            "add",
            "origin",
            origin_dir.path().to_str().unwrap(),
        ],
        dir.path(),
    );
    run_ok(
        "git",
        &[
            "remote",
            "add",
            "upstream",
            upstream_dir.path().to_str().unwrap(),
        ],
        dir.path(),
    );

    run_ok("git", &["push", "-u", "upstream", "main"], dir.path());
    run_ok("git", &["push", "origin", "main"], dir.path());

    make_commit(
        &repo,
        "refs/heads/main",
        "main-2.txt",
        "next",
        "main follow-up",
        &[&main_commit],
    );

    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    let output = kin_cmd()
        .arg("push")
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());

    let upstream_repo = Repository::open(upstream_dir.path()).unwrap();
    let upstream_main_tip = upstream_repo
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap();
    let origin_repo = Repository::open(origin_dir.path()).unwrap();
    let origin_main_tip = origin_repo
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap();
    let local_main_tip = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap();

    assert_eq!(upstream_main_tip, local_main_tip);
    assert_ne!(origin_main_tip, local_main_tip);
}

#[test]
fn test_push_on_main_uses_tracked_remote_without_origin() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_commit_id = make_commit(
        &repo,
        "refs/heads/main",
        "main.txt",
        "initial",
        "initial commit",
        &[],
    );
    let main_commit = repo.find_commit(main_commit_id).unwrap();

    let upstream_dir = tempdir().unwrap();
    run_ok("git", &["init", "--bare"], upstream_dir.path());
    run_ok(
        "git",
        &[
            "remote",
            "add",
            "upstream",
            upstream_dir.path().to_str().unwrap(),
        ],
        dir.path(),
    );

    run_ok("git", &["push", "-u", "upstream", "main"], dir.path());

    make_commit(
        &repo,
        "refs/heads/main",
        "main-2.txt",
        "next",
        "main follow-up",
        &[&main_commit],
    );

    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    let output = kin_cmd()
        .arg("push")
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());

    let upstream_repo = Repository::open(upstream_dir.path()).unwrap();
    let upstream_main_tip = upstream_repo
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap();
    let local_main_tip = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap();

    assert_eq!(upstream_main_tip, local_main_tip);
}

#[test]
fn test_push_tracked_stack_uses_tracked_remote_without_origin() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_commit_id = make_commit(
        &repo,
        "refs/heads/main",
        "main.txt",
        "initial",
        "initial commit",
        &[],
    );
    let main_commit = repo.find_commit(main_commit_id).unwrap();

    let a_commit_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feat: a",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_commit_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feat: b",
        &[&a_commit],
    );

    let extra_remote_dir = tempdir().unwrap();
    let upstream_dir = tempdir().unwrap();
    run_ok("git", &["init", "--bare"], extra_remote_dir.path());
    run_ok("git", &["init", "--bare"], upstream_dir.path());
    run_ok(
        "git",
        &[
            "remote",
            "add",
            "backup",
            extra_remote_dir.path().to_str().unwrap(),
        ],
        dir.path(),
    );
    run_ok(
        "git",
        &[
            "remote",
            "add",
            "upstream",
            upstream_dir.path().to_str().unwrap(),
        ],
        dir.path(),
    );

    run_ok("git", &["push", "-u", "upstream", "main"], dir.path());
    run_ok("git", &["push", "-u", "upstream", "feature-a"], dir.path());
    run_ok("git", &["push", "-u", "upstream", "feature-b"], dir.path());

    repo.set_head("refs/heads/feature-b").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
    let b_tip = repo.head().unwrap().peel_to_commit().unwrap();
    make_commit(
        &repo,
        "refs/heads/feature-b",
        "b2.txt",
        "b2",
        "feat: b extension",
        &[&b_tip],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    let output = kin_cmd()
        .arg("push")
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());

    let upstream_repo = Repository::open(upstream_dir.path()).unwrap();
    let upstream_b_tip = upstream_repo
        .find_reference("refs/heads/feature-b")
        .unwrap()
        .target()
        .unwrap();
    let local_b_tip = repo
        .find_reference("refs/heads/feature-b")
        .unwrap()
        .target()
        .unwrap();

    assert_eq!(upstream_b_tip, local_b_tip);
}

#[test]
fn test_push_empty_stack_does_not_resolve_remote() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    make_commit(
        &repo,
        "refs/heads/main",
        "main.txt",
        "initial",
        "initial commit",
        &[],
    );

    let backup_dir = tempdir().unwrap();
    let upstream_dir = tempdir().unwrap();
    run_ok("git", &["init", "--bare"], backup_dir.path());
    run_ok("git", &["init", "--bare"], upstream_dir.path());
    run_ok(
        "git",
        &[
            "remote",
            "add",
            "backup",
            backup_dir.path().to_str().unwrap(),
        ],
        dir.path(),
    );
    run_ok(
        "git",
        &[
            "remote",
            "add",
            "upstream",
            upstream_dir.path().to_str().unwrap(),
        ],
        dir.path(),
    );

    run_ok("git", &["checkout", "--detach", "main"], dir.path());

    let output = kin_cmd()
        .arg("push")
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "kin push should succeed on an empty stack even without a resolvable default remote: {:?}",
        output
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No branches in stack to push."));
}
