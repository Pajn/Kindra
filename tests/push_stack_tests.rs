//! Integration tests for kin push ensuring it pushes the whole stack.

mod common;
use common::{
    advance_remote_main, kin_cmd, make_commit, remote_tip, repo_init, run_ok,
    setup_trunk_tracking_branch, write_repo_config,
};
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

#[test]
fn test_push_reports_divergence_on_lease_failure() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(
        &repo,
        "refs/heads/main",
        "main.txt",
        "initial",
        "initial commit",
        &[],
    );
    let main_commit = repo.find_commit(main_id).unwrap();
    make_commit(
        &repo,
        "refs/heads/feature",
        "f.txt",
        "f",
        "feat: f",
        &[&main_commit],
    );

    // Bare remote with main + feature pushed and tracked.
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
    run_ok("git", &["push", "-u", "origin", "feature"], dir.path());

    // A teammate advances origin/feature via a separate clone. The original repo
    // never fetches, so its origin/feature remote-tracking ref goes stale.
    let other_dir = tempdir().unwrap();
    run_ok(
        "git",
        &[
            "clone",
            remote_dir.path().to_str().unwrap(),
            other_dir.path().to_str().unwrap(),
        ],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature"], other_dir.path());
    std::fs::write(other_dir.path().join("teammate.txt"), "teammate").unwrap();
    run_ok("git", &["add", "teammate.txt"], other_dir.path());
    run_ok(
        "git",
        &["commit", "-m", "teammate change"],
        other_dir.path(),
    );
    run_ok("git", &["push", "origin", "feature"], other_dir.path());

    // Locally advance feature too so it diverges from the (stale) tracking ref.
    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
    let f_tip = repo.head().unwrap().peel_to_commit().unwrap();
    make_commit(
        &repo,
        "refs/heads/feature",
        "local2.txt",
        "local2",
        "local change",
        &[&f_tip],
    );

    // The push must be rejected (force-with-lease --force-if-includes) and report
    // the divergence with actionable guidance instead of a bare failure line.
    let output = kin_cmd()
        .arg("push")
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "push should be rejected when the remote diverged: {:?}",
        output
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("was rejected"),
        "expected a rejection explanation, got stderr:\n{}",
        stderr
    );
    assert!(
        stderr.contains("git fetch"),
        "expected recovery guidance mentioning git fetch, got stderr:\n{}",
        stderr
    );
    assert!(
        stderr.contains("feature"),
        "expected the diverged branch to be named, got stderr:\n{}",
        stderr
    );
}

/// Regression: `kin push` (and therefore `kin pr`) must never turn a stack
/// branch that tracks the trunk into a `branch:main` force-push. This rewrote
/// `main` on a real repository and dropped three merged commits.
#[test]
fn push_refuses_to_push_stack_branch_onto_trunk() {
    let (dir, remote_dir, _repo) = setup_trunk_tracking_branch("ci/checks-frontend-runners");
    advance_remote_main(dir.path(), remote_dir.path(), 3);

    let main_before = remote_tip(remote_dir.path(), "refs/heads/main");

    let output = kin_cmd()
        .arg("push")
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        !output.status.success(),
        "kin push must refuse a branch tracking the trunk.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        stderr,
    );

    assert_eq!(
        remote_tip(remote_dir.path(), "refs/heads/main"),
        main_before,
        "remote main was rewritten by pushing a stack branch onto it",
    );

    assert!(
        stderr.contains("Refusing to push")
            && stderr.contains("ci/checks-frontend-runners")
            && stderr.contains("main"),
        "error should be the guard's, naming the branch and the trunk it would have \
         overwritten, got:\n{stderr}",
    );

    // Kindra must refuse on its own, before handing the refspec to git. git's
    // `--force-with-lease --force-if-includes` rejected this particular shape,
    // but it is a lease on "has the remote moved since I fetched?", not a guard
    // against pushing to the wrong ref — the incident proves shapes exist where
    // it lets the push through. So the guard may not depend on it firing.
    assert!(
        !stderr.contains("[rejected]") && !stderr.contains("failed to push some refs"),
        "the push must be refused before git runs, not rejected by git's lease:\n{stderr}",
    );
}

/// The commits that landed on the trunk after the branch was created must still
/// be reachable from remote `main` after the refused push.
#[test]
fn push_preserves_trunk_commits_added_after_branch_creation() {
    let (dir, remote_dir, _repo) = setup_trunk_tracking_branch("feature-x");
    advance_remote_main(dir.path(), remote_dir.path(), 3);

    let remote_repo = Repository::open(remote_dir.path()).unwrap();
    let main_tip = remote_repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let expected: Vec<String> = {
        let mut walk = remote_repo.revwalk().unwrap();
        walk.push(main_tip.id()).unwrap();
        walk.map(|id| id.unwrap().to_string()).collect()
    };

    let output = kin_cmd()
        .arg("push")
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // Assert the refusal is Kindra's, not git's lease rejecting the push: without
    // this the test passes against unfixed code, because in this fixture the lease
    // happens to reject too.
    assert!(!output.status.success(), "kin push must refuse:\n{stderr}");
    assert!(
        !stderr.contains("[rejected]"),
        "must be refused before git runs:\n{stderr}",
    );

    let remote_repo = Repository::open(remote_dir.path()).unwrap();
    let mut walk = remote_repo.revwalk().unwrap();
    walk.push(
        remote_repo
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap(),
    )
    .unwrap();
    let actual: Vec<String> = walk.map(|id| id.unwrap().to_string()).collect();

    assert_eq!(
        expected, actual,
        "commits on remote main must survive a push of a trunk-tracking stack branch",
    );
}

/// The guard must not overreach: a branch tracking its own same-named remote
/// branch still pushes normally.
#[test]
fn push_still_pushes_branch_tracking_its_own_remote_branch() {
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
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feat: a",
        &[&main_commit],
    );
    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
    run_ok("git", &["push", "-u", "origin", "feature-a"], dir.path());

    let tip = repo.head().unwrap().peel_to_commit().unwrap();
    make_commit(
        &repo,
        "refs/heads/feature-a",
        "a2.txt",
        "a2",
        "feat: a follow-up",
        &[&tip],
    );

    let output = kin_cmd()
        .arg("push")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "normal same-name push must still work.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    assert_eq!(
        remote_tip(remote_dir.path(), "refs/heads/feature-a"),
        repo.find_reference("refs/heads/feature-a")
            .unwrap()
            .target()
            .unwrap(),
        "feature-a was not pushed",
    );
    assert_eq!(
        remote_tip(remote_dir.path(), "refs/heads/main"),
        repo.find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap(),
        "main must be untouched",
    );
}

/// Interactive escape hatch: selecting the mis-tracked branch in the
/// set-upstream prompt repoints it at its own remote branch and pushes that,
/// leaving the trunk alone.
#[test]
fn push_can_repoint_trunk_tracking_branch_to_its_own_remote_branch() {
    let (dir, remote_dir, repo) = setup_trunk_tracking_branch("feature-y");
    advance_remote_main(dir.path(), remote_dir.path(), 1);

    let main_before = remote_tip(remote_dir.path(), "refs/heads/main");

    let output = kin_cmd()
        .arg("push")
        .env("KIN_TEST_MULTI_SELECTIONS", "0")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "selecting the branch should fix it and push.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    assert_eq!(
        remote_tip(remote_dir.path(), "refs/heads/main"),
        main_before,
        "main must be untouched",
    );
    assert_eq!(
        remote_tip(remote_dir.path(), "refs/heads/feature-y"),
        repo.find_reference("refs/heads/feature-y")
            .unwrap()
            .target()
            .unwrap(),
        "feature-y should have been pushed to its own remote branch",
    );

    let upstream = std::process::Command::new("git")
        .args([
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "feature-y@{upstream}",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();
    // A non-zero status here means feature-y has no upstream at all, which is a
    // real failure of the repoint — say so rather than comparing empty output.
    assert!(
        upstream.status.success(),
        "feature-y has no upstream; it should have been repointed at origin/feature-y.\nstderr:\n{}",
        String::from_utf8_lossy(&upstream.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&upstream.stdout).trim(),
        "origin/feature-y",
        "upstream should be repointed away from the trunk",
    );
}

/// Bypass regression: `find_upstream` resolves only the *first* base branch that
/// exists, so in a repo with both `main` and `master` only one would be protected.
/// A branch tracking the other one is the same incident under a different name and
/// must be refused too — git's lease is not the backstop here.
#[test]
fn push_refuses_branch_tracking_a_sibling_base_branch() {
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
    // Both long-lived branches exist; `find_upstream` resolves `main`.
    run_ok("git", &["branch", "master"], dir.path());
    run_ok("git", &["push", "-u", "origin", "main"], dir.path());
    run_ok("git", &["push", "-u", "origin", "master"], dir.path());

    run_ok(
        "git",
        &[
            "-c",
            "branch.autoSetupMerge=true",
            "checkout",
            "-b",
            "feature",
            "origin/master",
        ],
        dir.path(),
    );
    let base = repo.head().unwrap().peel_to_commit().unwrap();
    make_commit(
        &repo,
        "refs/heads/feature",
        "work.txt",
        "work",
        "feat: work",
        &[&base],
    );
    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    let master_before = remote_tip(remote_dir.path(), "refs/heads/master");

    let output = kin_cmd()
        .arg("push")
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        !output.status.success(),
        "pushing feature onto master must be refused.\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        !stderr.contains("[rejected]"),
        "must be refused by Kindra, not git's lease:\n{stderr}",
    );
    // Pin the failure to the guard: any unrelated error also exits non-zero.
    assert!(
        stderr.contains("Refusing to push") && stderr.contains("master"),
        "expected the base-branch refusal naming master, got:\n{stderr}",
    );
    assert_eq!(
        remote_tip(remote_dir.path(), "refs/heads/master"),
        master_before,
        "remote master was rewritten",
    );
}

/// Bypass regression: the base is reduced through `base_short_name`, which strips a
/// leading `<segment>/` when that segment names a configured remote. With a base
/// branch `release/2024` and a remote named `release`, the reduced name (`2024`)
/// no longer matches the refspec destination (`release/2024`), which silently
/// disarmed the guard entirely.
#[test]
fn push_refuses_slashed_base_branch_shadowed_by_a_remote_name() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    make_commit(
        &repo,
        "refs/heads/release/2024",
        "base.txt",
        "initial",
        "initial commit",
        &[],
    );
    repo.set_head("refs/heads/release/2024").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

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
    // A remote whose name collides with the base branch's first path segment.
    run_ok(
        "git",
        &[
            "remote",
            "add",
            "release",
            remote_dir.path().to_str().unwrap(),
        ],
        dir.path(),
    );
    run_ok("git", &["push", "-u", "origin", "release/2024"], dir.path());
    write_repo_config(dir.path(), "upstream_branch = \"release/2024\"\n");

    run_ok(
        "git",
        &[
            "-c",
            "branch.autoSetupMerge=true",
            "checkout",
            "-b",
            "feature",
            "origin/release/2024",
        ],
        dir.path(),
    );
    let base = repo.head().unwrap().peel_to_commit().unwrap();
    make_commit(
        &repo,
        "refs/heads/feature",
        "work.txt",
        "work",
        "feat: work",
        &[&base],
    );
    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    let base_before = remote_tip(remote_dir.path(), "refs/heads/release/2024");

    let output = kin_cmd()
        .arg("push")
        .current_dir(dir.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        !output.status.success(),
        "pushing feature onto release/2024 must be refused.\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        stderr.contains("Refusing to push") && stderr.contains("release/2024"),
        "expected the base-branch refusal naming release/2024, got:\n{stderr}",
    );
    assert_eq!(
        remote_tip(remote_dir.path(), "refs/heads/release/2024"),
        base_before,
        "remote release/2024 was rewritten",
    );
}

/// `--allow-base-push <branch>` lets a deliberately mis-tracked branch through,
/// and the output labels it so an override is never silent.
#[test]
fn allow_base_push_flag_permits_the_named_branch() {
    let (dir, remote_dir, repo) = setup_trunk_tracking_branch("mirror");

    let output = kin_cmd()
        .args(["push", "--allow-base-push", "mirror"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    assert!(
        output.status.success(),
        "the named branch should be allowed.\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        stdout.contains("(override: allow-base-push)"),
        "an overridden push must be labelled in the output:\n{stdout}",
    );
    assert_eq!(
        remote_tip(remote_dir.path(), "refs/heads/main"),
        repo.find_reference("refs/heads/mirror")
            .unwrap()
            .target()
            .unwrap(),
        "the deliberate base push should have landed",
    );
}

/// The flag is per-branch, not a blanket switch: naming one branch must not
/// unblock another mis-tracked branch in the same stack.
#[test]
fn allow_base_push_does_not_unblock_other_branches() {
    let (dir, remote_dir, repo) = setup_trunk_tracking_branch("mirror");

    // A second, unrelated branch that also tracks the base.
    let tip = repo
        .find_reference("refs/heads/mirror")
        .unwrap()
        .target()
        .unwrap();
    let tip = repo.find_commit(tip).unwrap();
    make_commit(
        &repo,
        "refs/heads/oops",
        "oops.txt",
        "oops",
        "accidental branch",
        &[&tip],
    );
    run_ok(
        "git",
        &["branch", "--set-upstream-to=origin/main", "oops"],
        dir.path(),
    );
    repo.set_head("refs/heads/oops").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    let main_before = remote_tip(remote_dir.path(), "refs/heads/main");

    let output = kin_cmd()
        .args(["push", "--allow-base-push", "mirror"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        !output.status.success(),
        "'oops' was not named and must still be refused.\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        stderr.contains("Refusing to push") && stderr.contains("oops"),
        "the guard should refuse, naming the branch that was not allowed:\n{stderr}",
    );
    assert_eq!(
        remote_tip(remote_dir.path(), "refs/heads/main"),
        main_before,
        "nothing should have been pushed",
    );
}

/// A typo in the flag leaves the branch refused rather than opening the guard.
#[test]
fn allow_base_push_typo_fails_closed() {
    let (dir, remote_dir, _repo) = setup_trunk_tracking_branch("mirror");
    let main_before = remote_tip(remote_dir.path(), "refs/heads/main");

    let output = kin_cmd()
        .args(["push", "--allow-base-push", "mirrr"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        !output.status.success(),
        "a typo must not allow the push.\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
    // Without this the test would also pass if the *flag name* were mistyped and
    // clap rejected the arguments, never reaching the guard at all.
    assert!(
        stderr.contains("Refusing to push") && stderr.contains("mirror"),
        "expected the base-branch refusal naming mirror, got:\n{stderr}",
    );
    assert_eq!(
        remote_tip(remote_dir.path(), "refs/heads/main"),
        main_before,
        "remote main must be untouched",
    );
}

/// The per-branch config opt-in works without the flag, for a long-lived mirror.
#[test]
fn allow_base_push_config_permits_the_branch() {
    let (dir, remote_dir, repo) = setup_trunk_tracking_branch("mirror");
    run_ok(
        "git",
        &["config", "branch.mirror.kinAllowBasePush", "true"],
        dir.path(),
    );

    let output = kin_cmd()
        .arg("push")
        .current_dir(dir.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    assert!(
        output.status.success(),
        "the configured branch should be allowed.\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    // The config path must label the override too, not just the flag path: "an
    // override is never silent" has to hold however the branch was authorised.
    assert!(
        stdout.contains("(override: allow-base-push)"),
        "a config-authorised base push must also be labelled in the output:\n{stdout}",
    );
    assert_eq!(
        remote_tip(remote_dir.path(), "refs/heads/main"),
        repo.find_reference("refs/heads/mirror")
            .unwrap()
            .target()
            .unwrap(),
    );
}

/// The global `--yes` must not imply the override: a non-interactive run that
/// auto-answers prompts still refuses to rewrite a base branch.
#[test]
fn yes_flag_does_not_imply_allow_base_push() {
    let (dir, remote_dir, _repo) = setup_trunk_tracking_branch("ci/checks");
    let main_before = remote_tip(remote_dir.path(), "refs/heads/main");

    let output = kin_cmd()
        .args(["push", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        !output.status.success(),
        "--yes must not open the guard.\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
    // Pin the failure to the guard, so the test cannot pass on an unrelated error.
    assert!(
        stderr.contains("Refusing to push") && stderr.contains("ci/checks"),
        "expected the base-branch refusal naming the branch, got:\n{stderr}",
    );
    assert_eq!(
        remote_tip(remote_dir.path(), "refs/heads/main"),
        main_before,
    );
}
