mod common;

use common::{
    canonical_output_path, current_branch, kin_cmd, read_worktree_metadata, repo_init, run_ok,
    write_repo_config,
};
use git2::{BranchType, Repository};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn setup_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    let repo = repo_init(dir.path());
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Test User").unwrap();
    config.set_str("user.email", "test@example.com").unwrap();

    fs::write(dir.path().join("file.txt"), "main").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok("git", &["commit", "-m", "initial"], dir.path());

    run_ok("git", &["checkout", "-b", "feature/auth"], dir.path());
    fs::write(dir.path().join("feature.txt"), "feature-auth").unwrap();
    run_ok("git", &["add", "feature.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature-auth"], dir.path());

    run_ok("git", &["checkout", "main"], dir.path());
    run_ok("git", &["checkout", "-b", "feature-auth"], dir.path());
    fs::write(dir.path().join("dash.txt"), "feature-auth").unwrap();
    run_ok("git", &["add", "dash.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature-auth-dash"], dir.path());
    run_ok("git", &["checkout", "feature/auth"], dir.path());

    dir
}

fn rev_parse(cwd: &Path, target: &str) -> String {
    let output = std::process::Command::new("git")
        .args(["rev-parse", target])
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git rev-parse {target} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn branch_upstream(cwd: &Path, branch: &str) -> String {
    let upstream = format!("{branch}@{{upstream}}");
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", &upstream])
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git rev-parse --abbrev-ref {upstream} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn toml_basic_string(value: &str) -> String {
    let mut escaped = value.replace('\\', "\\\\");
    escaped = escaped.replace('\n', "\\n");
    escaped = escaped.replace('\r', "\\r");
    escaped = escaped.replace('\t', "\\t");
    let mut rendered = String::with_capacity(escaped.len());
    for ch in escaped.chars() {
        match ch {
            '\u{08}' => rendered.push_str("\\b"),
            '\u{0C}' => rendered.push_str("\\f"),
            '"' => rendered.push_str("\\\""),
            ch if ch.is_control() => rendered.push_str(&format!("\\u{:04X}", ch as u32)),
            ch => rendered.push(ch),
        }
    }
    format!("\"{rendered}\"")
}

#[test]
fn worktree_temp_sanitizes_branch_names_and_reuses_existing_worktree() {
    let dir = setup_repo();
    let expected_path = dir.path().join(".git/kindra-worktrees/temp/feature-auth");

    let output = kin_cmd()
        .args(["wt", "temp"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        canonical_output_path(&output.stdout, dir.path()),
        fs::canonicalize(&expected_path).unwrap()
    );
    assert!(expected_path.exists());

    let output = kin_cmd()
        .args(["wt", "temp"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        canonical_output_path(&output.stdout, dir.path()),
        fs::canonicalize(&expected_path).unwrap()
    );

    let metadata = read_worktree_metadata(dir.path());
    assert!(
        metadata["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["role"] == "temp" && record["branch"] == "feature/auth")
    );
}

#[test]
fn worktree_temp_detects_sanitized_path_collisions() {
    let dir = setup_repo();

    let output = kin_cmd()
        .args(["wt", "temp", "feature/auth"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());

    let output = kin_cmd()
        .args(["wt", "temp", "feature-auth"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("already reserved for temp 'feature/auth'")
    );
}

#[test]
fn worktree_temp_rejects_paths_reserved_for_managed_non_temp_roles() {
    let dir = setup_repo();
    write_repo_config(
        dir.path(),
        "[worktrees.temp]\npath_template = \".git/kindra-worktrees/{branch}\"\n",
    );

    let output = kin_cmd()
        .args(["wt", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "kin wt main failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let output = kin_cmd()
        .args(["wt", "temp", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already reserved for main 'main'"));
}

#[test]
fn worktree_temp_b_creates_new_branch_from_current_branch() {
    let dir = setup_repo();
    let expected_path = dir.path().join(".git/kindra-worktrees/temp/feature-spike");
    let expected_oid = rev_parse(dir.path(), "feature/auth");

    let output = kin_cmd()
        .args(["wt", "temp", "-b", "feature/spike"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "kin wt temp -b feature/spike failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        canonical_output_path(&output.stdout, dir.path()),
        fs::canonicalize(&expected_path).unwrap()
    );
    assert_eq!(current_branch(&expected_path), "feature/spike");
    assert_eq!(rev_parse(dir.path(), "feature/spike"), expected_oid);

    let metadata = read_worktree_metadata(dir.path());
    assert!(
        metadata["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["role"] == "temp" && record["branch"] == "feature/spike")
    );
}

#[test]
fn worktree_temp_b_creates_new_branch_from_explicit_remote_start_point() {
    let dir = setup_repo();
    let remote = TempDir::new().unwrap();
    let remote_path = remote.path().to_str().unwrap();
    let expected_path = dir.path().join(".git/kindra-worktrees/temp/hotfix-main");

    run_ok("git", &["init", "--bare"], remote.path());
    run_ok("git", &["remote", "add", "origin", remote_path], dir.path());
    run_ok("git", &["push", "origin", "main"], dir.path());
    run_ok("git", &["fetch", "origin"], dir.path());

    let output = kin_cmd()
        .args(["wt", "temp", "-b", "hotfix/main", "origin/main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "kin wt temp -b hotfix/main origin/main failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        canonical_output_path(&output.stdout, dir.path()),
        fs::canonicalize(&expected_path).unwrap()
    );
    assert_eq!(current_branch(&expected_path), "hotfix/main");
    assert_eq!(
        rev_parse(dir.path(), "hotfix/main"),
        rev_parse(dir.path(), "origin/main")
    );
    assert_eq!(branch_upstream(dir.path(), "hotfix/main"), "origin/main");
}

#[test]
fn worktree_temp_b_rejects_existing_branch_names() {
    let dir = setup_repo();

    let output = kin_cmd()
        .args(["wt", "temp", "-b", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("A local branch named 'main' already exists.")
    );
}

#[test]
fn worktree_temp_b_failing_create_hook_rolls_back_created_branch() {
    let dir = setup_repo();
    let expected_path = dir.path().join(".git/kindra-worktrees/temp/feature-spike");
    write_repo_config(dir.path(), "[worktrees.temp]\non_create = [\"exit 1\"]\n");

    let output = kin_cmd()
        .args(["wt", "temp", "-b", "feature/spike"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "kin wt temp -b feature/spike unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("Worktree on_create hook failed"));
    assert!(!expected_path.exists());
    assert!(
        Repository::open(dir.path())
            .unwrap()
            .find_branch("feature/spike", BranchType::Local)
            .is_err(),
        "failed temp branch should be removed after hook failure"
    );
    assert!(!dir.path().join(".git/kindra_worktrees.json").exists());
}

#[test]
fn worktree_temp_b_failing_create_hook_rolls_back_branch_from_divergent_local_start_point() {
    let dir = setup_repo();
    let expected_path = dir.path().join(".git/kindra-worktrees/temp/feature-spike");
    write_repo_config(dir.path(), "[worktrees.temp]\non_create = [\"exit 1\"]\n");

    let output = kin_cmd()
        .args(["wt", "temp", "-b", "feature/spike", "feature-auth"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "kin wt temp -b feature/spike feature-auth unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("Worktree on_create hook failed"));
    assert!(!expected_path.exists());
    assert!(
        Repository::open(dir.path())
            .unwrap()
            .find_branch("feature/spike", BranchType::Local)
            .is_err(),
        "failed temp branch from a divergent local start-point should be removed after hook failure"
    );
    assert!(!dir.path().join(".git/kindra_worktrees.json").exists());
}

#[test]
fn worktree_temp_b_failing_create_hook_after_commit_keeps_branch_for_manual_cleanup() {
    let dir = setup_repo();
    let expected_path = dir.path().join(".git/kindra-worktrees/temp/feature-spike");
    let saved_tip = rev_parse(dir.path(), "feature/auth");
    let hook = if cfg!(windows) {
        "echo hook>hook-commit.txt && git add hook-commit.txt && git commit -m hook-temp-commit && exit /b 1"
    } else {
        "printf hook > hook-commit.txt && git add hook-commit.txt && git commit -m hook-temp-commit && exit 1"
    };
    write_repo_config(
        dir.path(),
        &format!(
            "[worktrees.temp]\non_create = [{}]\n",
            toml_basic_string(hook)
        ),
    );

    let output = kin_cmd()
        .args(["wt", "temp", "-b", "feature/spike"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "kin wt temp -b feature/spike unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(!expected_path.exists());

    let repo = Repository::open(dir.path()).unwrap();
    let current_tip = repo
        .find_branch("feature/spike", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap()
        .to_string();
    assert_ne!(current_tip, saved_tip);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Worktree on_create hook failed"));
    assert!(stderr.contains("manual cleanup"));
    assert!(stderr.contains(&saved_tip));
    assert!(stderr.contains(&current_tip));
    assert!(
        stderr.contains("Left branch 'feature/spike' in place"),
        "expected manual cleanup guidance in stderr\nstderr:\n{stderr}"
    );
    assert!(!dir.path().join(".git/kindra_worktrees.json").exists());
}
