mod common;

use common::{kin_cmd, repo_init, run_ok};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Repo with `main` and a `feature-a` branch, HEAD on `main`.
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

#[test]
fn cd_prints_worktree_path_and_is_quiet_when_captured() {
    let dir = setup_repo();
    // A worktree must exist for the branch to cd to it.
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
        .args(["wt", "cd", "feature-a"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        fs::canonicalize(String::from_utf8_lossy(&output.stdout).trim()).unwrap(),
        fs::canonicalize(&temp_path).unwrap()
    );
    // stdout is captured (not a TTY) here, mimicking the shell wrapper, so the
    // "enable integration" hint must stay quiet.
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("shell integration"),
        "hint should not fire when stdout is captured"
    );
}

#[test]
fn cd_fails_for_branch_without_worktree() {
    let dir = setup_repo();
    kin_cmd()
        .args(["wt", "cd", "feature-a"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "No worktree found for branch 'feature-a'.",
        ));
}

#[test]
fn shell_init_emits_wrappers_and_works_outside_a_repo() {
    // shell-init is for rc files, so it must not require being inside a repo.
    let non_repo = TempDir::new().unwrap();

    for (shell, needles) in [
        (
            "bash",
            vec!["kin()", "command kin wt cd", "cd \"$__kin_target\""],
        ),
        ("zsh", vec!["kin()", "command kin wt cd"]),
        ("fish", vec!["function kin", "command kin wt cd", "end"]),
    ] {
        let output = kin_cmd()
            .args(["shell-init", shell])
            .current_dir(non_repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "shell-init {shell} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let script = String::from_utf8_lossy(&output.stdout);
        for needle in needles {
            assert!(
                script.contains(needle),
                "shell-init {shell} missing '{needle}':\n{script}"
            );
        }
    }
}

#[test]
fn shell_init_bundles_completions_by_default() {
    let output = kin_cmd().args(["shell-init", "zsh"]).output().unwrap();
    assert!(output.status.success());
    let script = String::from_utf8_lossy(&output.stdout);
    // Both the completion registration and the cd wrapper are present.
    assert!(
        script.contains("#compdef kin"),
        "missing completions:\n{script}"
    );
    assert!(
        script.contains("command kin wt cd"),
        "missing cd wrapper:\n{script}"
    );
}

#[test]
fn shell_init_no_completions_omits_them() {
    let output = kin_cmd()
        .args(["shell-init", "zsh", "--no-completions"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let script = String::from_utf8_lossy(&output.stdout);
    assert!(
        script.contains("command kin wt cd"),
        "wrapper should still be present:\n{script}"
    );
    assert!(
        !script.contains("#compdef kin"),
        "completions should be omitted:\n{script}"
    );
}

#[test]
fn shell_init_rejects_unsupported_shell() {
    kin_cmd()
        .args(["shell-init", "powershell"])
        .assert()
        .failure();
}

/// End-to-end: source the bash wrapper and confirm `kin wt cd` actually changes
/// the shell's directory.
#[cfg(unix)]
#[test]
fn bash_wrapper_changes_directory() {
    let dir = setup_repo();
    let wt_home = TempDir::new().unwrap();
    let wt_path = wt_home.path().join("feature-a");
    assert!(
        kin_cmd()
            .args([
                "wt",
                "add",
                "feature-a",
                "--path",
                wt_path.to_str().unwrap()
            ])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .status
            .success()
    );

    let bin = assert_cmd::cargo::cargo_bin!("kin").to_path_buf();
    let bin_dir = bin.parent().unwrap();
    let path_env = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let script = format!(
        "eval \"$(kin shell-init bash)\"\ncd '{}'\nkin wt cd feature-a\npwd\n",
        dir.path().display()
    );
    let output = std::process::Command::new("bash")
        .arg("-c")
        .arg(&script)
        .env("PATH", path_env)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "bash wrapper failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let final_dir = String::from_utf8_lossy(&output.stdout)
        .lines()
        .last()
        .unwrap()
        .to_string();
    assert_eq!(
        canonicalize(&final_dir),
        canonicalize(wt_path.to_str().unwrap()),
        "wrapper should cd into the worktree"
    );
}

#[cfg(unix)]
fn canonicalize(path: &str) -> std::path::PathBuf {
    fs::canonicalize(Path::new(path)).unwrap()
}
