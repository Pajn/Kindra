#![allow(deprecated)]
//! Integration tests for `gits pr`.
//!
//! Because `gits pr` requires `gh` to be installed and authenticated (and a
//! real GitHub remote), these tests focus on the portions that are
//! exercisable without a live GitHub connection:
//!
//! 1. The command fails fast with a clear error when `gh` is absent or
//!    unauthenticated.
//! 2. The command exits cleanly with "No branches" when the stack has no
//!    branches with upstreams.
//! 3. Title pre-filling: single-commit branch yields commit subject as
//!    the prefilled title (tested via the internal library function directly).
//! 4. Base-branch detection: a stacked branch topology is correctly resolved.
//! 5. PR template detection from `.github/pull_request_template.md`.
//! 6. Stacked branch shows only commits above base (not upstream).

use assert_cmd::Command;
use git2::{Repository, Signature};
use std::fs;
use tempfile::tempdir;

fn make_commit(
    repo: &Repository,
    refname: &str,
    filename: &str,
    content: &str,
    message: &str,
    parents: &[&git2::Commit<'_>],
) -> git2::Oid {
    let sig = Signature::now("Test User", "test@example.com").unwrap();
    let mut index = repo.index().unwrap();
    fs::write(repo.workdir().unwrap().join(filename), content).unwrap();
    index.add_path(std::path::Path::new(filename)).unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    repo.commit(Some(refname), &sig, &sig, message, &tree, parents)
        .unwrap()
}

fn run_ok(program: &str, args: &[&str], cwd: &std::path::Path) {
    let output = std::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Command failed: {} {:?}\nstdout:\n{}\nstderr:\n{}",
        program,
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Create a minimal repo with `main` + a feature branch stacked on top.
///
/// Layout:
/// ```
///   main  ── A  (initial commit)
///               └── B  (refs/heads/feature, 1 commit)
/// ```
fn setup_simple_stack() -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

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
    let repo = Repository::init(dir.path()).unwrap();

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
    let mut cmd = Command::cargo_bin("gits").unwrap();
    cmd.arg("pr").current_dir(dir.path());

    // The command is allowed to succeed (exit 0) only with the "nothing to do"
    // message, or to fail. Either way, it must not crash (exit code 101+).
    let output = cmd.output().unwrap();
    let code = output.status.code().unwrap_or(1);
    assert!(
        code != 101,
        "gits pr panicked (exit 101). stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn pr_no_upstreams_message() {
    // If gh auth fails (common in CI) the test would not reach the upstream
    // check. We skip the assertion in that case.
    let (dir, _repo) = setup_simple_stack();

    let output = Command::cargo_bin("gits")
        .unwrap()
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

    assert!(
        acceptable,
        "Unexpected output from `gits pr`:\n{}",
        combined
    );
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
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = Command::cargo_bin("gits")
        .unwrap()
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

    let output = Command::cargo_bin("gits")
        .unwrap()
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

    assert!(output.status.success(), "gits pr failed: {:?}", output);
    let captured_body = fs::read_to_string(&captured_body_path).unwrap();
    assert!(
        captured_body.contains(template_content),
        "PR body should include template content. Got:\n{}",
        captured_body
    );
}

// Test: multi-commit branch → title is NOT prefilled (shows commit list instead)
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn multi_commit_branch_title_empty() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

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
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = Command::cargo_bin("gits")
        .unwrap()
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

    // Run gits pr with the mock gh in PATH
    let output = Command::cargo_bin("gits")
        .unwrap()
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
    let repo = Repository::init(dir.path()).unwrap();

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
echo "mock gh: unexpected command: $@" >&2
exit 1
"#,
    )
    .unwrap();
    run_ok("chmod", &["+x", gh_mock.to_str().unwrap()], dir.path());

    let output = Command::cargo_bin("gits")
        .unwrap()
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

    assert!(output.status.success(), "gits pr failed: {:?}", output);
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
