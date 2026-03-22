mod common;

use common::{current_branch, kin_cmd, repo_init, run_ok, write_repo_config};
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
    dir
}

fn shell_write_command(path: &Path, text: &str) -> String {
    shell_write_command_for_platform(path, text, cfg!(windows))
}

fn failing_shell_write_command(path: &Path, text: &str) -> String {
    if cfg!(windows) {
        format!(
            "{} && exit /b 1",
            shell_write_command_for_platform(path, text, true)
        )
    } else {
        format!(
            "{}; exit 1",
            shell_write_command_for_platform(path, text, false)
        )
    }
}

fn shell_write_command_for_platform(path: &Path, text: &str, windows: bool) -> String {
    let path = path.display().to_string();
    if windows {
        let path = path.replace('\'', "''");
        let text = text.replace('\'', "''");
        format!(
            "powershell -NoProfile -Command \"Set-Content -NoNewline -LiteralPath '{}' -Value '{}'\"",
            path, text
        )
    } else {
        let path = path.replace('\'', "'\\''");
        let text = text.replace('\'', "'\\''");
        format!("printf '{}' > '{}'", text, path)
    }
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
fn worktree_main_uses_configured_trunk_branch() {
    let dir = setup_repo();
    let main_path = dir.path().join(".git/kindra-worktrees/main");

    run_ok("git", &["checkout", "-b", "trunk"], dir.path());
    fs::write(dir.path().join("trunk.txt"), "trunk").unwrap();
    run_ok("git", &["add", "trunk.txt"], dir.path());
    run_ok("git", &["commit", "-m", "trunk"], dir.path());
    run_ok("git", &["checkout", "main"], dir.path());
    write_repo_config(dir.path(), "[worktrees]\ntrunk = \"trunk\"\n");

    let output = kin_cmd()
        .args(["wt", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(current_branch(&main_path), "trunk");
}

#[test]
fn worktree_main_bootstraps_local_branch_from_remote_trunk() {
    let remote = TempDir::new().unwrap();
    run_ok("git", &["init", "--bare"], remote.path());

    let dir = TempDir::new().unwrap();
    run_ok(
        "git",
        &["clone", remote.path().to_str().unwrap(), "."],
        dir.path(),
    );
    let repo = git2::Repository::open(dir.path()).unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Test User").unwrap();
    config.set_str("user.email", "test@example.com").unwrap();

    run_ok("git", &["checkout", "-b", "main"], dir.path());
    fs::write(dir.path().join("file.txt"), "main").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok("git", &["commit", "-m", "initial"], dir.path());
    run_ok("git", &["push", "-u", "origin", "main"], dir.path());
    run_ok("git", &["checkout", "-b", "feature-a"], dir.path());
    run_ok(
        "git",
        &["branch", "--set-upstream-to=origin/main", "feature-a"],
        dir.path(),
    );
    run_ok("git", &["branch", "-D", "main"], dir.path());

    let output = kin_cmd()
        .args(["wt", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(
        current_branch(&dir.path().join(".git/kindra-worktrees/main")),
        "main"
    );
    assert!(
        String::from_utf8_lossy(
            &run_ok_output("git", &["branch", "--format=%(refname:short)"], dir.path()).stdout
        )
        .lines()
        .any(|line| line.trim() == "main")
    );
}

#[test]
fn worktree_hooks_run_for_create_checkout_and_remove() {
    let dir = setup_repo();
    let create_marker = dir.path().join("create-marker.txt");
    let checkout_marker = dir.path().join("checkout-marker.txt");
    let remove_marker = dir.path().join("remove-marker.txt");

    write_repo_config(
        dir.path(),
        &format!(
            "[worktrees.hooks]\non_create = [{}]\non_remove = [{}]\n\n[worktrees.review]\non_checkout = [{}]\n",
            toml_basic_string(&shell_write_command(&create_marker, "created")),
            toml_basic_string(&shell_write_command(&remove_marker, "removed")),
            toml_basic_string(&shell_write_command(&checkout_marker, "checked-out")),
        ),
    );

    run_ok("git", &["checkout", "-b", "feature-a"], dir.path());
    fs::write(dir.path().join("feature.txt"), "feature").unwrap();
    run_ok("git", &["add", "feature.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature"], dir.path());

    let output = kin_cmd()
        .args(["wt", "review"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(fs::read_to_string(&create_marker).unwrap(), "created");

    let output = kin_cmd()
        .args(["wt", "review", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(fs::read_to_string(&checkout_marker).unwrap(), "checked-out");

    let output = kin_cmd()
        .args(["wt", "remove", "review", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(fs::read_to_string(&remove_marker).unwrap(), "removed");
}

#[test]
fn shell_write_command_escapes_single_quotes_for_each_platform() {
    let path = Path::new("dir/it's/hook's.txt");
    let text = "hook's value";

    assert_eq!(
        shell_write_command_for_platform(path, text, false),
        "printf 'hook'\\''s value' > 'dir/it'\\''s/hook'\\''s.txt'"
    );
    assert_eq!(
        shell_write_command_for_platform(path, text, true),
        "powershell -NoProfile -Command \"Set-Content -NoNewline -LiteralPath 'dir/it''s/hook''s.txt' -Value 'hook''s value'\""
    );
}

#[test]
fn worktree_hooks_support_single_quotes_in_paths_and_text() {
    let dir = setup_repo();
    let create_marker = dir.path().join("create marker 'quoted'.txt");
    let checkout_marker = dir.path().join("checkout marker 'quoted'.txt");
    let remove_marker = dir.path().join("remove marker 'quoted'.txt");

    write_repo_config(
        dir.path(),
        &format!(
            "[worktrees.hooks]\non_create = [{}]\non_remove = [{}]\n\n[worktrees.review]\non_checkout = [{}]\n",
            toml_basic_string(&shell_write_command(&create_marker, "create's done")),
            toml_basic_string(&shell_write_command(&remove_marker, "remove's done")),
            toml_basic_string(&shell_write_command(&checkout_marker, "checkout's done")),
        ),
    );

    run_ok("git", &["checkout", "-b", "feature-b"], dir.path());
    fs::write(dir.path().join("feature.txt"), "feature").unwrap();
    run_ok("git", &["add", "feature.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature"], dir.path());

    let output = kin_cmd()
        .args(["wt", "review"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(fs::read_to_string(&create_marker).unwrap(), "create's done");

    let output = kin_cmd()
        .args(["wt", "review", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(&checkout_marker).unwrap(),
        "checkout's done"
    );

    let output = kin_cmd()
        .args(["wt", "remove", "review", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(fs::read_to_string(&remove_marker).unwrap(), "remove's done");
}

#[test]
fn failing_create_hook_rolls_back_created_worktree() {
    let dir = setup_repo();
    write_repo_config(dir.path(), "[worktrees.hooks]\non_create = [\"exit 1\"]\n");

    let output = kin_cmd()
        .args(["wt", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Worktree on_create hook failed"));
    assert!(!dir.path().join(".git/kindra-worktrees/main").exists());
    assert!(!dir.path().join(".git/kindra_worktrees.json").exists());
}

#[test]
fn failing_checkout_hook_restores_previous_review_branch() {
    let dir = setup_repo();
    write_repo_config(
        dir.path(),
        "[worktrees.review]\non_checkout = [\"exit 1\"]\n",
    );

    run_ok("git", &["checkout", "-b", "feature-a"], dir.path());
    fs::write(dir.path().join("feature.txt"), "feature").unwrap();
    run_ok("git", &["add", "feature.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature"], dir.path());

    let output = kin_cmd()
        .args(["wt", "review"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());

    let output = kin_cmd()
        .args(["wt", "review", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        current_branch(&dir.path().join(".git/kindra-worktrees/review")),
        "feature-a"
    );

    let list_output = kin_cmd()
        .args(["wt", "list"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(list_output.status.success());
    assert!(
        !String::from_utf8_lossy(&list_output.stdout).contains("stale-meta"),
        "unexpected stale metadata after failed checkout hook\nstdout:\n{}",
        String::from_utf8_lossy(&list_output.stdout),
    );
}

#[test]
fn failing_checkout_hook_that_dirties_tracked_files_still_restores_previous_branch() {
    let dir = setup_repo();
    write_repo_config(
        dir.path(),
        &format!(
            "[worktrees.review]\non_checkout = [{}]\n",
            toml_basic_string(&failing_shell_write_command(Path::new("file.txt"), "dirty")),
        ),
    );

    run_ok("git", &["checkout", "-b", "feature-a"], dir.path());
    fs::write(dir.path().join("file.txt"), "feature").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature"], dir.path());

    let output = kin_cmd()
        .args(["wt", "review"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());

    let output = kin_cmd()
        .args(["wt", "review", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let review_path = dir.path().join(".git/kindra-worktrees/review");
    assert_eq!(current_branch(&review_path), "feature-a");
    assert_eq!(
        fs::read_to_string(review_path.join("file.txt")).unwrap(),
        "feature"
    );
}

#[test]
fn review_clean_before_switch_false_skips_forced_cleaning() {
    let dir = setup_repo();
    write_repo_config(
        dir.path(),
        "[worktrees.review]\nclean_before_switch = false\n",
    );

    run_ok("git", &["checkout", "-b", "feature-a"], dir.path());
    fs::write(dir.path().join("feature.txt"), "feature").unwrap();
    run_ok("git", &["add", "feature.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature"], dir.path());

    let output = kin_cmd()
        .args(["wt", "review"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());

    let review_path = dir.path().join(".git/kindra-worktrees/review");
    fs::write(review_path.join("local.txt"), "keep me").unwrap();

    let output = kin_cmd()
        .args(["wt", "review", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(current_branch(&review_path), "main");
    assert_eq!(
        fs::read_to_string(review_path.join("local.txt")).unwrap(),
        "keep me"
    );
}

fn run_ok_output(program: &str, args: &[&str], cwd: &std::path::Path) -> std::process::Output {
    let output = std::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Run Ok User")
        .env("GIT_AUTHOR_EMAIL", "run-ok@example.com")
        .env("GIT_COMMITTER_NAME", "Run Ok User")
        .env("GIT_COMMITTER_EMAIL", "run-ok@example.com")
        .output()
        .expect("failed to execute command");
    assert!(
        output.status.success(),
        "Command failed: {} {:?}\nstdout:\n{}\nstderr:\n{}",
        program,
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}
