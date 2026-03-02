//! Integration tests for gits push ensuring it pushes the whole stack.

mod common;
use common::{gits_cmd, make_commit, run_ok};
use git2::Repository;
use tempfile::tempdir;

#[test]
fn test_push_entire_stack() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

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

    // Run gits push
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

    // Now run gits push. It should push feature-b even though we are on feature-a
    let output = gits_cmd()
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
