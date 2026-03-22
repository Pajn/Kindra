mod common;

use common::{kin_cmd, managed_worktree_path, read_worktree_metadata, repo_init, run_ok};
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

    run_ok("git", &["checkout", "-b", "feature-a"], dir.path());
    fs::write(dir.path().join("feature.txt"), "feature").unwrap();
    run_ok("git", &["add", "feature.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature"], dir.path());
    run_ok("git", &["checkout", "main"], dir.path());

    dir
}

fn worktree_git_dir(worktree_path: &Path) -> std::path::PathBuf {
    let dot_git = worktree_path.join(".git");
    if dot_git.is_dir() {
        return dot_git;
    }

    let raw = fs::read_to_string(&dot_git).unwrap();
    let gitdir = raw
        .strip_prefix("gitdir: ")
        .expect("worktree .git file should start with gitdir:")
        .trim();
    let gitdir_path = std::path::PathBuf::from(gitdir);
    if gitdir_path.is_absolute() {
        gitdir_path
    } else {
        worktree_path.join(gitdir_path)
    }
}

#[test]
fn worktree_remove_prompts_by_default_and_removes_with_yes() {
    let dir = setup_repo();
    let temp_path = dir.path().join(".git/kindra-worktrees/temp/feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );

    let output = kin_cmd()
        .args(["wt", "remove", "feature-a"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("auto-denying"));
    assert!(temp_path.exists());

    let output = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!temp_path.exists());
    let metadata = read_worktree_metadata(dir.path());
    assert!(
        !metadata["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["branch"] == "feature-a")
    );
}

#[test]
fn worktree_cleanup_removes_merged_temp_worktrees_but_not_persistent_ones() {
    let dir = setup_repo();
    let main_path = dir.path().join(".git/kindra-worktrees/main");
    let temp_path = dir.path().join(".git/kindra-worktrees/temp/feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "main"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );

    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());

    let output = kin_cmd()
        .args(["wt", "cleanup", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(main_path.exists());
    assert!(!temp_path.exists());
    let metadata = read_worktree_metadata(dir.path());
    assert!(
        !metadata["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["role"] == "temp" && record["branch"] == "feature-a")
    );
}

#[test]
fn worktree_cleanup_can_prune_stale_temp_metadata() {
    let dir = setup_repo();
    let temp_path = dir.path().join(".git/kindra-worktrees/temp/feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    fs::remove_dir_all(&temp_path).unwrap();
    run_ok("git", &["worktree", "prune"], dir.path());

    let output = kin_cmd()
        .args(["wt", "cleanup", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let metadata = read_worktree_metadata(dir.path());
    assert!(
        !metadata["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["branch"] == "feature-a")
    );
}

#[test]
fn worktree_remove_requires_force_for_dirty_worktrees_even_with_yes() {
    let dir = setup_repo();
    let temp_path = managed_worktree_path(dir.path(), "temp/feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    fs::write(temp_path.join("dirty.txt"), "dirty").unwrap();

    let output = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Re-run with --force to remove it."));
    assert!(temp_path.exists());

    let output = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes", "--force"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!temp_path.exists());
}

#[test]
fn worktree_remove_requires_force_for_incomplete_git_operations() {
    let dir = setup_repo();
    let temp_path = managed_worktree_path(dir.path(), "temp/feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );

    fs::write(dir.path().join("file.txt"), "main change").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok("git", &["commit", "-m", "main change"], dir.path());

    fs::write(temp_path.join("file.txt"), "feature change").unwrap();
    run_ok("git", &["add", "file.txt"], &temp_path);
    run_ok("git", &["commit", "-m", "feature change"], &temp_path);

    let merge_output = std::process::Command::new("git")
        .args(["merge", "main"])
        .current_dir(&temp_path)
        .output()
        .unwrap();
    assert!(
        !merge_output.status.success(),
        "expected merge conflict\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&merge_output.stdout),
        String::from_utf8_lossy(&merge_output.stderr),
    );
    assert!(worktree_git_dir(&temp_path).join("MERGE_HEAD").exists());

    let output = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Re-run with --force to remove it."));
    assert!(temp_path.exists());

    let output = kin_cmd()
        .args(["wt", "remove", "feature-a", "--yes", "--force"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!temp_path.exists());
}

#[test]
fn worktree_cleanup_yes_skips_dirty_candidates_without_force() {
    let dir = setup_repo();
    let temp_path = managed_worktree_path(dir.path(), "temp/feature-a");

    assert!(
        kin_cmd()
            .args(["wt", "temp", "feature-a"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );

    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());
    fs::write(temp_path.join("dirty.txt"), "dirty").unwrap();

    let output = kin_cmd()
        .args(["wt", "cleanup", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Skipping dirty temp worktree 'feature-a'"));
    assert!(stdout.contains("found 1 temp worktree candidate(s), removed 0, skipped 1"));
    assert!(temp_path.exists());

    let output = kin_cmd()
        .args(["wt", "cleanup", "--yes", "--force"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!temp_path.exists());
}

#[test]
fn worktree_remove_rejects_missing_targets_even_with_yes() {
    let dir = setup_repo();

    for (target, expected) in [
        ("main", "No managed main worktree exists."),
        ("review", "No managed review worktree exists."),
        (
            "feature-a",
            "No managed temp worktree exists for branch 'feature-a'.",
        ),
    ] {
        let output = kin_cmd()
            .args(["wt", "remove", target, "--yes"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(!output.status.success(), "unexpected success for {target}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
    }
}
