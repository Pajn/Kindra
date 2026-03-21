mod common;
use common::{kin_cmd, make_commit, repo_init};
use git2::Repository;
use regex::Regex;
use tempfile::tempdir;

fn setup_simple_stack() -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    // Setup git config
    common::run_ok("git", &["config", "user.name", "Test User"], dir.path());
    common::run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        dir.path(),
    );

    // Initial commit on main
    let main_commit_id = make_commit(
        &repo,
        "refs/heads/main",
        "root.txt",
        "root content",
        "initial commit",
        &[],
    );

    // feature-a on main (get commit ID before creating branch commit)
    let feature_a_id = {
        let main_commit = repo.find_commit(main_commit_id).unwrap();
        make_commit(
            &repo,
            "refs/heads/feature-a",
            "a.txt",
            "a content",
            "add feature a",
            &[&main_commit],
        )
    };

    // feature-b on feature-a
    {
        let feature_a_commit = repo.find_commit(feature_a_id).unwrap();
        let _feature_b_id = make_commit(
            &repo,
            "refs/heads/feature-b",
            "b.txt",
            "b content",
            "add feature b",
            &[&feature_a_commit],
        );
    }

    // Checkout main
    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    (dir, repo)
}

fn setup_fork_stack() -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    // Setup git config
    common::run_ok("git", &["config", "user.name", "Test User"], dir.path());
    common::run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        dir.path(),
    );

    // Initial commit on main
    let main_commit_id = make_commit(
        &repo,
        "refs/heads/main",
        "root.txt",
        "root content",
        "initial commit",
        &[],
    );

    // feature-a on main
    let feature_a_id = {
        let main_commit = repo.find_commit(main_commit_id).unwrap();
        make_commit(
            &repo,
            "refs/heads/feature-a",
            "a.txt",
            "a content",
            "add feature a",
            &[&main_commit],
        )
    };

    // feature-b on feature-a
    // feature-c on feature-a (fork from same parent as feature-b)
    {
        let feature_a_commit = repo.find_commit(feature_a_id).unwrap();
        let _feature_b_id = make_commit(
            &repo,
            "refs/heads/feature-b",
            "b.txt",
            "b content",
            "add feature b",
            &[&feature_a_commit],
        );
        let _feature_c_id = make_commit(
            &repo,
            "refs/heads/feature-c",
            "c.txt",
            "c content",
            "add feature c",
            &[&feature_a_commit],
        );
    }

    // Checkout main
    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    (dir, repo)
}

/// Helper to run kin tree command and get output
fn run_tree_command(dir: &std::path::Path, args: &[&str]) -> String {
    let mut cmd = kin_cmd();
    cmd.arg("tree");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.current_dir(dir);

    let output = cmd.output().expect("Failed to execute kin tree");
    assert!(
        output.status.success(),
        "kin tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Test basic tree output for a simple stack: main -> feature-a -> feature-b
#[test]
fn test_tree_simple_stack() {
    let (dir, _repo) = setup_simple_stack();

    let output_str = run_tree_command(dir.path(), &[]);

    // Should show main at the root
    assert!(
        output_str.contains("main"),
        "Output should contain 'main':\n{}",
        output_str
    );

    // Should show feature-a
    assert!(
        output_str.contains("feature-a"),
        "Output should contain 'feature-a':\n{}",
        output_str
    );

    // Should show feature-b
    assert!(
        output_str.contains("feature-b"),
        "Output should contain 'feature-b':\n{}",
        output_str
    );
}

/// Test tree output shows proper hierarchical structure with box-drawing characters
#[test]
fn test_tree_shows_proper_structure() {
    let (dir, _repo) = setup_simple_stack();

    let output_str = run_tree_command(dir.path(), &[]);

    // Should use box-drawing characters for tree structure
    // Either └─ or ├─ should be present
    assert!(
        output_str.contains("└─") || output_str.contains("├─"),
        "Output should contain box-drawing characters for tree structure:\n{}",
        output_str
    );
}

/// Test tree --commits shows commit hashes and messages
#[test]
fn test_tree_with_commits_flag() {
    let (dir, _repo) = setup_simple_stack();

    let output_str = run_tree_command(dir.path(), &["--commits"]);

    // Should contain commit messages
    assert!(
        output_str.contains("add feature"),
        "Output should contain commit messages with --commits:\n{}",
        output_str
    );
}

/// Test tree --verbose shows all information
#[test]
fn test_tree_verbose_flag() {
    let (dir, _repo) = setup_simple_stack();

    let output_str = run_tree_command(dir.path(), &["--verbose"]);

    assert!(
        output_str.contains("main")
            && output_str.contains("feature-a")
            && output_str.contains("feature-b"),
        "Verbose output should contain all branch names:\n{}",
        output_str
    );
}

/// Test tree with fork (multiple branches from same parent)
#[test]
fn test_tree_fork_branches() {
    let (dir, _repo) = setup_fork_stack();

    let output_str = run_tree_command(dir.path(), &[]);

    // All branches should be present
    assert!(
        output_str.contains("main"),
        "Output should contain 'main':\n{}",
        output_str
    );
    assert!(
        output_str.contains("feature-a"),
        "Output should contain 'feature-a':\n{}",
        output_str
    );
    assert!(
        output_str.contains("feature-b"),
        "Output should contain 'feature-b':\n{}",
        output_str
    );
    assert!(
        output_str.contains("feature-c"),
        "Output should contain 'feature-c':\n{}",
        output_str
    );

    // Should use box-drawing characters
    assert!(
        output_str.contains("└─") || output_str.contains("├─"),
        "Output should contain box-drawing characters:\n{}",
        output_str
    );
}

/// Test tree on repo with only main branch (empty stack from perspective of tree)
#[test]
fn test_tree_empty_stack_shows_message() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    // Setup git config
    common::run_ok("git", &["config", "user.name", "Test User"], dir.path());
    common::run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        dir.path(),
    );

    // Just one commit on main, no other branches
    let _main_commit_id = make_commit(
        &repo,
        "refs/heads/main",
        "root.txt",
        "root content",
        "initial commit",
        &[],
    );

    let output_str = run_tree_command(dir.path(), &[]);

    // Empty stack should show a message
    assert!(
        output_str.contains("empty stack") || output_str.contains("main"),
        "Empty stack should show 'empty stack' message or 'main':\n{}",
        output_str
    );
}

/// Test tree with --upstream flag
#[test]
fn test_tree_with_upstream_flag() {
    let (dir, _repo) = setup_simple_stack();

    // Should show the stack starting from main
    let output_str = run_tree_command(dir.path(), &["--upstream", "main"]);

    assert!(
        output_str.contains("main") && output_str.contains("feature-a"),
        "Output should contain branches:\n{}",
        output_str
    );
}

/// Test tree with --remote flag (should succeed even without remote)
#[test]
fn test_tree_remote_flag() {
    let (dir, _repo) = setup_simple_stack();

    // Command should succeed even without a remote configured
    let output_str = run_tree_command(dir.path(), &["--remote"]);

    assert!(
        output_str.contains("feature-a") || output_str.contains("feature-b"),
        "Output should contain branch names:\n{}",
        output_str
    );
}

#[test]
fn test_tree_root_branch_syncs_against_upstream() {
    let (dir, _repo) = setup_simple_stack();

    common::run_ok(
        "git",
        &["branch", "--set-upstream-to=main", "feature-a"],
        dir.path(),
    );

    let output_str = run_tree_command(dir.path(), &["--remote"]);

    assert!(
        output_str.contains("feature-a") && output_str.contains("[In Sync]"),
        "Root branch should report in sync against its upstream:\n{}",
        output_str
    );
}

/// Test tree with --pr flag (should succeed even without GitHub CLI)
#[test]
fn test_tree_pr_flag() {
    let (dir, _repo) = setup_simple_stack();

    // Command should succeed even without gh CLI
    let output_str = run_tree_command(dir.path(), &["--pr"]);

    assert!(
        output_str.contains("feature-a") || output_str.contains("feature-b"),
        "Output should contain branch names:\n{}",
        output_str
    );
}

/// Test tree output on detached HEAD
#[test]
fn test_tree_detached_head() {
    let (dir, repo) = setup_simple_stack();

    // Get the commit that feature-a points to
    let feature_a_ref = repo.find_reference("refs/heads/feature-a").unwrap();
    let feature_a_commit = feature_a_ref.peel_to_commit().unwrap();

    // Checkout in detached HEAD state
    repo.set_head_detached(feature_a_commit.id()).unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Should still show the stack
    let output_str = run_tree_command(dir.path(), &[]);

    assert!(
        output_str.contains("feature-a"),
        "Output should contain 'feature-a' even in detached HEAD state:\n{}",
        output_str
    );
}

/// Test that tree command doesn't panic on complex fork topology
#[test]
fn test_tree_complex_fork_topology() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    // Setup git config
    common::run_ok("git", &["config", "user.name", "Test User"], dir.path());
    common::run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        dir.path(),
    );

    // Create a more complex fork topology:
    // main -> a -> b
    //          \-> c
    //          \-> d
    //               \-> e

    let main_id = make_commit(&repo, "refs/heads/main", "m.txt", "m", "main", &[]);

    let a_id = {
        let main = repo.find_commit(main_id).unwrap();
        make_commit(&repo, "refs/heads/a", "a.txt", "a", "add a", &[&main])
    };

    {
        let a = repo.find_commit(a_id).unwrap();
        let _b_id = make_commit(&repo, "refs/heads/b", "b.txt", "b", "add b", &[&a]);
        let _c_id = make_commit(&repo, "refs/heads/c", "c.txt", "c", "add c", &[&a]);
        let _d_id = make_commit(&repo, "refs/heads/d", "d.txt", "d", "add d", &[&a]);
        let d = repo.find_commit(_d_id).unwrap();
        let _e_id = make_commit(&repo, "refs/heads/e", "e.txt", "e", "add e", &[&d]);
    }

    // Checkout main
    repo.set_head("refs/heads/main").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Run tree - should not panic
    let output_str = run_tree_command(dir.path(), &[]);

    // All branches should be present
    assert!(output_str.contains("main"), "Should contain main");
    assert!(
        Regex::new(r"\ba\b").unwrap().is_match(&output_str),
        "Should contain branch 'a'"
    );
    assert!(
        Regex::new(r"\bb\b").unwrap().is_match(&output_str),
        "Should contain branch 'b'"
    );
    assert!(
        Regex::new(r"\bc\b").unwrap().is_match(&output_str),
        "Should contain branch 'c'"
    );
    assert!(
        Regex::new(r"\bd\b").unwrap().is_match(&output_str),
        "Should contain branch 'd'"
    );
    assert!(
        Regex::new(r"\be\b").unwrap().is_match(&output_str),
        "Should contain branch 'e'"
    );
}
