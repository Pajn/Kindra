//! Integration tests for stack filtering - ensuring merged branches are excluded.

mod common;
use common::{gits_cmd, make_commit, make_commit_at, repo_init, run_ok};
use std::fs;
use tempfile::tempdir;

// ============================================================================
// Test 1: After branch is merged (tip on main), new branch targets main
// ============================================================================

/// Scenario:
/// 1. main at M0
/// 2. Create branch A from main, add commit A1
/// 3. Update main to point to A's commit (simulating merge)
/// 4. Create branch B from main
///
/// When running `gits pr`, B should target main (not A, which is now on main)
#[test]
fn pr_after_branch_merged_into_main() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    // 1. Initial commit on main
    let main_commit = make_commit(
        &repo,
        "refs/heads/main",
        "main.txt",
        "main content",
        "initial commit",
        &[],
    );

    // 2. Create branch A from main, add commit A1
    let a_commit = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "feature A content",
        "feat: add feature A",
        &[&repo.find_commit(main_commit).unwrap()],
    );

    // 3. Update main to point to A's commit (simulating merge into main)
    // First detach HEAD so we can update main
    repo.set_head_detached(main_commit).unwrap();
    repo.branch("main", &repo.find_commit(a_commit).unwrap(), true)
        .unwrap();
    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // 4. Create branch B from main (which now has A's commits)
    let main_tip = repo
        .revparse_single("main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "feature B content",
        "feat: add feature B",
        &[&main_tip],
    );

    // Set HEAD to feature-b
    repo.set_head("refs/heads/feature-b").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Set up remote
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

    // Create mock gh that captures the base branch
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
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "--base" ]]; then
            echo "$2" > "{}"
        fi
        shift
    done
    echo "https://github.com/test/repo/pull/1"
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

    let output = gits_cmd()
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
        "gits pr failed!\nSTDOUT:\n{}\nSTDERR:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);

    println!("Output:\n{}", combined);

    // Read what base was captured
    let base = fs::read_to_string(&captured_base)
        .unwrap_or_default()
        .trim()
        .to_string();
    println!("Captured base: '{}'", base);

    // After A is merged into main (main now at A's commit), B should target main
    // The bug would target "feature-a" instead
    assert_eq!(
        base, "main",
        "After A is merged into main, B should target 'main'. Got: {}",
        base
    );
}

// ============================================================================
// Test 2: Unmerged branches still work correctly
// ============================================================================

/// main -> A -> B (both unmerged)
/// Both should be in the stack: A targets main, B targets A
#[test]
fn pr_unmerged_branches_work() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_commit = make_commit(&repo, "refs/heads/main", "main.txt", "m", "initial", &[]);

    let a_commit = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feat: a",
        &[&repo.find_commit(main_commit).unwrap()],
    );

    make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feat: b",
        &[&repo.find_commit(a_commit).unwrap()],
    );

    repo.set_head("refs/heads/feature-b").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Set up remote
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

    // Create mock gh
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
    while [[ $# -gt 0 ]]; do
        if [[ "$1" == "--base" ]]; then
            echo "$2" >> "{}"
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

    let output = gits_cmd()
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
        "gits pr failed!\nSTDOUT:\n{}\nSTDERR:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}{}", stdout, stderr);

    println!("Output:\n{}", combined);

    // Read captured bases - should have one per branch
    let bases = fs::read_to_string(&captured_base).unwrap_or_default();
    println!("Captured bases:\n{}", bases);

    // Both branches should be processed with correct bases
    // feature-a targets main, feature-b targets feature-a
    let base_lines: std::collections::HashSet<_> = bases
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    let expected: std::collections::HashSet<_> = ["main", "feature-a"].into_iter().collect();

    assert_eq!(
        base_lines, expected,
        "Captured bases did not match exactly. Expected {{'main', 'feature-a'}}, found {:?}",
        base_lines
    );
}

// ============================================================================
// Test 3: Fork siblings (not on current HEAD lineage) are excluded
// ============================================================================

/// Scenario:
/// main -> A -> B (HEAD)
///           -> C (fork sibling of B)
///
/// Only A and B should be in the stack when on B.
#[test]
fn get_stack_branches_excludes_fork_siblings() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    // 1. Initial commit on main
    let main_commit_id = make_commit(
        &repo,
        "refs/heads/main",
        "main.txt",
        "main content",
        "initial commit",
        &[],
    );
    let main_commit = repo.find_commit(main_commit_id).unwrap();

    // 2. Create branch A from main
    let a_commit_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "feature A content",
        "feat: add feature A",
        &[&main_commit],
    );
    let a_commit = repo.find_commit(a_commit_id).unwrap();

    // 3. Create branch B from A
    let b_commit_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "feature B content",
        "feat: add feature B",
        &[&a_commit],
    );

    // 4. Create branch C from A (fork sibling of B)
    make_commit(
        &repo,
        "refs/heads/feature-c",
        "c.txt",
        "feature C content",
        "feat: add feature C",
        &[&a_commit],
    );

    // Set HEAD to feature-b
    repo.set_head("refs/heads/feature-b").unwrap();
    let head_id = b_commit_id;
    let upstream_id = main_commit_id;
    let upstream_name = "main";

    // Get stack branches
    let merge_base_for_test = repo.merge_base(head_id, upstream_id).unwrap();
    let branches = gits::stack::get_stack_branches_from_merge_base(
        &repo,
        merge_base_for_test,
        head_id,
        upstream_id,
        upstream_name,
    )
    .unwrap();

    let branch_names: Vec<String> = branches.iter().map(|b| b.name.clone()).collect();
    println!("Stack branches: {:?}", branch_names);

    // Should only contain feature-a and feature-b
    assert!(branch_names.contains(&"feature-a".to_string()));
    assert!(branch_names.contains(&"feature-b".to_string()));
    assert!(
        !branch_names.contains(&"feature-c".to_string()),
        "Stack should not contain fork sibling 'feature-c'"
    );
}

// ============================================================================
// Test 4: Handle non-monotonic timestamps (regression test)
// ============================================================================

/// Scenario:
/// main -> A (HEAD, t=100) -> B (t=50, clock skew)
///
/// B is still a descendant of A and should be in the stack even if its timestamp
/// is earlier than A's.
#[test]
fn get_stack_branches_handles_non_monotonic_timestamps() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    // 1. Initial commit on main (t=10)
    let main_commit_id = make_commit_at(
        &repo,
        "refs/heads/main",
        "main.txt",
        "main content",
        "initial commit",
        &[],
        10,
    );
    let main_commit = repo.find_commit(main_commit_id).unwrap();

    // 2. Create branch A from main (t=100)
    let a_commit_id = make_commit_at(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "feature A content",
        "feat: add feature A",
        &[&main_commit],
        100,
    );
    let a_commit = repo.find_commit(a_commit_id).unwrap();

    // 3. Create branch B from A (t=50, simulating clock skew/non-monotonicity)
    let _b_commit_id = make_commit_at(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "feature B content",
        "feat: add feature B",
        &[&a_commit],
        50,
    );

    // Set HEAD to feature-a
    repo.set_head("refs/heads/feature-a").unwrap();
    let head_id = a_commit_id;
    let upstream_id = main_commit_id;
    let upstream_name = "main";

    // Get stack branches
    let merge_base_for_test = repo.merge_base(head_id, upstream_id).unwrap();
    let branches = gits::stack::get_stack_branches_from_merge_base(
        &repo,
        merge_base_for_test,
        head_id,
        upstream_id,
        upstream_name,
    )
    .unwrap();

    let branch_names: Vec<String> = branches.iter().map(|b| b.name.clone()).collect();
    println!("Stack branches: {:?}", branch_names);

    // Should contain both feature-a and feature-b
    assert!(
        branch_names.contains(&"feature-a".to_string()),
        "Stack should contain feature-a"
    );
    assert!(
        branch_names.contains(&"feature-b".to_string()),
        "Stack should contain feature-b despite earlier timestamp"
    );
}

// ============================================================================
// Test 5: Skewed timestamp + merged into upstream (regression test)
// ============================================================================

/// Scenario:
/// main -> M0 (t=10)
///          ├─ A (HEAD, t=100) ── C (t=30, skew)
///          └─ B (t=50, skew)  ← merged into main
///
/// A and C are in the stack. B is NOT because it's merged into main.
/// C has t=30 < HEAD t=100, so it hits the fallback.
#[test]
fn get_stack_branches_handles_skewed_merged_exclusion() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    // 1. Initial commit on main (t=10)
    let m0_id = make_commit_at(
        &repo,
        "refs/heads/main",
        "main.txt",
        "initial",
        "initial",
        &[],
        10,
    );
    let m0 = repo.find_commit(m0_id).unwrap();

    // 2. Create A from main (t=100)
    let a_id = make_commit_at(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feat: a",
        &[&m0],
        100,
    );
    let a = repo.find_commit(a_id).unwrap();

    // 3. Create C from A (t=30, skew)
    let _c_id = make_commit_at(
        &repo,
        "refs/heads/feature-c",
        "c.txt",
        "c",
        "feat: c",
        &[&a],
        30,
    );

    // 4. Create B from main (t=50, skew)
    let b_id = make_commit_at(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feat: b",
        &[&m0],
        50,
    );

    // 5. Merge B into main
    repo.set_head_detached(m0_id).unwrap();
    repo.branch("main", &repo.find_commit(b_id).unwrap(), true)
        .unwrap();
    repo.set_head("refs/heads/main").unwrap();

    // Set HEAD to feature-a
    repo.set_head("refs/heads/feature-a").unwrap();
    let head_id = a_id;
    let upstream_id = b_id; // main is now at B
    let upstream_name = "main";

    // Get stack branches
    let merge_base_for_test = repo.merge_base(head_id, upstream_id).unwrap();
    let branches = gits::stack::get_stack_branches_from_merge_base(
        &repo,
        merge_base_for_test,
        head_id,
        upstream_id,
        upstream_name,
    )
    .unwrap();

    let branch_names: Vec<String> = branches.iter().map(|b| b.name.clone()).collect();
    println!("Stack branches: {:?}", branch_names);

    // Should contain feature-a and feature-c
    assert!(
        branch_names.contains(&"feature-a".to_string()),
        "Stack should contain feature-a"
    );
    assert!(
        branch_names.contains(&"feature-c".to_string()),
        "Stack should contain feature-c despite skewed timestamp"
    );

    // Should NOT contain feature-b (it's merged into main)
    assert!(
        !branch_names.contains(&"feature-b".to_string()),
        "Merged branch feature-b should be excluded"
    );
}
