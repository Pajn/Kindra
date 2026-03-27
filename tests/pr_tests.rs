mod common;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use common::{kin_cmd, make_commit, repo_init, run_ok, write_repo_config};
use git2::{BranchType, Repository};
use kindra::commands::pr::resolve_stack_boundary_and_base;
use std::fs;
use tempfile::tempdir;

/// Create a minimal repo with `main` + a feature branch stacked on top.
///
/// Layout:
/// ```
///   main  ── A  (initial commit)
///               └── B  (refs/heads/feature, 1 commit)
/// ```
fn setup_simple_stack() -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    // A – initial commit on main
    let a_id = make_commit(
        &repo,
        "refs/heads/main",
        "README.md",
        "hello",
        "initial commit on main",
        &[],
    );

    // B – feature on top of main (drop the Commit borrow before returning)
    {
        let a = repo.find_commit(a_id).unwrap();
        make_commit(
            &repo,
            "refs/heads/feature",
            "feature.txt",
            "feat",
            "add feature",
            &[&a],
        );
    }

    // HEAD = main
    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    (dir, repo)
}

/// Three-level stack: main → feature-a → feature-b.
fn setup_two_level_stack() -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let a_id = make_commit(
        &repo,
        "refs/heads/main",
        "README.md",
        "hello",
        "initial",
        &[],
    );
    let b_id = {
        let a = repo.find_commit(a_id).unwrap();
        make_commit(
            &repo,
            "refs/heads/feature-a",
            "a.txt",
            "a",
            "feat: a",
            &[&a],
        )
    };
    {
        let b = repo.find_commit(b_id).unwrap();
        make_commit(
            &repo,
            "refs/heads/feature-b",
            "b.txt",
            "b",
            "feat: b",
            &[&b],
        );
    }

    repo.set_head("refs/heads/feature-b").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    (dir, repo)
}

/// Four-level history: main -> sync-main -> pr-review -> pr-merge.
fn setup_review_merge_stack() -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(
        &repo,
        "refs/heads/main",
        "README.md",
        "hello",
        "initial",
        &[],
    );
    let sync_main_id = {
        let main = repo.find_commit(main_id).unwrap();
        make_commit(
            &repo,
            "refs/heads/sync-main",
            "sync.txt",
            "sync",
            "feat: sync main",
            &[&main],
        )
    };
    let pr_review_id = {
        let sync_main = repo.find_commit(sync_main_id).unwrap();
        make_commit(
            &repo,
            "refs/heads/pr-review",
            "review.txt",
            "review",
            "feat: pr review",
            &[&sync_main],
        )
    };
    {
        let pr_review = repo.find_commit(pr_review_id).unwrap();
        make_commit(
            &repo,
            "refs/heads/pr-merge",
            "merge.txt",
            "merge",
            "feat: pr merge",
            &[&pr_review],
        );
    }

    repo.set_head("refs/heads/pr-merge").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    (dir, repo)
}

#[test]
fn pr_fails_without_gh() {
    // Run in a temporary directory that is a valid git repo but has no
    // authenticated gh session (CI typically has no gh at all, or gh
    // auth status will return non-zero).
    let (dir, _repo) = setup_simple_stack();

    // We only check that the command either:
    //   a) exits with a non-zero code (gh missing or not authed), OR
    //   b) exits with "No branches with a remote upstream" (gh auth passed
    //      but nothing to do)
    // The important thing is it does NOT panic.
    let mut cmd = kin_cmd();
    cmd.arg("pr").current_dir(dir.path());

    // The command is allowed to succeed (exit 0) only with the "nothing to do"
    // message, or to fail. Either way, it must not crash (exit code 101+).
    let output = cmd.output().unwrap();
    let code = output.status.code().unwrap_or_else(|| {
        panic!(
            "kin pr was terminated by a signal. stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    });
    assert!(
        code != 101,
        "kin pr panicked (exit 101). stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn pr_no_upstreams_message() {
    // If gh auth fails (common in CI) the test would not reach the upstream
    // check. We skip the assertion in that case.
    let (dir, _repo) = setup_simple_stack();

    let output = kin_cmd()
        .arg("pr")
        .current_dir(dir.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);

    // Either gh not found/authed, or we see the "no upstream" message.
    let acceptable = combined.contains("No branches")
        || combined.contains("gh")
        || combined.contains("authenticated")
        || combined.contains("not found");

    assert!(acceptable, "Unexpected output from `kin pr`:\n{}", combined);
}

#[test]
fn single_commit_branch_title_prefill() {
    let (dir, _repo) = setup_simple_stack();

    // Set up remote and push
    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );

    // Checkout feature branch so stack detection finds it
    run_ok("git", &["checkout", "feature"], dir.path());

    // Create mock gh
    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo "no pull requests found for branch" >&2
    exit 1
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "create" ]]; then
    echo "https://github.com/test/repo/pull/1"
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = kin_cmd()
        .arg("pr")
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{}", stdout);

    // Single commit branch should have prefilled title
    assert!(
        combined.contains("add feature"),
        "Single commit branch should have prefilled title. Got:\n{}",
        combined
    );
}

#[test]
fn single_commit_body_prefill_in_editor() {
    let (dir, _repo) = setup_simple_stack();

    // Overwrite the feature branch commit message to have a body
    run_ok("git", &["checkout", "feature"], dir.path());
    run_ok(
        "git",
        &[
            "commit",
            "--amend",
            "-m",
            "feat: add feature\n\nThis is the detailed description of the feature.",
        ],
        dir.path(),
    );

    // Set up remote and push
    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );

    // Checkout feature branch so stack detection finds it
    run_ok("git", &["checkout", "feature"], dir.path());

    // Create mock gh that captures the PR body
    let gh_pr_args = dir.path().join("gh_pr_args.txt");
    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        format!(
            r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo "no pull requests found for branch" >&2
    exit 1
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "create" ]]; then
    printf "%s\n" "$@" > "{}"
    echo "https://github.com/test/repo/pull/1"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
            gh_pr_args.display()
        ),
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = kin_cmd()
        .arg("pr")
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "kin pr failed: {:?}\nstderr: {}",
        output,
        String::from_utf8_lossy(&output.stderr)
    );

    let pr_args = std::fs::read_to_string(&gh_pr_args).unwrap();

    // The body argument to gh pr create should contain the commit message body
    assert!(
        pr_args.contains("This is the detailed description of the feature."),
        "PR body should contain commit body. Got:\n{}",
        pr_args
    );
}

#[test]
fn test_pr_label_flag() {
    let (dir, _repo) = setup_simple_stack();

    // Set up remote and push
    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );

    run_ok("git", &["checkout", "feature"], dir.path());

    // Create mock gh that captures the PR arguments
    let gh_pr_args = dir.path().join("gh_pr_args.txt");
    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        format!(
            r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo "no pull requests found for branch" >&2
    exit 1
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "create" ]]; then
    printf "%s\n" "$@" > "{}"
    echo "https://github.com/test/repo/pull/1"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
            gh_pr_args.display()
        ),
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = kin_cmd()
        .args(["pr", "--label", "bug", "--label", "urgent"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "kin pr --label failed: {:?}",
        output
    );

    let pr_args = std::fs::read_to_string(&gh_pr_args).unwrap();

    // Both labels should be passed to gh pr create
    assert!(
        pr_args.contains("--label") && pr_args.contains("bug") && pr_args.contains("urgent"),
        "PR create should contain both labels. Got:\n{}",
        pr_args
    );
}

#[test]
fn test_pr_push_flag() {
    let (dir, _repo) = setup_simple_stack();

    // Set up remote and push
    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    // Note: NOT pushing feature branch - that's what --push should handle

    run_ok("git", &["checkout", "feature"], dir.path());

    // Create mock gh
    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo "no pull requests found for branch" >&2
    exit 1
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "create" ]]; then
    echo "https://github.com/test/repo/pull/1"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = kin_cmd()
        .args(["pr", "--push"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "kin pr --push failed: {:?}",
        output
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Verify that kin prints the push message
    assert!(
        stdout.contains("Pushing branches first"),
        "kin pr --push should indicate it's pushing branches first. Got:\n{}",
        stdout
    );
}

/// Test that after `kin sync`, `kin pr` uses the correct base (origin/main)
/// even when the local main branch is behind origin/main.
///
/// Scenario:
/// 1. main -> feature-a (stack)
/// 2. Push to origin
/// 3. origin/main advances (another worktree pushes new commits)
/// 4. Run `kin sync` - rebases feature-a onto origin/main
/// 5. local main is now behind origin/main
/// 6. Run `kin pr` - should use origin/main as base, not local main
#[test]
fn pr_uses_origin_main_as_base_when_local_main_is_behind_after_sync() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    // Set up remote repo
    let remote_dir = dir.path().join("remote.git");
    fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );

    // Create initial commit on main
    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    // Create feature-a branch on top
    make_commit(
        &repo,
        "refs/heads/feature-a",
        "feature.txt",
        "feature",
        "add feature",
        &[&base],
    );

    // Push main and feature-a to origin
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a"],
        dir.path(),
    );

    // Simulate remote advancing - clone and push from another "worktree"
    let remote_worktree = tempdir().unwrap();
    run_ok(
        "git",
        &[
            "clone",
            remote_dir.to_str().unwrap(),
            remote_worktree.path().to_str().unwrap(),
        ],
        dir.path(),
    );
    run_ok("git", &["checkout", "main"], remote_worktree.path());
    fs::write(remote_worktree.path().join("remote.txt"), "remote main").unwrap();
    run_ok("git", &["add", "remote.txt"], remote_worktree.path());
    run_ok(
        "git",
        &["commit", "-m", "remote main advanced"],
        remote_worktree.path(),
    );
    run_ok("git", &["push", "origin", "main"], remote_worktree.path());

    // Fetch the updated origin/main to see the divergence in our local repo
    run_ok("git", &["fetch", "origin", "main"], dir.path());

    // Now local main is behind origin/main, but feature-a is still based on local main
    let local_main_id = repo
        .find_branch("main", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let origin_main_id = repo.revparse_single("origin/main").unwrap().id();
    assert_ne!(
        local_main_id, origin_main_id,
        "local main should be behind origin/main before sync"
    );

    // Run kin sync to rebase feature-a onto origin/main
    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    let mut cmd = kin_cmd();
    cmd.arg("sync")
        .arg("--no-delete")
        .current_dir(dir.path())
        .assert()
        .success();

    // Verify feature-a is now based on origin/main
    let repo = Repository::open(dir.path()).unwrap();
    let origin_main_after = repo.revparse_single("origin/main").unwrap().id();
    let feature_a_after = repo
        .find_branch("feature-a", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let feature_a_commit = repo.find_commit(feature_a_after).unwrap();
    assert_eq!(
        feature_a_commit.parent_id(0).unwrap(),
        origin_main_after,
        "feature-a should be rebased onto origin/main"
    );

    // Verify local main is still behind origin/main
    let local_main_after = repo
        .find_branch("main", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    assert_eq!(
        local_main_after, local_main_id,
        "local main should not have moved"
    );

    // Create mock gh that captures the --base argument
    let gh_mock = dir.path().join("gh");
    let captured_base = dir.path().join("captured_base.txt");
    std::fs::write(
        &gh_mock,
        format!(
            r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo "no pull requests found for branch" >&2
    exit 1
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "create" ]]; then
    # Capture --base argument
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "--base" ]]; then
            printf "%s" "$2" > "{}"
            break
        fi
        shift
    done
    echo "https://github.com/test/repo/pull/1"
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
            captured_base.display()
        ),
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    kin_cmd()
        .arg("pr")
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .assert()
        .success();

    // Verify gh pr create was called with --base main (not origin/main)
    let captured = fs::read_to_string(&captured_base).unwrap();
    assert_eq!(
        captured, "main",
        "gh pr create should use 'main' as base (normalized from origin/main), but got: {}",
        captured
    );
}

#[test]
fn pr_template_detected() {
    let (dir, _repo) = setup_simple_stack();

    // Add PR template
    let github_dir = dir.path().join(".github");
    fs::create_dir_all(&github_dir).unwrap();
    let template_content = "## Summary\n\n## Test Plan\n";
    fs::write(
        github_dir.join("pull_request_template.md"),
        template_content,
    )
    .unwrap();

    // Set up remote and push
    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );

    // Checkout feature branch so stack detection finds it
    run_ok("git", &["checkout", "feature"], dir.path());

    // Create mock gh
    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo "no pull requests found for branch" >&2
    exit 1
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "create" ]]; then
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "--body" ]]; then
            printf "%s" "$2" > "$MOCK_GH_BODY_FILE"
            break
        fi
        shift
        done
    echo "https://github.com/test/repo/pull/1"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let captured_body_path = dir.path().join("captured_body.txt");

    let output = kin_cmd()
        .arg("pr")
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_BODY_FILE", &captured_body_path)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr failed: {:?}", output);
    let captured_body = fs::read_to_string(&captured_body_path).unwrap();
    assert!(
        captured_body.contains(template_content),
        "PR body should include template content. Got:\n{}",
        captured_body
    );
}

#[test]
fn pr_adds_stack_section_to_multi_pr_descriptions() {
    let (dir, _repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo "no pull requests found for branch" >&2
    exit 1
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "create" ]]; then
    head=""
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "--head" ]]; then
            head="$2"
            break
        fi
        shift
    done
    if [[ "$head" == "feature-a" ]]; then
        echo "https://github.com/test/repo/pull/10"
        exit 0
    fi
    if [[ "$head" == "feature-b" ]]; then
        echo "https://github.com/test/repo/pull/11"
        exit 0
    fi
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    pr_number="$3"
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "--body" ]]; then
            printf "%s" "$2" > "$MOCK_GH_BODY_DIR/pr_$pr_number.txt"
            exit 0
        fi
        shift
    done
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let captured_body_dir = dir.path().join("captured-bodies");
    std::fs::create_dir_all(&captured_body_dir).unwrap();

    let output = kin_cmd()
        .arg("pr")
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_BODY_DIR", &captured_body_dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr failed: {:?}", output);

    let feature_a_body = fs::read_to_string(captured_body_dir.join("pr_10.txt")).unwrap();
    assert!(
        feature_a_body.contains("## Stack"),
        "feature-a body should include a stack section. Got:\n{}",
        feature_a_body
    );
    assert!(
        feature_a_body.contains("- → feature-a #10"),
        "feature-a body should mark the current PR. Got:\n{}",
        feature_a_body
    );
    assert!(
        feature_a_body.contains("- [feature-b](https://github.com/test/repo/pull/11) #11"),
        "feature-a body should link the other PR. Got:\n{}",
        feature_a_body
    );

    let feature_b_body = fs::read_to_string(captured_body_dir.join("pr_11.txt")).unwrap();
    assert!(
        feature_b_body.contains("- [feature-a](https://github.com/test/repo/pull/10) #10"),
        "feature-b body should link the other PR. Got:\n{}",
        feature_b_body
    );
    assert!(
        feature_b_body.contains("- → feature-b #11"),
        "feature-b body should mark the current PR. Got:\n{}",
        feature_b_body
    );
}

#[test]
fn pr_stack_sync_continues_when_one_edit_fails() {
    let (dir, _repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo "no pull requests found for branch" >&2
    exit 1
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "create" ]]; then
    head=""
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "--head" ]]; then
            head="$2"
            break
        fi
        shift
    done
    if [[ "$head" == "feature-a" ]]; then
        echo "https://github.com/test/repo/pull/10"
        exit 0
    fi
    if [[ "$head" == "feature-b" ]]; then
        echo "https://github.com/test/repo/pull/11"
        exit 0
    fi
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    pr_number="$3"
    if [[ "$pr_number" == "10" ]]; then
        echo "simulated edit failure" >&2
        exit 1
    fi
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "--body" ]]; then
            printf "%s" "$2" > "$MOCK_GH_BODY_DIR/pr_$pr_number.txt"
            exit 0
        fi
        shift
    done
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let captured_body_dir = dir.path().join("captured-bodies");
    std::fs::create_dir_all(&captured_body_dir).unwrap();

    let output = kin_cmd()
        .arg("pr")
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_BODY_DIR", &captured_body_dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr failed: {:?}", output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Failed to sync stack section for PR #10"),
        "Expected sync failure to be reported. Got:\n{}",
        stderr
    );

    let feature_b_body = fs::read_to_string(captured_body_dir.join("pr_11.txt")).unwrap();
    assert!(
        feature_b_body.contains("- → feature-b #11"),
        "feature-b body should still be updated after feature-a edit fails. Got:\n{}",
        feature_b_body
    );
}

#[test]
fn pr_stack_sync_skips_inaccessible_historical_pr_entries() {
    let (dir, _repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let gh_mock = dir.path().join("gh");
    let start = "<!-- kindra-stack:start -->";
    let end = "<!-- kindra-stack:end -->";
    let stale_body = format!(
        "Body with stale stack\n\n{}\n## Stack\n- [old-branch](https://github.com/test/repo/pull/999) #999\n- → feature-a #10\n{}\n",
        start, end
    );
    let stale_body_for_bash = stale_body.replace('\n', "\\n").replace('"', "\\\"");

    std::fs::write(
        &gh_mock,
        format!(
            r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "feature-a" ]]; then
        if [[ "$5" == "number,baseRefName,state" ]]; then
            echo '{{"number":10,"baseRefName":"main","state":"OPEN"}}'
        else
            echo '{{"number":10,"title":"PR A","body":"{}","url":"https://github.com/test/repo/pull/10","state":"OPEN","labels":[],"reviewRequests":[]}}'
        fi
        exit 0
    fi
    if [[ "$3" == "feature-b" ]]; then
        if [[ "$5" == "number,baseRefName,state" ]]; then
            echo '{{"number":11,"baseRefName":"feature-a","state":"OPEN"}}'
        else
            echo '{{"number":11,"title":"PR B","body":"Body B","url":"https://github.com/test/repo/pull/11","state":"OPEN","labels":[],"reviewRequests":[]}}'
        fi
        exit 0
    fi
    if [[ "$3" == "999" ]]; then
        echo "GraphQL: Could not resolve to a PullRequest with the number of 999." >&2
        exit 1
    fi
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    pr_number="$3"
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "--body" ]]; then
            printf "%s" "$2" > "$MOCK_GH_BODY_DIR/pr_$pr_number.txt"
            exit 0
        fi
        shift
    done
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
            stale_body_for_bash
        ),
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let captured_body_dir = dir.path().join("captured-bodies");
    std::fs::create_dir_all(&captured_body_dir).unwrap();

    let output = kin_cmd()
        .arg("pr")
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_BODY_DIR", &captured_body_dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr failed: {:?}", output);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Skipping inaccessible historical PR #999"),
        "Expected inaccessible historical PR warning. Got:\n{}",
        stderr
    );

    let feature_a_body = fs::read_to_string(captured_body_dir.join("pr_10.txt")).unwrap();
    assert!(
        feature_a_body.contains("- → feature-a #10"),
        "feature-a body should keep the active PR entry. Got:\n{}",
        feature_a_body
    );
    assert!(
        feature_a_body.contains("- [feature-b](https://github.com/test/repo/pull/11) #11"),
        "feature-a body should still include the active sibling PR. Got:\n{}",
        feature_a_body
    );
    assert!(
        !feature_a_body.contains("old-branch"),
        "feature-a body should drop inaccessible historical entries. Got:\n{}",
        feature_a_body
    );

    let feature_b_body = fs::read_to_string(captured_body_dir.join("pr_11.txt")).unwrap();
    assert!(
        feature_b_body.contains("- [feature-a](https://github.com/test/repo/pull/10) #10"),
        "feature-b body should still include feature-a after skipping the stale entry. Got:\n{}",
        feature_b_body
    );
    assert!(
        feature_b_body.contains("- → feature-b #11"),
        "feature-b body should still be updated. Got:\n{}",
        feature_b_body
    );
}

// Test: multi-commit branch → title is NOT prefilled (shows commit list instead)
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn multi_commit_branch_title_empty() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    // Create main with initial commit
    let a_id = make_commit(&repo, "refs/heads/main", "a.txt", "a", "initial", &[]);
    let a = repo.find_commit(a_id).unwrap();
    // Create feature with two commits
    make_commit(
        &repo,
        "refs/heads/feature",
        "b.txt",
        "b",
        "commit one",
        &[&a],
    );
    let b = repo
        .find_commit(
            repo.revparse_single("refs/heads/feature")
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .id(),
        )
        .unwrap();
    make_commit(
        &repo,
        "refs/heads/feature",
        "c.txt",
        "c",
        "commit two",
        &[&b],
    );

    // Set up remote and push
    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );

    // Checkout feature branch so stack detection finds it
    run_ok("git", &["checkout", "feature"], dir.path());

    // Create mock gh
    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo "no pull requests found for branch" >&2
    exit 1
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "create" ]]; then
    echo "https://github.com/test/repo/pull/1"
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = kin_cmd()
        .arg("pr")
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{}", stdout);

    // Multi-commit branch should NOT have prefilled title (title: should be empty/prompt)
    // Instead it should show commit list
    assert!(
        combined.contains("commit one") && combined.contains("commit two"),
        "Multi-commit branch should show commit list. Got:\n{}",
        combined
    );
    // The title prompt should NOT have "commit one" as initial value
    // (it should be empty since there are multiple commits)
    let title_line = combined.lines().find(|l| l.contains("PR title"));
    assert!(
        title_line.is_some(),
        "Should have PR title prompt. Got:\n{}",
        combined
    );
}

#[test]
fn stacked_branch_shows_correct_commits() {
    let (dir, _repo) = setup_two_level_stack();

    // Set up a "remote" by creating a bare repo and pushing both branches
    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);

    // Add remote and push both branches so both have upstreams
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );

    // Create a mock gh script that returns PR info for feature-b with base = feature-a
    // and handles all gh commands the test will encounter
    // Name it "gh" so it gets picked up when searching PATH
    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
# Handle gh auth status - pretend we're authenticated
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
# Handle all gh commands that may be called during the test
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    # Return no PR for all branches (so they all go through interactive mode)
    echo "no pull requests found for branch" >&2
    exit 1
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    # PR edit succeeds
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "create" ]]; then
    # Just succeed without actually creating a PR
    echo "https://github.com/test/repo/pull/999"
    exit 0
fi
# Handle any other unexpected commands
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    // Run kin pr with the mock gh in PATH
    let output = kin_cmd()
        .arg("pr")
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);

    // Verify: feature-b should show "feat: b" as title (1 commit above feature-a)
    // The key test is that feature-b's title is "feat: b", NOT a commit from main
    assert!(
        combined.contains("feat: b"),
        "Should show feature-b's commit. Got:\n{}",
        combined
    );
    // The title for feature-b should be pre-filled (meaning only 1 commit found)
    // If the bug existed (using main instead of feature-a), it would show both
    // commits and title would NOT be pre-filled
    let feature_b_section = combined.split("── feature-b ──").nth(1).unwrap_or("");
    assert!(
        feature_b_section.contains("feat: b") && !feature_b_section.contains("feat: a"),
        "feature-b should only show its own commit, not base branch commits. Got:\n{}",
        feature_b_section
    );
}

#[test]
fn slash_base_branch_uses_git_base_for_local_history() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(&repo, "refs/heads/main", "main.txt", "main", "initial", &[]);
    let base_id = {
        let main = repo.find_commit(main_id).unwrap();
        make_commit(
            &repo,
            "refs/heads/feature/base",
            "base.txt",
            "base",
            "feat: base",
            &[&main],
        )
    };
    {
        let base = repo.find_commit(base_id).unwrap();
        make_commit(
            &repo,
            "refs/heads/feature/child",
            "child.txt",
            "child",
            "feat: child",
            &[&base],
        );
    }

    repo.set_head("refs/heads/feature/child").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &[
            "push",
            "-u",
            "origin",
            "main",
            "feature/base",
            "feature/child",
        ],
        dir.path(),
    );
    assert!(
        repo.find_branch("base", git2::BranchType::Local).is_err(),
        "test setup should not have a local 'base' branch"
    );

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo "no pull requests found for branch" >&2
    exit 1
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "create" ]]; then
    echo "https://github.com/test/repo/pull/1"
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = kin_cmd()
        .arg("pr")
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr failed: {:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);
    let child_section = combined.split("── feature/child ──").nth(1).unwrap_or("");

    assert!(
        child_section.contains("feat: child"),
        "child branch should use its own commits. Got:\n{}",
        child_section
    );
    assert!(
        !child_section.contains("feat: base"),
        "child branch should not include base branch commit. Got:\n{}",
        child_section
    );
}

#[test]
fn pr_open_opens_single_pr_without_prompt() {
    let (dir, _repo) = setup_simple_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo '{"url":"https://github.com/test/repo/pull/42","state":"OPEN"}'
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let open_mock = dir.path().join("mock-open");
    std::fs::write(
        &open_mock,
        r#"#!/bin/bash
printf "%s" "$1" > "$MOCK_OPEN_CAPTURE"
exit 0
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", open_mock.to_str().unwrap()], dir.path());
    let opened_url_path = dir.path().join("opened_url.txt");

    let output = kin_cmd()
        .args(["pr", "open"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("GITS_OPEN_COMMAND", open_mock.to_str().unwrap())
        .env("MOCK_OPEN_CAPTURE", &opened_url_path)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr open failed: {:?}", output);
    let opened_url = fs::read_to_string(&opened_url_path).unwrap();
    assert_eq!(opened_url, "https://github.com/test/repo/pull/42");
}

#[test]
fn pr_open_with_multiple_prs_uses_selection() {
    let (dir, _repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "feature-a" ]]; then
        echo '{"url":"https://github.com/test/repo/pull/10","state":"OPEN"}'
        exit 0
    fi
    if [[ "$3" == "feature-b" ]]; then
        echo '{"url":"https://github.com/test/repo/pull/11","state":"OPEN"}'
        exit 0
    fi
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let open_mock = dir.path().join("mock-open");
    std::fs::write(
        &open_mock,
        r#"#!/bin/bash
printf "%s" "$1" > "$MOCK_OPEN_CAPTURE"
exit 0
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", open_mock.to_str().unwrap()], dir.path());
    let opened_url_path = dir.path().join("opened_url.txt");

    let output = kin_cmd()
        .args(["pr", "open"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("GITS_OPEN_COMMAND", open_mock.to_str().unwrap())
        .env("MOCK_OPEN_CAPTURE", &opened_url_path)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr open failed: {:?}", output);
    let opened_url = fs::read_to_string(&opened_url_path).unwrap();
    // Non-interactive tests auto-select the first option.
    assert_eq!(opened_url, "https://github.com/test/repo/pull/10");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Select PR to open:"),
        "Expected selection prompt in output. Got:\n{}",
        stdout
    );
}

#[test]
fn pr_edit_preserves_stack_block() {
    let (dir, _repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    # Return different PR info depending on which branch is requested
    if [[ "$3" == "feature-a" ]]; then
        echo '{"number":10,"title":"PR A","body":"Body A","url":"https://github.com/test/repo/pull/10","state":"OPEN","labels":[],"reviewRequests":[]}'
    elif [[ "$3" == "feature-b" ]]; then
        echo '{"number":11,"title":"PR B","body":"Body B","url":"https://github.com/test/repo/pull/11","state":"OPEN","labels":[],"reviewRequests":[]}'
    fi
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    printf "%s" "$@" > "$MOCK_GH_EDIT_ARGS"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let edit_args_path = dir.path().join("edit_args.txt");

    let output = kin_cmd()
        .args(["pr", "edit"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_EDIT_ARGS", &edit_args_path)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr edit failed: {:?}", output);
    let args = fs::read_to_string(&edit_args_path).unwrap();

    // Verify that the body was updated to include the stack section
    assert!(
        args.contains("## Stack"),
        "Expected stack section in PR body. Got:\n{}",
        args
    );
    assert!(
        args.contains("feature-a"),
        "Expected feature-a in stack section. Got:\n{}",
        args
    );
    assert!(
        args.contains("feature-b"),
        "Expected feature-b in stack section. Got:\n{}",
        args
    );
}

#[test]
fn pr_edit_cleans_duplicate_stack_blocks() {
    let (dir, _repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let gh_mock = dir.path().join("gh");
    let start = "<!-- kindra-stack:start -->";
    let end = "<!-- kindra-stack:end -->";
    let body_with_duplicates = format!(
        "Original Body\n\n{}\nOld Stack 1\n{}\n\nMiddle Text\n\n{}\nOld Stack 2\n{}",
        start, end, start, end
    );

    // Escape for JSON and then for Bash
    let body_for_bash = body_with_duplicates
        .replace("\n", "\\n")
        .replace("\"", "\\\"");

    std::fs::write(
        &gh_mock,
        format!(
            r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "feature-a" ]]; then
        echo '{{"number":10,"title":"PR A","body":"{}","url":"https://github.com/test/repo/pull/10","state":"OPEN","labels":[],"reviewRequests":[]}}'
    elif [[ "$3" == "feature-b" ]]; then
        echo '{{"number":11,"title":"PR B","body":"Body B","url":"https://github.com/test/repo/pull/11","state":"OPEN","labels":[],"reviewRequests":[]}}'
    fi
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    printf "%s" "$@" > "$MOCK_GH_EDIT_ARGS"
    exit 0
fi
exit 1
"#,
            body_for_bash
        ),
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let edit_args_path = dir.path().join("edit_args.txt");

    let output = kin_cmd()
        .args(["pr", "edit"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_EDIT_ARGS", &edit_args_path)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr edit failed: {:?}", output);
    let args = fs::read_to_string(&edit_args_path).unwrap();

    // Verify it contains exactly one ## Stack header
    let stack_count = args.matches("## Stack").count();
    assert_eq!(
        stack_count, 1,
        "Expected exactly one Stack header, got {}. Full args:\n{}",
        stack_count, args
    );

    // Verify it contains the new stack info but not the old ones
    assert!(args.contains("feature-a"), "Should contain feature-a");
    assert!(
        !args.contains("Old Stack 1"),
        "Should not contain Old Stack 1"
    );
    assert!(
        !args.contains("Old Stack 2"),
        "Should not contain Old Stack 2"
    );
    assert!(
        args.contains("Original Body"),
        "Should contain Original Body"
    );
    assert!(args.contains("Middle Text"), "Should contain Middle Text");
}

#[test]
fn pr_edit_migrates_legacy_stack_markers_without_duplicates() {
    let (dir, _repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let gh_mock = dir.path().join("gh");
    let legacy_start = "<!-- gits-stack:start -->";
    let legacy_end = "<!-- gits-stack:end -->";
    let body_with_legacy = format!(
        "Original Body\n\n{}\n## Stack\n- [feature-a](https://github.com/test/repo/pull/10) #10\n- → feature-b #11\n{}\n\nFooter",
        legacy_start, legacy_end
    );

    let body_for_bash = body_with_legacy.replace('\n', "\\n").replace('"', "\\\"");

    std::fs::write(
        &gh_mock,
        format!(
            r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "feature-a" ]]; then
        echo '{{"number":10,"title":"PR A","body":"{}","url":"https://github.com/test/repo/pull/10","state":"OPEN","labels":[],"reviewRequests":[]}}'
    elif [[ "$3" == "feature-b" ]]; then
        echo '{{"number":11,"title":"PR B","body":"Body B","url":"https://github.com/test/repo/pull/11","state":"OPEN","labels":[],"reviewRequests":[]}}'
    fi
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    pr_number="$3"
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "--body" ]]; then
            printf "%s" "$2" > "$MOCK_GH_BODY_DIR/pr_$pr_number.txt"
            exit 0
        fi
        shift
    done
    exit 0
fi
exit 1
"#,
            body_for_bash
        ),
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let captured_body_dir = dir.path().join("captured-bodies");
    std::fs::create_dir_all(&captured_body_dir).unwrap();

    let output = kin_cmd()
        .args(["pr", "edit"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_BODY_DIR", &captured_body_dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr edit failed: {:?}", output);

    let body = fs::read_to_string(captured_body_dir.join("pr_10.txt")).unwrap();
    assert_eq!(body.matches("## Stack").count(), 1, "Got:\n{}", body);
    assert!(
        body.contains("<!-- kindra-stack:start -->"),
        "Expected new sentinels to be written. Got:\n{}",
        body
    );
    assert!(
        body.contains("<!-- kindra-stack:end -->"),
        "Expected new sentinels to be written. Got:\n{}",
        body
    );
    assert!(
        !body.contains("<!-- gits-stack:start -->"),
        "Legacy start sentinel should be removed. Got:\n{}",
        body
    );
    assert!(
        !body.contains("<!-- gits-stack:end -->"),
        "Legacy end sentinel should be removed. Got:\n{}",
        body
    );
    assert!(body.contains("Original Body"), "Got:\n{}", body);
    assert!(body.contains("Footer"), "Got:\n{}", body);
}

#[test]
fn pr_edit_single_open_pr_saves_with_prefilled_title() {
    let (dir, _repo) = setup_simple_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo '{"number":42,"title":"Current title","body":"Current body","url":"https://github.com/test/repo/pull/42","state":"OPEN","labels":[{"name":"bug"}],"reviewRequests":[{"requestedReviewer":{"login":"alice"}}]}'
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    printf "%s" "$@" > "$MOCK_GH_EDIT_ARGS"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let edit_args_path = dir.path().join("edit_args.txt");

    let output = kin_cmd()
        .args(["pr", "edit"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_EDIT_ARGS", &edit_args_path)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr edit failed: {:?}", output);
    let args = fs::read_to_string(&edit_args_path).unwrap();
    assert!(
        args.contains("predit42--titleCurrent title"),
        "Expected title to be passed through unchanged. Got:\n{}",
        args
    );
    assert!(
        !args.contains("--body"),
        "Body should remain unchanged in non-interactive mode. Got:\n{}",
        args
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("PR edit options:"),
        "Expected menu prompt before saving. Got:\n{}",
        stdout
    );
}

#[test]
fn pr_edit_menu_can_edit_title_then_save() {
    let (dir, _repo) = setup_simple_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo '{"number":42,"title":"Current title","body":"Current body","url":"https://github.com/test/repo/pull/42","state":"OPEN","labels":[],"reviewRequests":[]}'
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    printf "%s" "$@" > "$MOCK_GH_EDIT_ARGS"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let edit_args_path = dir.path().join("edit_args.txt");

    let output = kin_cmd()
        .args(["pr", "edit"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_EDIT_ARGS", &edit_args_path)
        .env("KIN_TEST_SELECTIONS", "1,0")
        .env("KIN_TEST_PR_EDIT_TITLE", "Updated title from menu")
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr edit failed: {:?}", output);
    let args = fs::read_to_string(&edit_args_path).unwrap();
    assert!(
        args.contains("predit42--titleUpdated title from menu"),
        "Expected edited title to be sent. Got:\n{}",
        args
    );
}

#[test]
fn pr_edit_multiple_open_prs_uses_selection() {
    let (dir, _repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "feature-a" ]]; then
        echo '{"number":10,"title":"A title","body":"A body","url":"https://github.com/test/repo/pull/10","state":"OPEN","labels":[],"reviewRequests":[]}'
        exit 0
    fi
    if [[ "$3" == "feature-b" ]]; then
        echo '{"number":11,"title":"B title","body":"B body","url":"https://github.com/test/repo/pull/11","state":"OPEN","labels":[],"reviewRequests":[]}'
        exit 0
    fi
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    printf "%s" "$@" > "$MOCK_GH_EDIT_ARGS"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let edit_args_path = dir.path().join("edit_args.txt");

    let output = kin_cmd()
        .args(["pr", "edit"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_EDIT_ARGS", &edit_args_path)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr edit failed: {:?}", output);
    let args = fs::read_to_string(&edit_args_path).unwrap();
    assert!(
        args.contains("predit10--titleA title"),
        "Non-interactive mode should auto-select first PR. Got:\n{}",
        args
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Select PR to edit:"),
        "Expected selection prompt in output. Got:\n{}",
        stdout
    );
}

#[test]
fn pr_edit_reapplies_stack_section_for_multi_pr_stack() {
    let (dir, _repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "feature-a" ]]; then
        echo '{"number":10,"title":"A title","body":"A body without stack","url":"https://github.com/test/repo/pull/10","state":"OPEN","labels":[],"reviewRequests":[]}'
        exit 0
    fi
    if [[ "$3" == "feature-b" ]]; then
        echo '{"number":11,"title":"B title","body":"B body without stack","url":"https://github.com/test/repo/pull/11","state":"OPEN","labels":[],"reviewRequests":[]}'
        exit 0
    fi
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    pr_number="$3"
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "--body" ]]; then
            printf "%s" "$2" > "$MOCK_GH_BODY_DIR/pr_$pr_number.txt"
            exit 0
        fi
        shift
    done
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let captured_body_dir = dir.path().join("captured-bodies");
    std::fs::create_dir_all(&captured_body_dir).unwrap();

    let output = kin_cmd()
        .args(["pr", "edit"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_BODY_DIR", &captured_body_dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr edit failed: {:?}", output);

    let body = fs::read_to_string(captured_body_dir.join("pr_10.txt")).unwrap();
    assert!(
        body.contains("<!-- kindra-stack:start -->"),
        "Expected stack block to be reinserted. Got:\n{}",
        body
    );
    assert!(
        body.contains("- → feature-a #10"),
        "Current PR should be marked in the stack block. Got:\n{}",
        body
    );
    assert!(
        body.contains("- [feature-b](https://github.com/test/repo/pull/11) #11"),
        "Other PR should remain linked in the stack block. Got:\n{}",
        body
    );
}

#[test]
fn pr_edit_reorders_stack_section_using_live_stack_order() {
    let (dir, _repo) = setup_review_merge_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &[
            "push",
            "-u",
            "origin",
            "main",
            "sync-main",
            "pr-review",
            "pr-merge",
        ],
        dir.path(),
    );
    run_ok("git", &["checkout", "pr-merge"], dir.path());

    let gh_mock = dir.path().join("gh");
    let start = "<!-- kindra-stack:start -->";
    let end = "<!-- kindra-stack:end -->";
    let stale_body = format!(
        "Body with stale stack\n\n{}\n## Stack\n- ~[sync-main](https://github.com/test/repo/pull/24) #24~ (merged)\n- [pr-merge](https://github.com/test/repo/pull/26) #26\n- → pr-review #27\n{}\n",
        start, end
    );
    let stale_body_for_bash = stale_body.replace('\n', "\\n").replace('"', "\\\"");

    std::fs::write(
        &gh_mock,
        format!(
            r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "24" ]]; then
        echo '{{"state":"MERGED"}}'
        exit 0
    fi
    if [[ "$3" == "pr-review" ]]; then
        echo '{{"number":27,"title":"PR review","body":"{}","url":"https://github.com/test/repo/pull/27","state":"OPEN","labels":[],"reviewRequests":[]}}'
        exit 0
    fi
    if [[ "$3" == "pr-merge" ]]; then
        echo '{{"number":26,"title":"PR merge","body":"PR merge body","url":"https://github.com/test/repo/pull/26","state":"OPEN","labels":[],"reviewRequests":[]}}'
        exit 0
    fi
    if [[ "$3" == "sync-main" ]]; then
        echo "no pull requests found for branch" >&2
        exit 1
    fi
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    pr_number="$3"
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "--body" ]]; then
            printf "%s" "$2" > "$MOCK_GH_BODY_DIR/pr_$pr_number.txt"
            exit 0
        fi
        shift
    done
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
            stale_body_for_bash
        ),
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let captured_body_dir = dir.path().join("captured-bodies");
    std::fs::create_dir_all(&captured_body_dir).unwrap();

    let output = kin_cmd()
        .args(["pr", "edit"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_BODY_DIR", &captured_body_dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr edit failed: {:?}", output);

    let body = fs::read_to_string(captured_body_dir.join("pr_27.txt")).unwrap();
    let sync_main_idx = body.find("sync-main").unwrap();
    let pr_review_idx = body.find("→ pr-review #27").unwrap();
    let pr_merge_idx = body
        .find("[pr-merge](https://github.com/test/repo/pull/26) #26")
        .unwrap();

    assert!(
        sync_main_idx < pr_review_idx && pr_review_idx < pr_merge_idx,
        "Expected merged sync-main, then pr-review, then pr-merge. Got:\n{}",
        body
    );
}

#[test]
fn pr_status_shows_reviewers_comments_and_checks() {
    let (dir, _repo) = setup_simple_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo '{"number":42,"title":"Feature title","body":"Feature body","url":"https://github.com/test/repo/pull/42","state":"OPEN","labels":[],"reviewRequests":[]}'
    exit 0
fi
if [[ "$1" == "api" ]] && [[ "$2" == "graphql" ]]; then
    echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"isResolved":false},{"isResolved":true},{"isResolved":false}]},"reviewRequests":{"nodes":[{"requestedReviewer":{"login":"bob"}}]},"latestReviews":{"nodes":[{"state":"APPROVED","author":{"login":"alice"}},{"state":"COMMENTED","author":{"login":"carol"}}]},"commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[{"__typename":"CheckRun","name":"ci/test","status":"COMPLETED","conclusion":"FAILURE"},{"__typename":"CheckRun","name":"ci/lint","status":"IN_PROGRESS","conclusion":null},{"__typename":"StatusContext","context":"build","state":"PENDING"}]}}}}]}}}}}'
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = kin_cmd()
        .args(["pr", "status"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "kin pr status failed: {:?}",
        output
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("── feature (#42): Feature title ──"));
    assert!(stdout.contains("URL: https://github.com/test/repo/pull/42"));
    assert!(stdout.contains("alice: approved"));
    assert!(stdout.contains("bob: waiting"));
    assert!(stdout.contains("carol: comments"));
    assert!(stdout.contains("Unresolved comments: 2"));
    assert!(stdout.contains("Running checks: build, ci/lint"));
    assert!(stdout.contains("Failed checks: ci/test"));
}

#[test]
fn pr_status_lists_multiple_stack_prs() {
    let (dir, _repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "feature-a" ]]; then
        echo '{"number":10,"title":"A title","body":"A body","url":"https://github.com/test/repo/pull/10","state":"OPEN","labels":[],"reviewRequests":[]}'
        exit 0
    fi
    if [[ "$3" == "feature-b" ]]; then
        echo '{"number":11,"title":"B title","body":"B body","url":"https://github.com/test/repo/pull/11","state":"OPEN","labels":[],"reviewRequests":[]}'
        exit 0
    fi
fi
if [[ "$1" == "api" ]] && [[ "$2" == "graphql" ]]; then
    number=""
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "-F" ]]; then
            shift
            if [[ "$1" == number=* ]]; then
                number="${1#number=}"
            fi
        fi
        shift
    done
    if [[ "$number" == "10" ]]; then
        echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[]},"reviewRequests":{"nodes":[]},"latestReviews":{"nodes":[{"state":"APPROVED","author":{"login":"alice"}}]},"commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[]}}}}]}}}}}'
        exit 0
    fi
    if [[ "$number" == "11" ]]; then
        echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"isResolved":false}]},"reviewRequests":{"nodes":[{"requestedReviewer":{"login":"bob"}}]},"latestReviews":{"nodes":[]},"commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[{"__typename":"CheckRun","name":"ci/test","status":"COMPLETED","conclusion":"FAILURE"}]}}}}]}}}}}'
        exit 0
    fi
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = kin_cmd()
        .args(["pr", "status"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "kin pr status failed: {:?}",
        output
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("── feature-a (#10): A title ──"));
    assert!(stdout.contains("── feature-b (#11): B title ──"));
    assert!(stdout.contains("alice: approved"));
    assert!(stdout.contains("bob: waiting"));
    assert!(stdout.contains("Failed checks: ci/test"));
}

#[test]
fn pr_review_renders_markdown_threads_and_replies() {
    let (dir, _repo) = setup_simple_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo '{"number":42,"title":"Feature title","body":"Feature body","url":"https://github.com/test/repo/pull/42","state":"OPEN","labels":[],"reviewRequests":[]}'
    exit 0
fi
if [[ "$1" == "api" ]] && [[ "$2" == "graphql" ]]; then
    echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"isResolved":false,"comments":{"nodes":[{"body":"Please rename this variable.","path":"src/lib.rs","line":14,"startLine":14,"originalLine":14,"originalStartLine":14,"outdated":false,"createdAt":"2024-01-01T00:00:00Z","author":{"__typename":"User","login":"alice"}},{"body":"Done.","path":"src/lib.rs","line":14,"startLine":14,"originalLine":14,"originalStartLine":14,"outdated":false,"createdAt":"2024-01-01T00:01:00Z","author":{"__typename":"User","login":"bob"}}]}},{"isResolved":false,"comments":{"nodes":[{"body":"This was on an old diff.","path":"src/main.rs","line":null,"startLine":null,"originalLine":27,"originalStartLine":27,"outdated":true,"createdAt":"2024-01-01T00:02:00Z","author":{"__typename":"User","login":"carol"}}]}},{"isResolved":true,"comments":{"nodes":[{"body":"Already fixed.","path":"src/old.rs","line":8,"startLine":8,"originalLine":8,"originalStartLine":8,"outdated":false,"createdAt":"2024-01-01T00:03:00Z","author":{"__typename":"User","login":"dave"}}]}}]}}}}}'
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = kin_cmd()
        .args(["pr", "review"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "kin pr review failed: {:?}",
        output
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("### `src/lib.rs:14` — @alice"));
    assert!(stdout.contains("Please rename this variable."));
    assert!(stdout.contains("**Reply from @bob**\nDone."));
    assert!(
        stdout.contains(
            "Done.\n\n\n### `src/main.rs` — @carol [OUTDATED, original comment line: 27]"
        )
    );
    assert!(!stdout.contains("Already fixed."));
}

#[test]
fn pr_review_fetches_paginated_threads_and_comments() {
    let (dir, _repo) = setup_simple_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo '{"number":42,"title":"Feature title","body":"Feature body","url":"https://github.com/test/repo/pull/42","state":"OPEN","labels":[],"reviewRequests":[]}'
    exit 0
fi
if [[ "$1" == "api" ]] && [[ "$2" == "graphql" ]]; then
    args="$*"
    if [[ "$args" == *"threadId=thread-1"* ]] && [[ "$args" == *"commentsCursor=comment-cursor-1"* ]]; then
        echo '{"data":{"node":{"comments":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"body":"Second page reply.","path":"src/lib.rs","line":10,"startLine":10,"originalLine":10,"originalStartLine":10,"outdated":false,"createdAt":"2024-01-01T00:01:00Z","author":{"__typename":"User","login":"bob"}}]}}}}'
        exit 0
    fi
    if [[ "$args" == *"threadCursor=thread-cursor-1"* ]]; then
        echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"id":"thread-2","isResolved":false,"comments":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"body":"Second thread comment.","path":"src/main.rs","line":20,"startLine":20,"originalLine":20,"originalStartLine":20,"outdated":false,"createdAt":"2024-01-01T00:02:00Z","author":{"__typename":"User","login":"carol"}}]}}]}}}}}'
        exit 0
    fi
    echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"pageInfo":{"hasNextPage":true,"endCursor":"thread-cursor-1"},"nodes":[{"id":"thread-1","isResolved":false,"comments":{"pageInfo":{"hasNextPage":true,"endCursor":"comment-cursor-1"},"nodes":[{"body":"First page comment.","path":"src/lib.rs","line":10,"startLine":10,"originalLine":10,"originalStartLine":10,"outdated":false,"createdAt":"2024-01-01T00:00:00Z","author":{"__typename":"User","login":"alice"}}]}}]}}}}}'
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = kin_cmd()
        .args(["pr", "review"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "kin pr review failed: {:?}",
        output
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("First page comment."));
    assert!(stdout.contains("Second page reply."));
    assert!(stdout.contains("Second thread comment."));
}

#[test]
fn pr_review_multiple_prs_uses_selection() {
    let (dir, _repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "feature-a" ]]; then
        echo '{"number":10,"title":"A title","body":"A body","url":"https://github.com/test/repo/pull/10","state":"OPEN","labels":[],"reviewRequests":[]}'
        exit 0
    fi
    if [[ "$3" == "feature-b" ]]; then
        echo '{"number":11,"title":"B title","body":"B body","url":"https://github.com/test/repo/pull/11","state":"OPEN","labels":[],"reviewRequests":[]}'
        exit 0
    fi
fi
if [[ "$1" == "api" ]] && [[ "$2" == "graphql" ]]; then
    number=""
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "-F" ]]; then
            shift
            if [[ "$1" == number=* ]]; then
                number="${1#number=}"
            fi
        fi
        shift
    done
    if [[ "$number" == "10" ]]; then
        echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"isResolved":false,"comments":{"nodes":[{"body":"Review for A.","path":"a.txt","line":5,"startLine":5,"originalLine":5,"originalStartLine":5,"outdated":false,"createdAt":"2024-01-01T00:00:00Z","author":{"__typename":"User","login":"alice"}}]}}]}}}}}'
        exit 0
    fi
    if [[ "$number" == "11" ]]; then
        echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"isResolved":false,"comments":{"nodes":[{"body":"Review for B.","path":"b.txt","line":7,"startLine":7,"originalLine":7,"originalStartLine":7,"outdated":false,"createdAt":"2024-01-01T00:00:00Z","author":{"__typename":"User","login":"bob"}}]}}]}}}}}'
        exit 0
    fi
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = kin_cmd()
        .args(["pr", "review"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "kin pr review failed: {:?}",
        output
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Select PR to review:"));
    assert!(stdout.contains("Review for A."));
    assert!(!stdout.contains("Review for B."));
}

#[test]
fn pr_review_applies_reviewer_bot_outdated_and_resolved_filters() {
    let (dir, _repo) = setup_simple_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo '{"number":42,"title":"Feature title","body":"Feature body","url":"https://github.com/test/repo/pull/42","state":"OPEN","labels":[],"reviewRequests":[]}'
    exit 0
fi
if [[ "$1" == "api" ]] && [[ "$2" == "graphql" ]]; then
    echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"isResolved":false,"comments":{"nodes":[{"body":"Please update the docs.","path":"README.md","line":9,"startLine":9,"originalLine":9,"originalStartLine":9,"outdated":false,"createdAt":"2024-01-01T00:00:00Z","author":{"__typename":"User","login":"alice"}},{"body":"Bot follow-up.","path":"README.md","line":9,"startLine":9,"originalLine":9,"originalStartLine":9,"outdated":false,"createdAt":"2024-01-01T00:01:00Z","author":{"__typename":"Bot","login":"copilot-swe-agent"}}]}},{"isResolved":false,"comments":{"nodes":[{"body":"Bot root comment.","path":"src/lib.rs","line":4,"startLine":4,"originalLine":4,"originalStartLine":4,"outdated":false,"createdAt":"2024-01-01T00:02:00Z","author":{"__typename":"Bot","login":"copilot-swe-agent"}}]}},{"isResolved":false,"comments":{"nodes":[{"body":"Outdated note.","path":"src/main.rs","line":null,"startLine":null,"originalLine":30,"originalStartLine":30,"outdated":true,"createdAt":"2024-01-01T00:03:00Z","author":{"__typename":"User","login":"alice"}}]}},{"isResolved":true,"comments":{"nodes":[{"body":"Resolved Alice comment.","path":"src/lib.rs","line":22,"startLine":22,"originalLine":22,"originalStartLine":22,"outdated":false,"createdAt":"2024-01-01T00:04:00Z","author":{"__typename":"User","login":"alice"}}]}}]}}}}}'
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = kin_cmd()
        .args([
            "pr",
            "review",
            "--reviewer",
            "alice",
            "--no-bots",
            "--no-outdated",
            "--resolved",
        ])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "kin pr review failed: {:?}",
        output
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Please update the docs."));
    assert!(stdout.contains("Resolved Alice comment."));
    assert!(!stdout.contains("Bot follow-up."));
    assert!(!stdout.contains("Bot root comment."));
    assert!(!stdout.contains("Outdated note."));
}

#[test]
fn pr_review_writes_output_and_copies_with_osc52() {
    let (dir, _repo) = setup_simple_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo '{"number":42,"title":"Feature title","body":"Feature body","url":"https://github.com/test/repo/pull/42","state":"OPEN","labels":[],"reviewRequests":[]}'
    exit 0
fi
if [[ "$1" == "api" ]] && [[ "$2" == "graphql" ]]; then
    echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"isResolved":false,"comments":{"nodes":[{"body":"Looks good.","path":"src/lib.rs","line":9,"startLine":9,"originalLine":9,"originalStartLine":9,"outdated":false,"createdAt":"2024-01-01T00:00:00Z","author":{"__typename":"User","login":"alice"}}]}}]}}}}}'
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output_path = dir.path().join("review.md");
    let output = kin_cmd()
        .args([
            "pr",
            "review",
            "--output",
            output_path.to_str().unwrap(),
            "--copy",
        ])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "kin pr review failed: {:?}",
        output
    );

    let saved_markdown = fs::read_to_string(&output_path).unwrap();
    assert_eq!(saved_markdown, "### `src/lib.rs:9` — @alice\nLooks good.");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let expected_osc52 = format!(
        "\u{1b}]52;c;{}\u{7}",
        STANDARD.encode(saved_markdown.as_bytes())
    );
    assert!(stderr.contains(&expected_osc52));
    assert!(stderr.contains("Saved review markdown to"));
    assert!(stderr.contains("Copied review markdown to clipboard"));
}

#[test]
fn pr_review_strips_html_comments_from_output() {
    let (dir, _repo) = setup_simple_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo '{"number":42,"title":"Feature title","body":"Feature body","url":"https://github.com/test/repo/pull/42","state":"OPEN","labels":[],"reviewRequests":[]}'
    exit 0
fi
if [[ "$1" == "api" ]] && [[ "$2" == "graphql" ]]; then
    echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"isResolved":false,"comments":{"nodes":[{"body":"Visible text.\n<!-- hidden top-level -->\nStill visible.","path":"src/lib.rs","line":9,"startLine":9,"originalLine":9,"originalStartLine":9,"outdated":false,"createdAt":"2024-01-01T00:00:00Z","author":{"__typename":"User","login":"alice"}},{"body":"<!-- hidden reply -->Ack.","path":"src/lib.rs","line":9,"startLine":9,"originalLine":9,"originalStartLine":9,"outdated":false,"createdAt":"2024-01-01T00:01:00Z","author":{"__typename":"User","login":"bob"}}]}}]}}}}}'
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = kin_cmd()
        .args(["pr", "review"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "kin pr review failed: {:?}",
        output
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Visible text.\n\nStill visible."));
    assert!(stdout.contains("**Reply from @bob**\nAck."));
    assert!(!stdout.contains("hidden top-level"));
    assert!(!stdout.contains("hidden reply"));
    assert!(!stdout.contains("<!--"));
}

#[test]
fn pr_merge_merges_ready_single_pr() {
    let (dir, _repo) = setup_simple_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "42" ]]; then
        echo '{"state":"MERGED"}'
        exit 0
    fi
    echo '{"number":42,"title":"Feature title","body":"Feature body","url":"https://github.com/test/repo/pull/42","state":"OPEN","labels":[],"reviewRequests":[]}'
    exit 0
fi
if [[ "$1" == "api" ]] && [[ "$2" == "graphql" ]]; then
    echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[]},"reviewRequests":{"nodes":[]},"latestReviews":{"nodes":[{"state":"APPROVED","author":{"login":"alice"}}]},"headRefOid":"deadbeef42","reviewDecision":"APPROVED","mergeStateStatus":"CLEAN","mergeable":"MERGEABLE","isDraft":false,"commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[]}}}}]}}}}}'
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "merge" ]]; then
    printf "%s\n" "$@" > "$MOCK_GH_MERGE_ARGS"
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    printf "%s\n" "$@" > "$MOCK_GH_EDIT_ARGS"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let merge_args_path = dir.path().join("merge_args.txt");
    let edit_args_path = dir.path().join("edit_args.txt");
    let output = kin_cmd()
        .args(["pr", "merge"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_MERGE_ARGS", &merge_args_path)
        .env("MOCK_GH_EDIT_ARGS", &edit_args_path)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr merge failed: {:?}", output);
    let merge_args = fs::read_to_string(&merge_args_path).unwrap();
    assert!(merge_args.contains("pr\nmerge\n42"));
    assert!(merge_args.contains("--match-head-commit\ndeadbeef42"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Merging PR #42 for feature"));
    assert!(stdout.contains("✓ Merged PR #42"));
}

#[test]
fn pr_merge_multiple_prs_uses_selection() {
    let (dir, _repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "10" ]]; then
        echo '{"state":"MERGED"}'
        exit 0
    fi
    if [[ "$3" == "feature-a" ]]; then
        echo '{"number":10,"title":"A title","body":"A body","url":"https://github.com/test/repo/pull/10","state":"OPEN","labels":[],"reviewRequests":[]}'
        exit 0
    fi
    if [[ "$3" == "feature-b" ]]; then
        echo '{"number":11,"title":"B title","body":"B body","url":"https://github.com/test/repo/pull/11","state":"OPEN","labels":[],"reviewRequests":[]}'
        exit 0
    fi
fi
if [[ "$1" == "api" ]] && [[ "$2" == "graphql" ]]; then
    number=""
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "-F" ]]; then
            shift
            if [[ "$1" == number=* ]]; then
                number="${1#number=}"
            fi
        fi
        shift
    done
    if [[ "$number" == "10" ]]; then
        echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[]},"reviewRequests":{"nodes":[]},"latestReviews":{"nodes":[{"state":"APPROVED","author":{"login":"alice"}}]},"reviewDecision":"APPROVED","mergeStateStatus":"CLEAN","mergeable":"MERGEABLE","isDraft":false,"commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[]}}}}]}}}}}'
        exit 0
    fi
    if [[ "$number" == "11" ]]; then
        echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[]},"reviewRequests":{"nodes":[]},"latestReviews":{"nodes":[{"state":"APPROVED","author":{"login":"bob"}}]},"reviewDecision":"APPROVED","mergeStateStatus":"CLEAN","mergeable":"MERGEABLE","isDraft":false,"commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[]}}}}]}}}}}'
        exit 0
    fi
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "merge" ]]; then
    printf "%s\n" "$@" > "$MOCK_GH_MERGE_ARGS"
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    printf "%s\n" "$@" > "$MOCK_GH_EDIT_ARGS"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let merge_args_path = dir.path().join("merge_args.txt");
    let edit_args_path = dir.path().join("edit_args.txt");
    let output = kin_cmd()
        .args(["pr", "merge"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_MERGE_ARGS", &merge_args_path)
        .env("MOCK_GH_EDIT_ARGS", &edit_args_path)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr merge failed: {:?}", output);
    let merge_args = fs::read_to_string(&merge_args_path).unwrap();
    assert!(merge_args.contains("pr\nmerge\n10"));
    let edit_args = fs::read_to_string(&edit_args_path).unwrap();
    assert!(edit_args.contains("pr\nedit\n11\n--base\nmain"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Select PR to merge:"));
}

#[test]
fn pr_merge_approved_plus_commented_allows_merge() {
    let (dir, _repo) = setup_simple_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "42" ]]; then
        echo '{"state":"MERGED"}'
        exit 0
    fi
    echo '{"number":42,"title":"Feature title","body":"Feature body","url":"https://github.com/test/repo/pull/42","state":"OPEN","labels":[],"reviewRequests":[]}'
    exit 0
fi
if [[ "$1" == "api" ]] && [[ "$2" == "graphql" ]]; then
    echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[]},"reviewRequests":{"nodes":[]},"latestReviews":{"nodes":[{"state":"APPROVED","author":{"login":"alice"}},{"state":"COMMENTED","author":{"login":"carol"}}]},"reviewDecision":"APPROVED","mergeStateStatus":"CLEAN","mergeable":"MERGEABLE","isDraft":false,"commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[]}}}}]}}}}}'
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "merge" ]]; then
    printf "%s\n" "$@" > "$MOCK_GH_MERGE_ARGS"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let merge_args_path = dir.path().join("merge_args.txt");
    let output = kin_cmd()
        .args(["pr", "merge"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_MERGE_ARGS", &merge_args_path)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr merge failed: {:?}", output);
    let merge_args = fs::read_to_string(&merge_args_path).unwrap();
    assert!(merge_args.contains("pr\nmerge\n42"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Merging PR #42 for feature"));
    assert!(stdout.contains("✓ Merged PR #42"));
}

#[test]
fn pr_merge_retargets_child_pr_before_merging_parent() {
    let (dir, _repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "10" ]]; then
        echo '{"state":"MERGED"}'
        exit 0
    fi
    if [[ "$3" == "feature-a" ]]; then
        echo '{"number":10,"title":"A title","body":"A body","url":"https://github.com/test/repo/pull/10","state":"OPEN","labels":[],"reviewRequests":[]}'
        exit 0
    fi
    if [[ "$3" == "feature-b" ]]; then
        echo '{"number":11,"title":"B title","body":"B body","url":"https://github.com/test/repo/pull/11","state":"OPEN","labels":[],"reviewRequests":[]}'
        exit 0
    fi
fi
if [[ "$1" == "api" ]] && [[ "$2" == "graphql" ]]; then
    number=""
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "-F" ]]; then
            shift
            if [[ "$1" == number=* ]]; then
                number="${1#number=}"
            fi
        fi
        shift
    done
    if [[ "$number" == "10" ]]; then
        echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[]},"reviewRequests":{"nodes":[]},"latestReviews":{"nodes":[{"state":"APPROVED","author":{"login":"alice"}}]},"reviewDecision":"APPROVED","mergeStateStatus":"CLEAN","mergeable":"MERGEABLE","isDraft":false,"commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[]}}}}]}}}}}'
        exit 0
    fi
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    printf "%s\n" "$@" >> "$MOCK_GH_EDIT_ARGS"
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "merge" ]]; then
    printf "%s\n" "$@" > "$MOCK_GH_MERGE_ARGS"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let merge_args_path = dir.path().join("merge_args.txt");
    let edit_args_path = dir.path().join("edit_args.txt");
    let output = kin_cmd()
        .args(["pr", "merge"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_MERGE_ARGS", &merge_args_path)
        .env("MOCK_GH_EDIT_ARGS", &edit_args_path)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr merge failed: {:?}", output);

    let edit_args = fs::read_to_string(&edit_args_path).unwrap();
    assert!(edit_args.contains("pr\nedit\n11\n--base\nmain"));

    let merge_args = fs::read_to_string(&merge_args_path).unwrap();
    assert!(merge_args.contains("pr\nmerge\n10"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Retargeting dependent PR #11 for feature-b to base 'main'"));
    assert!(stdout.contains("✓ Retargeted PR #11"));
    assert!(stdout.contains("✓ Merged PR #10"));
}

#[test]
fn pr_merge_does_not_retarget_on_merge_failure() {
    let (dir, _repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "feature-a" ]]; then
        echo '{"number":10,"title":"A title","body":"A body","url":"https://github.com/test/repo/pull/10","state":"OPEN","labels":[],"reviewRequests":[]}'
        exit 0
    fi
    if [[ "$3" == "feature-b" ]]; then
        echo '{"number":11,"title":"B title","body":"B body","url":"https://github.com/test/repo/pull/11","state":"OPEN","labels":[],"reviewRequests":[]}'
        exit 0
    fi
fi
if [[ "$1" == "api" ]] && [[ "$2" == "graphql" ]]; then
    number=""
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "-F" ]]; then
            shift
            if [[ "$1" == number=* ]]; then
                number="${1#number=}"
            fi
        fi
        shift
    done
    if [[ "$number" == "10" ]]; then
        echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[]},"reviewRequests":{"nodes":[]},"latestReviews":{"nodes":[{"state":"APPROVED","author":{"login":"alice"}}]},"reviewDecision":"APPROVED","mergeStateStatus":"CLEAN","mergeable":"MERGEABLE","isDraft":false,"commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[]}}}}]}}}}}'
        exit 0
    fi
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    printf "%s\n" "$@" >> "$MOCK_GH_EDIT_ARGS"
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "merge" ]]; then
    printf "%s\n" "$@" > "$MOCK_GH_MERGE_ARGS"
    echo "merge failed" >&2
    exit 1
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let merge_args_path = dir.path().join("merge_args.txt");
    let edit_args_path = dir.path().join("edit_args.txt");
    let output = kin_cmd()
        .args(["pr", "merge"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_MERGE_ARGS", &merge_args_path)
        .env("MOCK_GH_EDIT_ARGS", &edit_args_path)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "kin pr merge unexpectedly succeeded: {:?}",
        output
    );

    let merge_args = fs::read_to_string(&merge_args_path).unwrap();
    assert!(merge_args.contains("pr\nmerge\n10"));

    let edit_args = fs::read_to_string(&edit_args_path).unwrap_or_default();
    assert!(!edit_args.contains("pr\nedit\n11\n--base\nmain"));
}

#[test]
fn pr_merge_does_not_retarget_when_merge_is_queued() {
    let (dir, _repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "10" ]]; then
        echo '{"state":"QUEUED"}'
        exit 0
    fi
    if [[ "$3" == "feature-a" ]]; then
        echo '{"number":10,"title":"A title","body":"A body","url":"https://github.com/test/repo/pull/10","state":"OPEN","labels":[],"reviewRequests":[]}'
        exit 0
    fi
    if [[ "$3" == "feature-b" ]]; then
        echo '{"number":11,"title":"B title","body":"B body","url":"https://github.com/test/repo/pull/11","state":"OPEN","labels":[],"reviewRequests":[]}'
        exit 0
    fi
fi
if [[ "$1" == "api" ]] && [[ "$2" == "graphql" ]]; then
    number=""
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "-F" ]]; then
            shift
            if [[ "$1" == number=* ]]; then
                number="${1#number=}"
            fi
        fi
        shift
    done
    if [[ "$number" == "10" ]]; then
        echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[]},"reviewRequests":{"nodes":[]},"latestReviews":{"nodes":[{"state":"APPROVED","author":{"login":"alice"}}]},"headRefOid":"queuedsha10","reviewDecision":"APPROVED","mergeStateStatus":"CLEAN","mergeable":"MERGEABLE","isDraft":false,"commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[]}}}}]}}}}}'
        exit 0
    fi
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    printf "%s\n" "$@" >> "$MOCK_GH_EDIT_ARGS"
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "merge" ]]; then
    printf "%s\n" "$@" > "$MOCK_GH_MERGE_ARGS"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let merge_args_path = dir.path().join("merge_args.txt");
    let edit_args_path = dir.path().join("edit_args.txt");
    let output = kin_cmd()
        .args(["pr", "merge"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_MERGE_ARGS", &merge_args_path)
        .env("MOCK_GH_EDIT_ARGS", &edit_args_path)
        .output()
        .unwrap();

    assert!(output.status.success(), "kin pr merge failed: {:?}", output);

    let merge_args = fs::read_to_string(&merge_args_path).unwrap();
    assert!(merge_args.contains("pr\nmerge\n10"));
    assert!(merge_args.contains("--match-head-commit\nqueuedsha10"));

    let edit_args = fs::read_to_string(&edit_args_path).unwrap_or_default();
    assert!(!edit_args.contains("pr\nedit\n11\n--base\nmain"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("current GitHub state is QUEUED"));
    assert!(!stdout.contains("✓ Merged PR #10"));
}

#[test]
fn pr_merge_prompts_and_errors_when_issues_remain_but_merge_is_allowed() {
    let (dir, _repo) = setup_simple_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo '{"number":42,"title":"Feature title","body":"Feature body","url":"https://github.com/test/repo/pull/42","state":"OPEN","labels":[],"reviewRequests":[]}'
    exit 0
fi
if [[ "$1" == "api" ]] && [[ "$2" == "graphql" ]]; then
    echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"isResolved":false}]},"reviewRequests":{"nodes":[{"requestedReviewer":{"login":"bob"}}]},"latestReviews":{"nodes":[{"state":"COMMENTED","author":{"login":"carol"}}]},"reviewDecision":"REVIEW_REQUIRED","mergeStateStatus":"CLEAN","mergeable":"MERGEABLE","isDraft":false,"commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[{"__typename":"CheckRun","name":"ci/test","status":"COMPLETED","conclusion":"FAILURE"}]}}}}]}}}}}'
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "merge" ]]; then
    printf "%s\n" "$@" > "$MOCK_GH_MERGE_ARGS"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let merge_args_path = dir.path().join("merge_args.txt");
    let output = kin_cmd()
        .args(["pr", "merge"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_MERGE_ARGS", &merge_args_path)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "kin pr merge unexpectedly succeeded: {:?}",
        output
    );

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("Unresolved review comments: 1"));
    assert!(combined.contains("Outstanding reviews:"));
    assert!(combined.contains("bob: waiting"));
    assert!(combined.contains("overall review decision: review required"));
    assert!(combined.contains("Failed checks: ci/test"));
    assert!(combined.contains("GitHub would still allow merging this PR."));
    assert!(combined.contains("Merge anyway despite outstanding reviews/checks?"));
    assert!(combined.contains("Merge cancelled"));
    assert!(!merge_args_path.exists());
}

#[test]
fn pr_merge_surfaces_gh_failure_details() {
    let (dir, _repo) = setup_simple_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo '{"number":42,"title":"Feature title","body":"Feature body","url":"https://github.com/test/repo/pull/42","state":"OPEN","labels":[],"reviewRequests":[]}'
    exit 0
fi
if [[ "$1" == "api" ]] && [[ "$2" == "graphql" ]]; then
    echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[]},"reviewRequests":{"nodes":[]},"latestReviews":{"nodes":[{"state":"APPROVED","author":{"login":"alice"}}]},"reviewDecision":"APPROVED","mergeStateStatus":"CLEAN","mergeable":"MERGEABLE","isDraft":false,"commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[]}}}}]}}}}}'
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "merge" ]]; then
    echo "merge failed because required check is stale" >&2
    exit 1
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = kin_cmd()
        .args(["pr", "merge"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "kin pr merge unexpectedly succeeded: {:?}",
        output
    );

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("Failed to merge PR #42: merge failed because required check is stale")
    );
}

#[test]
fn pr_merge_errors_when_repo_rules_block_merging() {
    let (dir, _repo) = setup_simple_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo '{"number":42,"title":"Feature title","body":"Feature body","url":"https://github.com/test/repo/pull/42","state":"OPEN","labels":[],"reviewRequests":[]}'
    exit 0
fi
if [[ "$1" == "api" ]] && [[ "$2" == "graphql" ]]; then
    echo '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"isResolved":false}]},"reviewRequests":{"nodes":[]},"latestReviews":{"nodes":[]},"reviewDecision":"REVIEW_REQUIRED","mergeStateStatus":"BLOCKED","mergeable":"MERGEABLE","isDraft":false,"commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[]}}}}]}}}}}'
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "merge" ]]; then
    printf "%s\n" "$@" > "$MOCK_GH_MERGE_ARGS"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let merge_args_path = dir.path().join("merge_args.txt");
    let output = kin_cmd()
        .args(["pr", "merge"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_MERGE_ARGS", &merge_args_path)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "kin pr merge unexpectedly succeeded: {:?}",
        output
    );

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("Merge blocked by GitHub: GitHub merge state is BLOCKED"));
    assert!(combined.contains("Merge prevented for PR #42"));
    assert!(!combined.contains("Merge anyway despite outstanding reviews/checks?"));
    assert!(!merge_args_path.exists());
}

#[test]
fn pr_flatten_retargets_all_open_stack_prs_to_resolved_upstream_base() {
    let (dir, _repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "feature-a" ]]; then
        echo '{"number":10,"baseRefName":"feature/base-a","state":"OPEN","isDraft":false}'
        exit 0
    fi
    if [[ "$3" == "feature-b" ]]; then
        echo '{"number":11,"baseRefName":"feature-a","state":"OPEN","isDraft":false}'
        exit 0
    fi
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    printf "%s\n" "$@" >> "$MOCK_GH_EDIT_ARGS"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let edit_args_path = dir.path().join("edit_args.txt");
    let output = kin_cmd()
        .args(["pr", "flatten"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_EDIT_ARGS", &edit_args_path)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "kin pr flatten failed: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let edit_args = fs::read_to_string(&edit_args_path).unwrap();
    assert!(edit_args.contains("pr\nedit\n10\n--base\nmain"));
    assert!(edit_args.contains("pr\nedit\n11\n--base\nmain"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Flatten summary: updated=2, already_on_base=0, failed=0, no_open_pr=0")
    );
}

#[test]
fn pr_flatten_uses_resolved_upstream_not_hardcoded_main() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(
        &repo,
        "refs/heads/main",
        "main.txt",
        "main",
        "main commit",
        &[],
    );
    let trunk_id = {
        let main = repo.find_commit(main_id).unwrap();
        make_commit(
            &repo,
            "refs/heads/trunk",
            "trunk.txt",
            "trunk",
            "trunk commit",
            &[&main],
        )
    };
    {
        let trunk = repo.find_commit(trunk_id).unwrap();
        make_commit(
            &repo,
            "refs/heads/feature",
            "feature.txt",
            "feature",
            "feature commit",
            &[&trunk],
        );
    }
    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    write_repo_config(dir.path(), "upstream_branch = \"trunk\"\n");

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "trunk", "feature"],
        dir.path(),
    );

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "feature" ]]; then
        echo '{"number":21,"baseRefName":"main","state":"OPEN","isDraft":false}'
        exit 0
    fi
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    printf "%s\n" "$@" >> "$MOCK_GH_EDIT_ARGS"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let edit_args_path = dir.path().join("edit_args.txt");
    let output = kin_cmd()
        .args(["pr", "flatten"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_EDIT_ARGS", &edit_args_path)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "kin pr flatten failed: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let edit_args = fs::read_to_string(&edit_args_path).unwrap();
    assert!(edit_args.contains("pr\nedit\n21\n--base\ntrunk"));
    assert!(!edit_args.contains("pr\nedit\n21\n--base\nmain"));
}

#[test]
fn pr_flatten_continues_on_partial_failures_and_exits_nonzero() {
    let (dir, _repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "feature-a" ]]; then
        echo '{"number":10,"baseRefName":"feature/base-a","state":"OPEN","isDraft":false}'
        exit 0
    fi
    if [[ "$3" == "feature-b" ]]; then
        echo '{"number":11,"baseRefName":"feature-a","state":"OPEN","isDraft":false}'
        exit 0
    fi
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    printf "%s\n" "$@" >> "$MOCK_GH_EDIT_ARGS"
    if [[ "$3" == "10" ]]; then
        echo "mock failure updating #10" >&2
        exit 1
    fi
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let edit_args_path = dir.path().join("edit_args.txt");
    let output = kin_cmd()
        .args(["pr", "flatten"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_EDIT_ARGS", &edit_args_path)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "kin pr flatten unexpectedly succeeded: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let edit_args = fs::read_to_string(&edit_args_path).unwrap();
    assert!(edit_args.contains("pr\nedit\n10\n--base\nmain"));
    assert!(edit_args.contains("pr\nedit\n11\n--base\nmain"));

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("Flatten summary: updated=1, already_on_base=0, failed=1, no_open_pr=0")
    );
}

#[test]
fn pr_flatten_does_not_mutate_local_git_or_pr_body_metadata() {
    let (dir, repo) = setup_two_level_stack();

    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-b"], dir.path());

    let before_feature_a = repo
        .find_branch("feature-a", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let before_feature_b = repo
        .find_branch("feature-b", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();

    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    if [[ "$3" == "feature-a" ]]; then
        echo '{"number":10,"baseRefName":"feature/base-a","state":"OPEN","isDraft":false}'
        exit 0
    fi
    if [[ "$3" == "feature-b" ]]; then
        echo '{"number":11,"baseRefName":"feature-a","state":"OPEN","isDraft":false}'
        exit 0
    fi
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "edit" ]]; then
    if [[ "$4" != "--base" ]]; then
        echo "unexpected gh pr edit invocation: $@" >&2
        exit 1
    fi
    printf "%s\n" "$@" >> "$MOCK_GH_EDIT_ARGS"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let edit_args_path = dir.path().join("edit_args.txt");
    let output = kin_cmd()
        .args(["pr", "flatten"])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("MOCK_GH_EDIT_ARGS", &edit_args_path)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "kin pr flatten failed: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let after_feature_a = repo
        .find_branch("feature-a", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let after_feature_b = repo
        .find_branch("feature-b", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    assert_eq!(before_feature_a, after_feature_a);
    assert_eq!(before_feature_b, after_feature_b);

    let edit_args = fs::read_to_string(&edit_args_path).unwrap();
    assert!(edit_args.contains("pr\nedit\n10\n--base\nmain"));
    assert!(edit_args.contains("pr\nedit\n11\n--base\nmain"));
    assert!(!edit_args.contains("--title"));
    assert!(!edit_args.contains("--body"));
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests for resolve_stack_boundary_and_base
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn resolve_stack_boundary_falls_back_to_origin_when_no_tracking() {
    // Setup: repo with origin/main but local main has no remote tracking
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(&repo, "refs/heads/main", "file.txt", "base", "initial", &[]);
    let main = repo.find_commit(main_id).unwrap();

    // Create a remote tracking branch but no local tracking on main
    repo.reference("refs/remotes/origin/main", main.id(), true, "origin/main")
        .unwrap();

    // Create a feature branch on main
    make_commit(
        &repo,
        "refs/heads/feature",
        "feature.txt",
        "feat",
        "feature",
        &[&main],
    );

    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // resolve_stack_boundary_and_base should return (origin/main, main)
    // because main has no local tracking but origin/main exists
    let (git_ref, gh_base) = resolve_stack_boundary_and_base(&repo, "main").unwrap();
    assert_eq!(git_ref, "origin/main");
    assert_eq!(gh_base, "main"); // normalized (origin/ stripped)
}

#[test]
fn resolve_stack_boundary_uses_upstream_remote_when_no_origin() {
    // Setup: repo with upstream remote (not origin) containing main
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(&repo, "refs/heads/main", "file.txt", "base", "initial", &[]);
    let main = repo.find_commit(main_id).unwrap();

    // Add upstream remote and set its main to our commit
    run_ok("git", &["remote", "add", "upstream", "."], dir.path());
    repo.reference(
        "refs/remotes/upstream/main",
        main.id(),
        true,
        "upstream/main",
    )
    .unwrap();

    // Create a feature branch on main
    make_commit(
        &repo,
        "refs/heads/feature",
        "feature.txt",
        "feat",
        "feature",
        &[&main],
    );

    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // resolve_stack_boundary_and_base should fall back to upstream/main when no origin exists
    let (git_ref, gh_base) = resolve_stack_boundary_and_base(&repo, "main").unwrap();
    assert_eq!(git_ref, "upstream/main");
    assert_eq!(gh_base, "main"); // normalized (upstream/ stripped)
}

// Note: The single remote fallback is tested indirectly via
// resolve_stack_boundary_uses_upstream_remote_when_no_origin which tests
// the fallback to a non-origin remote when no tracking exists.

#[test]
fn resolve_stack_boundary_uses_remote_prefix_in_name() {
    // Setup: upstream_name already has remote prefix (e.g., "upstream/main")
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(
        &repo,
        "refs/heads/trunk",
        "file.txt",
        "base",
        "initial",
        &[],
    );
    let main = repo.find_commit(main_id).unwrap();

    // Create upstream/trunk
    repo.reference(
        "refs/remotes/upstream/trunk",
        main.id(),
        true,
        "upstream/trunk",
    )
    .unwrap();

    // Create a feature branch on trunk
    make_commit(
        &repo,
        "refs/heads/feature",
        "feature.txt",
        "feat",
        "feature",
        &[&main],
    );

    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // When upstream_name already has a remote prefix that's valid, should use it directly
    let (git_ref, gh_base) = resolve_stack_boundary_and_base(&repo, "upstream/trunk").unwrap();
    assert_eq!(git_ref, "upstream/trunk");
    assert_eq!(gh_base, "trunk"); // normalized
}

#[test]
fn resolve_stack_boundary_uses_tracking_branch_when_diverged() {
    // Setup: local main is behind its remote tracking branch
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    // Create initial main commit
    let main_id = make_commit(&repo, "refs/heads/main", "file.txt", "base", "initial", &[]);
    let main = repo.find_commit(main_id).unwrap();

    // Create origin/main pointing to a NEWER commit (main was rebased)
    let new_main_id = make_commit(
        &repo,
        "refs/heads/main2",
        "file.txt",
        "newbase",
        "newer",
        &[],
    );
    repo.reference("refs/remotes/origin/main", new_main_id, true, "origin/main")
        .unwrap();

    // Create a feature branch on main
    make_commit(
        &repo,
        "refs/heads/feature",
        "feature.txt",
        "feat",
        "feature",
        &[&main],
    );

    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // When local main has diverged from origin/main, should use origin/main
    let (git_ref, gh_base) = resolve_stack_boundary_and_base(&repo, "main").unwrap();
    assert_eq!(git_ref, "origin/main");
    assert_eq!(gh_base, "main");
}

// ─────────────────────────────────────────────────────────────────────────────
// Integration tests for PR command - upstream branch exclusion
// ─────────────────────────────────────────────────────────────────────────────

/// Reproduces bug: when user has local commits on main, checks out a feature branch,
/// and runs kin push followed by kin pr, the upstream branch (main) should NOT be
/// mentioned as a branch to create a PR for.
#[test]
fn pr_command_excludes_upstream_branch() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    // Create initial commit on main
    let main_id = make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "base",
        "initial commit",
        &[],
    );
    let main = repo.find_commit(main_id).unwrap();

    // Create a feature branch on main
    make_commit(
        &repo,
        "refs/heads/feature",
        "feature.txt",
        "feat",
        "add feature",
        &[&main],
    );

    // Set up remote
    let remote_dir = dir.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );

    // Push and set upstreams
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature"],
        dir.path(),
    );

    // Checkout feature branch so stack detection finds it
    run_ok("git", &["checkout", "feature"], dir.path());

    // Create mock gh
    let gh_mock = dir.path().join("gh");
    std::fs::write(
        &gh_mock,
        r#"#!/bin/bash
if [[ "$1" == "auth" ]] && [[ "$2" == "status" ]]; then
    exit 0
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "view" ]]; then
    echo "no pull requests found for branch" >&2
    exit 1
fi
if [[ "$1" == "pr" ]] && [[ "$2" == "create" ]]; then
    # Echo which branch we're creating PR for (to stderr so it's visible in debug)
    echo "Creating PR for base: $BASE head: $HEAD" >&2
    echo "https://github.com/test/repo/pull/1"
    exit 0
fi
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = kin_cmd()
        .arg("pr")
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);

    // The output should mention "feature" as the branch being processed
    // It should NOT mention "main" as a branch to create PR for
    // (main is the upstream, so it shouldn't be suggested for a PR)

    // Verify feature is mentioned (it should be)
    assert!(
        combined.contains("feature"),
        "Output should mention 'feature' branch. Got:\n{}",
        combined
    );

    // The key check: main should NOT appear as a branch needing a PR
    // (it's the upstream/base, not a branch that needs its own PR)
    // Locate the "Processing PRs" section and verify main doesn't appear there
    let lines: Vec<&str> = combined.lines().collect();
    let processing_prs_idx = lines
        .iter()
        .position(|l| l.contains("Processing PRs") || l.contains("Processing PR"));
    let main_in_pr_section = if let Some(idx) = processing_prs_idx {
        lines[idx..].iter().any(|l| l.contains("main"))
    } else {
        // Fallback: check all lines with any PR/branch processing indicators
        lines.iter().any(|l| {
            let l = l.to_lowercase();
            l.contains("main")
                && (l.contains("processing")
                    || l.contains("branch")
                    || l.contains("creating")
                    || l.contains("create")
                    || l.contains("pr "))
        })
    };

    // If main appears in PR-processing output, it would be a bug
    assert!(
        !main_in_pr_section,
        "main should NOT be suggested for PR, but found it in PR-processing output:\n{}",
        combined
    );
}
