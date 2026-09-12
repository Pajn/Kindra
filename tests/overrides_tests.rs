#![cfg(unix)]
mod common;
use common::{git_command, kin_cmd, repo_init, run_ok};
use std::{fs, path::Path};
use tempfile::{TempDir, tempdir};

fn git(path: &Path, args: &[&str]) -> String {
    let out = git_command(path).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
fn commit(path: &Path, name: &str, content: &str) {
    fs::write(path.join(name), content).unwrap();
    run_ok("git", &["add", "--", name], path);
    run_ok("git", &["commit", "-m", content], path);
}
fn setup() -> TempDir {
    let dir = tempdir().unwrap();
    repo_init(dir.path());
    commit(dir.path(), "AGENTS.md", "base\n");
    run_ok("git", &["checkout", "-b", "feature"], dir.path());
    commit(dir.path(), "AGENTS.md", "feature\n");
    fs::write(
        dir.path().join(".git/apply.sh"),
        "printf 'local override\\n' > AGENTS.md\n",
    )
    .unwrap();
    fs::write(dir.path().join(".git/kindra.toml"), "[overrides]\npaths = ['AGENTS.md']\napply = ['sh \"$(git rev-parse --git-common-dir)/apply.sh\"']\n").unwrap();
    fs::write(dir.path().join("AGENTS.md"), "local override\n").unwrap();
    run_ok(
        "git",
        &["update-index", "--skip-worktree", "AGENTS.md"],
        dir.path(),
    );
    dir
}
fn assert_applied(path: &Path) {
    assert_eq!(
        fs::read_to_string(path.join("AGENTS.md")).unwrap(),
        "local override\n"
    );
    assert!(git(path, &["ls-files", "-v", "AGENTS.md"]).starts_with("S "));
    assert!(
        !git2::Repository::open(path)
            .unwrap()
            .path()
            .join("kindra_overrides_state.json")
            .exists()
    );
}
#[test]
fn checkout_suspends_overrides_and_preserves_other_staged_changes() {
    let dir = setup();
    fs::write(dir.path().join("staged.txt"), "keep staged").unwrap();
    run_ok("git", &["add", "staged.txt"], dir.path());
    kin_cmd()
        .current_dir(dir.path())
        .args(["checkout", "down"])
        .assert()
        .success();
    assert_eq!(
        git(dir.path(), &["branch", "--show-current"]).trim(),
        "main"
    );
    assert_eq!(
        git(dir.path(), &["diff", "--cached", "--name-only"]).trim(),
        "staged.txt"
    );
    assert_applied(dir.path());
}
#[test]
fn failed_operation_reapplies_overrides() {
    let dir = setup();
    kin_cmd()
        .current_dir(dir.path())
        .args(["move", "--onto", "nonexistent"])
        .assert()
        .failure();
    assert_applied(dir.path());
}
#[test]
fn staged_managed_file_is_not_overwritten() {
    let dir = setup();
    run_ok(
        "git",
        &["update-index", "--no-skip-worktree", "AGENTS.md"],
        dir.path(),
    );
    fs::write(dir.path().join("AGENTS.md"), "intentional staged change").unwrap();
    run_ok("git", &["add", "AGENTS.md"], dir.path());
    kin_cmd()
        .current_dir(dir.path())
        .args(["checkout", "down"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("staged"));
    assert_eq!(
        git(dir.path(), &["show", ":AGENTS.md"]),
        "intentional staged change"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
        "intentional staged change"
    );
}
fn conflict_setup() -> TempDir {
    let dir = setup();
    run_ok(
        "git",
        &["update-index", "--no-skip-worktree", "AGENTS.md"],
        dir.path(),
    );
    run_ok("git", &["restore", "AGENTS.md"], dir.path());
    run_ok("git", &["checkout", "-b", "target", "main"], dir.path());
    commit(dir.path(), "AGENTS.md", "target\n");
    run_ok("git", &["checkout", "feature"], dir.path());
    fs::write(dir.path().join("AGENTS.md"), "local override\n").unwrap();
    run_ok(
        "git",
        &["update-index", "--skip-worktree", "AGENTS.md"],
        dir.path(),
    );
    dir
}
#[test]
fn rebase_conflict_keeps_overrides_suspended_until_continue() {
    let dir = conflict_setup();
    kin_cmd()
        .current_dir(dir.path())
        .args(["move", "--onto", "target"])
        .assert()
        .failure();
    assert!(
        fs::read_to_string(dir.path().join("AGENTS.md"))
            .unwrap()
            .contains("<<<<<<<")
    );
    assert!(dir.path().join(".git/kindra_overrides_state.json").exists());
    kin_cmd()
        .current_dir(dir.path())
        .arg("split")
        .assert()
        .failure();
    assert!(
        fs::read_to_string(dir.path().join("AGENTS.md"))
            .unwrap()
            .contains("<<<<<<<")
    );
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "apply"])
        .assert()
        .failure();
    fs::write(dir.path().join("AGENTS.md"), "resolved\n").unwrap();
    run_ok("git", &["add", "AGENTS.md"], dir.path());
    kin_cmd()
        .current_dir(dir.path())
        .arg("continue")
        .assert()
        .success();
    assert_eq!(git(dir.path(), &["show", "HEAD:AGENTS.md"]), "resolved\n");
    assert_applied(dir.path());
}
#[test]
fn rebase_conflict_abort_reapplies_overrides() {
    let dir = conflict_setup();
    kin_cmd()
        .current_dir(dir.path())
        .args(["move", "--onto", "target"])
        .assert()
        .failure();
    kin_cmd()
        .current_dir(dir.path())
        .arg("abort")
        .assert()
        .success();
    assert_eq!(git(dir.path(), &["show", "HEAD:AGENTS.md"]), "feature\n");
    assert_applied(dir.path());
}
#[test]
fn failed_apply_is_persisted_and_continue_retries() {
    let dir = setup();
    fs::write(dir.path().join(".git/apply.sh"), "exit 42\n").unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .args(["checkout", "down"])
        .assert()
        .failure();
    assert!(dir.path().join(".git/kindra_overrides_state.json").exists());
    assert!(!git(dir.path(), &["ls-files", "-v", "AGENTS.md"]).starts_with("S "));
    fs::write(
        dir.path().join(".git/apply.sh"),
        "printf 'local override\\n' > AGENTS.md\n",
    )
    .unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .arg("continue")
        .assert()
        .success();
    assert_applied(dir.path());
}
#[test]
fn apply_bootstraps_overrides_without_existing_flags() {
    let dir = setup();
    run_ok(
        "git",
        &["update-index", "--no-skip-worktree", "AGENTS.md"],
        dir.path(),
    );
    run_ok("git", &["restore", "AGENTS.md"], dir.path());
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "apply"])
        .assert()
        .success();
    assert_applied(dir.path());
}

#[test]
fn creation_and_review_switch_apply_overrides_in_the_target_worktree() {
    let dir = setup();
    kin_cmd()
        .current_dir(dir.path())
        .args(["wt", "review", "main"])
        .assert()
        .success();
    let review = dir.path().join(".git/kindra-worktrees/review");
    assert_applied(&review);
    kin_cmd()
        .current_dir(dir.path())
        .args(["wt", "review", "feature"])
        .assert()
        .success();
    assert_eq!(git(&review, &["show", "HEAD:AGENTS.md"]), "feature\n");
    assert_applied(&review);
    assert_applied(dir.path());
}

#[test]
fn unmatched_skip_worktree_files_are_left_alone() {
    let dir = setup();
    commit(dir.path(), "untouched.txt", "tracked");
    fs::write(dir.path().join("untouched.txt"), "unmanaged local").unwrap();
    run_ok(
        "git",
        &["update-index", "--skip-worktree", "untouched.txt"],
        dir.path(),
    );
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "apply"])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(dir.path().join("untouched.txt")).unwrap(),
        "unmanaged local"
    );
    assert!(git(dir.path(), &["ls-files", "-v", "untouched.txt"]).starts_with("S "));
}

#[test]
fn symlinks_deletions_and_paths_with_spaces_and_newlines() {
    let dir = setup();
    for name in ["CLAUDE.md", "removed file", "line\nbreak"] {
        commit(dir.path(), name, "tracked");
    }
    fs::write(dir.path().join(".git/kindra.toml"), "[overrides]\npaths = ['AGENTS.md', 'CLAUDE.md', 'removed file', \"line\\nbreak\"]\napply = ['sh \"$(git rev-parse --git-common-dir)/apply.sh\"']\n").unwrap();
    fs::write(dir.path().join(".git/apply.sh"), "printf 'local override\\n' > AGENTS.md\nrm -f CLAUDE.md 'removed file'\nln -s AGENTS.md CLAUDE.md\nprintf 'local' > 'line\nbreak'\n").unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "apply"])
        .assert()
        .success();
    kin_cmd()
        .current_dir(dir.path())
        .args(["checkout", "down"])
        .assert()
        .success();
    assert_eq!(
        fs::read_link(dir.path().join("CLAUDE.md")).unwrap(),
        Path::new("AGENTS.md")
    );
    assert!(!dir.path().join("removed file").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("line\nbreak")).unwrap(),
        "local"
    );
    assert_applied(dir.path());
}

#[test]
fn untracked_overlay_does_not_block_a_branch_that_tracks_the_path() {
    let dir = setup();
    fs::write(dir.path().join(".git/kindra.toml"), "[overrides]\npaths = ['AGENTS.md', 'new config']\napply = ['sh \"$(git rev-parse --git-common-dir)/apply.sh\"']\n").unwrap();
    fs::write(
        dir.path().join(".git/apply.sh"),
        "printf 'local override\\n' > AGENTS.md\nprintf 'local config' > 'new config'\n",
    )
    .unwrap();
    commit(dir.path(), "new config", "tracked config");
    kin_cmd()
        .current_dir(dir.path())
        .args(["checkout", "down"])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(dir.path().join("new config")).unwrap(),
        "local config"
    );
    kin_cmd()
        .current_dir(dir.path())
        .args(["checkout", "up"])
        .assert()
        .success();
    assert_eq!(
        git(dir.path(), &["show", "HEAD:new config"]),
        "tracked config"
    );
    assert!(git(dir.path(), &["ls-files", "-v", "new config"]).starts_with("S "));
}

#[test]
fn symlinked_parent_is_rejected_without_modifying_the_source() {
    let dir = setup();
    fs::create_dir(dir.path().join("agents")).unwrap();
    commit(dir.path(), "agents/rule", "tracked");
    let source = tempdir().unwrap();
    fs::write(source.path().join("rule"), "external override").unwrap();
    fs::remove_file(dir.path().join("agents/rule")).unwrap();
    fs::remove_dir(dir.path().join("agents")).unwrap();
    std::os::unix::fs::symlink(source.path(), dir.path().join("agents")).unwrap();
    fs::write(
        dir.path().join(".git/kindra.toml"),
        "[overrides]\npaths = ['agents']\napply = ['true']\n",
    )
    .unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "apply"])
        .assert()
        .failure();
    assert_eq!(
        fs::read_to_string(source.path().join("rule")).unwrap(),
        "external override"
    );
    assert!(!dir.path().join(".git/kindra_overrides_state.json").exists());
}

#[test]
fn clear_state_preserves_suspended_conflicts() {
    let dir = conflict_setup();
    kin_cmd()
        .current_dir(dir.path())
        .args(["move", "--onto", "target"])
        .assert()
        .failure();
    let conflict = fs::read(dir.path().join("AGENTS.md")).unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .args(["abort", "--clear-state"])
        .assert()
        .success();
    assert_eq!(fs::read(dir.path().join("AGENTS.md")).unwrap(), conflict);
    assert!(dir.path().join(".git/kindra_overrides_state.json").exists());
    run_ok("git", &["rebase", "--abort"], dir.path());
    kin_cmd()
        .current_dir(dir.path())
        .arg("continue")
        .assert()
        .success();
    assert_applied(dir.path());
}

#[test]
fn worktree_setup_hooks_see_applied_overrides_after_switching() {
    let dir = setup();
    let config = dir.path().join(".git/kindra.toml");
    let mut contents = fs::read_to_string(&config).unwrap();
    contents.push_str("\n[worktrees.hooks]\non_create = [\"test \\\"$(cat AGENTS.md)\\\" = 'local override'\"]\non_checkout = [\"test \\\"$(cat AGENTS.md)\\\" = 'local override'\"]\n");
    fs::write(config, contents).unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .args(["wt", "review", "main"])
        .assert()
        .success();
    kin_cmd()
        .current_dir(dir.path())
        .args(["wt", "review", "feature"])
        .assert()
        .success();
    assert_applied(&dir.path().join(".git/kindra-worktrees/review"));
}

#[test]
fn initial_checkpoint_failure_leaves_files_and_index_unchanged() {
    let dir = setup();
    let marker = dir.path().join(".git/fail-state-write");
    fs::write(
        &marker,
        dir.path()
            .canonicalize()
            .unwrap()
            .join(".git/kindra_overrides_state.json")
            .to_str()
            .unwrap(),
    )
    .unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .env("KIN_TEST_FAIL_STATE_WRITE", &marker)
        .args(["checkout", "down"])
        .assert()
        .failure();
    assert_eq!(
        git(dir.path(), &["branch", "--show-current"]).trim(),
        "feature"
    );
    assert_applied(dir.path());
}

#[test]
fn preparation_failure_restores_symlinks_deletions_modes_and_flags() {
    use std::os::unix::fs::PermissionsExt;
    let dir = setup();
    commit(dir.path(), "CLAUDE.md", "tracked");
    commit(dir.path(), "deleted", "tracked");
    fs::remove_file(dir.path().join("CLAUDE.md")).unwrap();
    std::os::unix::fs::symlink("AGENTS.md", dir.path().join("CLAUDE.md")).unwrap();
    fs::remove_file(dir.path().join("deleted")).unwrap();
    fs::set_permissions(
        dir.path().join("AGENTS.md"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    run_ok(
        "git",
        &["update-index", "--skip-worktree", "CLAUDE.md", "deleted"],
        dir.path(),
    );
    fs::write(
        dir.path().join(".git/kindra.toml"),
        "[overrides]\npaths = ['AGENTS.md', 'CLAUDE.md', 'deleted']\napply = ['exit 99']\n",
    )
    .unwrap();
    let bin = tempdir().unwrap();
    let real_git = which::which("git").unwrap();
    let wrapper = bin.path().join("git");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nif [ \"$1\" = checkout-index ]; then exit 42; fi\nexec '{}' \"$@\"\n",
            real_git.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").unwrap()
    );
    kin_cmd()
        .current_dir(dir.path())
        .env("PATH", path)
        .args(["checkout", "down"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("checkout-index"));
    assert_applied(dir.path());
    assert_eq!(
        fs::read_link(dir.path().join("CLAUDE.md")).unwrap(),
        Path::new("AGENTS.md")
    );
    assert!(!dir.path().join("deleted").exists());
    assert_eq!(
        fs::metadata(dir.path().join("AGENTS.md"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert!(
        git(dir.path(), &["ls-files", "-v", "CLAUDE.md", "deleted"])
            .lines()
            .all(|l| l.starts_with("S "))
    );
}

#[test]
fn recovery_snapshot_is_private_and_read_only_commands_do_not_apply() {
    use std::os::unix::fs::PermissionsExt;
    let dir = setup();
    fs::write(dir.path().join(".git/apply.sh"), "exit 42\n").unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .arg("status")
        .assert()
        .success();
    assert_applied(dir.path());
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "apply"])
        .assert()
        .failure();
    let mode = fs::metadata(dir.path().join(".git/kindra_overrides_state.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    kin_cmd()
        .current_dir(dir.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("overrides"));
}

#[test]
fn failed_review_setup_rolls_back_and_reapplies_overrides() {
    let dir = setup();
    kin_cmd()
        .current_dir(dir.path())
        .args(["wt", "review", "main"])
        .assert()
        .success();
    let config = dir.path().join(".git/kindra.toml");
    let contents =
        fs::read_to_string(&config).unwrap() + "\n[worktrees.hooks]\non_checkout = ['exit 42']\n";
    fs::write(config, contents).unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .args(["wt", "review", "feature"])
        .assert()
        .failure();
    let review = dir.path().join(".git/kindra-worktrees/review");
    assert_eq!(git(&review, &["branch", "--show-current"]).trim(), "main");
    assert_applied(&review);
}

#[test]
fn commit_does_not_commit_overrides_and_ignored_env_files_stay_present() {
    let dir = setup();
    fs::write(dir.path().join(".git/info/exclude"), ".env\n").unwrap();
    fs::write(dir.path().join(".env"), "LOCAL_ONLY=secret").unwrap();
    fs::write(dir.path().join("feature.txt"), "new feature").unwrap();
    run_ok("git", &["add", "feature.txt"], dir.path());
    kin_cmd()
        .current_dir(dir.path())
        .args(["commit", "-m", "new feature"])
        .assert()
        .success();
    assert_eq!(git(dir.path(), &["show", "HEAD:AGENTS.md"]), "feature\n");
    assert_eq!(
        fs::read_to_string(dir.path().join(".env")).unwrap(),
        "LOCAL_ONLY=secret"
    );
    assert_applied(dir.path());
}

#[test]
fn interrupted_preparation_recovers_only_if_head_and_index_still_match() {
    for change_head in [false, true] {
        let dir = setup();
        fs::write(dir.path().join(".git/apply.sh"), "exit 42\n").unwrap();
        kin_cmd()
            .current_dir(dir.path())
            .args(["overrides", "apply"])
            .assert()
            .failure();
        // Model a process stopping after checkout-index but before the durable
        // Suspended checkpoint: saved contents are original, disk is baseline.
        let state_path = dir.path().join(".git/kindra_overrides_state.json");
        let mut state: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
        state["phase"] = "Preparing".into();
        fs::write(&state_path, serde_json::to_string(&state).unwrap()).unwrap();
        if change_head {
            run_ok("git", &["checkout", "main"], dir.path());
            kin_cmd()
                .current_dir(dir.path())
                .arg("continue")
                .assert()
                .failure()
                .stderr(predicates::str::contains("Git changed"));
            assert_eq!(
                fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
                "base\n"
            );
            assert!(state_path.exists());
        } else {
            kin_cmd()
                .current_dir(dir.path())
                .arg("continue")
                .assert()
                .success();
            assert_applied(dir.path());
        }
    }
}

#[test]
fn glob_pathspecs_apply_from_a_subdirectory() {
    let dir = setup();
    fs::create_dir_all(dir.path().join("apps/web")).unwrap();
    commit(dir.path(), "apps/web/AGENTS.md", "nested tracked");
    fs::write(dir.path().join(".git/kindra.toml"), "[overrides]\npaths = [':(glob)**/AGENTS.md']\napply = ['printf nested-local > apps/web/AGENTS.md']\n").unwrap();
    kin_cmd()
        .current_dir(dir.path().join("apps/web"))
        .args(["overrides", "apply"])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(dir.path().join("apps/web/AGENTS.md")).unwrap(),
        "nested-local"
    );
    assert!(git(dir.path(), &["ls-files", "-v", "apps/web/AGENTS.md"]).starts_with("S "));
}

#[test]
fn remove_restores_originals_until_explicitly_reenabled() {
    let dir = setup();
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "remove"])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
        "feature\n"
    );
    assert!(git(dir.path(), &["ls-files", "-v", "AGENTS.md"]).starts_with("H "));
    kin_cmd()
        .current_dir(dir.path())
        .args(["checkout", "down"])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
        "base\n"
    );
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "apply"])
        .assert()
        .success();
    assert_applied(dir.path());
    kin_cmd()
        .current_dir(dir.path())
        .args(["checkout", "up"])
        .assert()
        .success();
    assert_applied(dir.path());
}

#[test]
fn removed_overrides_allow_editing_and_committing_original_files() {
    let dir = setup();
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "remove"])
        .assert()
        .success();
    fs::write(dir.path().join("AGENTS.md"), "intentional edit\n").unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "remove"])
        .assert()
        .success();
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "apply"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("uncommitted"));
    assert_eq!(
        fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
        "intentional edit\n"
    );
    run_ok("git", &["add", "AGENTS.md"], dir.path());
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "apply"])
        .assert()
        .failure();
    kin_cmd()
        .current_dir(dir.path())
        .args(["commit", "-m", "Edit original instructions"])
        .assert()
        .success();
    assert_eq!(
        git(dir.path(), &["show", "HEAD:AGENTS.md"]),
        "intentional edit\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
        "intentional edit\n"
    );
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "apply"])
        .assert()
        .success();
    assert_applied(dir.path());
}

#[test]
fn removal_restores_symlinks_and_deletions_and_removes_untracked_overlay_files() {
    let dir = setup();
    commit(dir.path(), "CLAUDE.md", "original Claude instructions");
    commit(dir.path(), "deleted", "original deleted file");
    fs::write(dir.path().join(".git/kindra.toml"), "[overrides]\npaths = ['AGENTS.md', 'CLAUDE.md', 'deleted', 'local.json']\napply = ['sh \"$(git rev-parse --git-common-dir)/apply.sh\"']\n").unwrap();
    fs::write(dir.path().join(".git/apply.sh"), "printf 'local override\\n' > AGENTS.md\nrm -f CLAUDE.md deleted\nln -s AGENTS.md CLAUDE.md\nprintf local > local.json\n").unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "apply"])
        .assert()
        .success();
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "remove"])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap(),
        "original Claude instructions"
    );
    assert!(
        !fs::symlink_metadata(dir.path().join("CLAUDE.md"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("deleted")).unwrap(),
        "original deleted file"
    );
    assert!(!dir.path().join("local.json").exists());
    // A newly created original file must also be preserved when re-enabling.
    fs::write(dir.path().join("local.json"), "new original config").unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "apply"])
        .assert()
        .failure();
    assert_eq!(
        fs::read_to_string(dir.path().join("local.json")).unwrap(),
        "new original config"
    );
}

#[test]
fn removal_is_worktree_local_and_review_switches_remain_disabled() {
    let dir = setup();
    kin_cmd()
        .current_dir(dir.path())
        .args(["wt", "review", "main"])
        .assert()
        .success();
    let review = dir.path().join(".git/kindra-worktrees/review");
    kin_cmd()
        .current_dir(&review)
        .args(["overrides", "remove"])
        .assert()
        .success();
    kin_cmd()
        .current_dir(dir.path())
        .args(["wt", "review", "feature"])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(review.join("AGENTS.md")).unwrap(),
        "feature\n"
    );
    assert_applied(dir.path());
    kin_cmd()
        .current_dir(&review)
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("disabled"));
}

#[test]
fn removal_refuses_conflicts_without_overwriting_resolutions() {
    let dir = conflict_setup();
    kin_cmd()
        .current_dir(dir.path())
        .args(["move", "--onto", "target"])
        .assert()
        .failure();
    let conflict = fs::read(dir.path().join("AGENTS.md")).unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "remove"])
        .assert()
        .failure();
    assert_eq!(fs::read(dir.path().join("AGENTS.md")).unwrap(), conflict);
    kin_cmd()
        .current_dir(dir.path())
        .arg("abort")
        .assert()
        .success();
    assert_applied(dir.path());
}

#[test]
fn interrupted_removal_finishes_without_reapplying_overrides() {
    let dir = setup();
    let marker = dir.path().join(".git/fail-disable-write");
    fs::write(
        &marker,
        dir.path()
            .canonicalize()
            .unwrap()
            .join(".git/kindra_overrides_disabled")
            .to_str()
            .unwrap(),
    )
    .unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .env("KIN_TEST_FAIL_STATE_WRITE", &marker)
        .args(["overrides", "remove"])
        .assert()
        .failure();
    assert_eq!(
        fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
        "feature\n"
    );
    assert!(dir.path().join(".git/kindra_overrides_state.json").exists());
    fs::write(dir.path().join(".git/apply.sh"), "exit 42\n").unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .arg("continue")
        .assert()
        .success();
    assert!(!dir.path().join(".git/kindra_overrides_state.json").exists());
    assert!(dir.path().join(".git/kindra_overrides_disabled").exists());
    kin_cmd()
        .current_dir(dir.path())
        .args(["checkout", "down"])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
        "base\n"
    );
}

#[test]
fn failed_reenable_keeps_disabled_marker_until_recovery_succeeds() {
    let dir = setup();
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "remove"])
        .assert()
        .success();
    fs::write(dir.path().join(".git/apply.sh"), "exit 42\n").unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "apply"])
        .assert()
        .failure();
    assert!(dir.path().join(".git/kindra_overrides_disabled").exists());
    kin_cmd()
        .current_dir(dir.path())
        .args(["checkout", "down"])
        .assert()
        .failure();
    fs::write(
        dir.path().join(".git/apply.sh"),
        "printf 'local override\\n' > AGENTS.md\n",
    )
    .unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .arg("continue")
        .assert()
        .success();
    assert!(!dir.path().join(".git/kindra_overrides_disabled").exists());
    assert_applied(dir.path());
}

#[test]
fn removal_refuses_staged_managed_changes_and_preserves_other_staged_files() {
    let dir = setup();
    fs::write(dir.path().join("staged.txt"), "keep").unwrap();
    run_ok("git", &["add", "staged.txt"], dir.path());
    run_ok(
        "git",
        &["update-index", "--no-skip-worktree", "AGENTS.md"],
        dir.path(),
    );
    fs::write(dir.path().join("AGENTS.md"), "staged original").unwrap();
    run_ok("git", &["add", "AGENTS.md"], dir.path());
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "remove"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("staged"));
    assert_eq!(git(dir.path(), &["show", ":AGENTS.md"]), "staged original");
    run_ok("git", &["restore", "--staged", "AGENTS.md"], dir.path());
    kin_cmd()
        .current_dir(dir.path())
        .args(["overrides", "remove"])
        .assert()
        .success();
    assert_eq!(
        git(dir.path(), &["diff", "--cached", "--name-only"]).trim(),
        "staged.txt"
    );
}
