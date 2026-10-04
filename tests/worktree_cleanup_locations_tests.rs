mod common;

use common::{branch_exists, kin_cmd, run_ok, setup_worktree_repo, write_repo_config};
use std::fs;
use std::path::Path;

fn add_worktree(repo: &Path, path: &Path, branch: &str) {
    run_ok(
        "git",
        &[
            "worktree",
            "add",
            "-b",
            branch,
            path.to_str().unwrap(),
            "main",
        ],
        repo,
    );
}

fn list(repo: &Path) -> String {
    let output = kin_cmd()
        .args(["wt", "list"])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

fn row<'a>(listing: &'a str, branch: &str) -> &'a str {
    listing
        .lines()
        .find(|line| line.split_whitespace().nth(1) == Some(branch))
        .unwrap()
}

#[test]
fn cleanup_locations_classify_multiple_roots_and_never_create_in_them() {
    let dir = setup_worktree_repo();
    let external = tempfile::tempdir().unwrap();
    let agents = dir.path().join(".claude/worktrees");
    add_worktree(dir.path(), &agents.join("one"), "agent-one");
    add_worktree(dir.path(), &external.path().join("two"), "agent-two");
    write_repo_config(
        dir.path(),
        &format!(
            r#"
[worktrees]
root = "."
[worktrees.main]
path = "."
[worktrees.review]
enabled = false
[worktrees.temp]
path_template = ".git/kindra-worktrees/temp/{{branch}}"
[[worktrees.cleanup]]
path = ".claude/worktrees"
[[worktrees.cleanup]]
path = {}
"#,
            serde_json::to_string(&external.path().to_string_lossy()).unwrap()
        ),
    );
    let listing = list(dir.path());
    assert!(row(&listing, "main").starts_with("main "), "{listing}");
    assert!(
        row(&listing, "agent-one").starts_with("cleanup "),
        "{listing}"
    );
    assert!(
        row(&listing, "agent-two").starts_with("cleanup "),
        "{listing}"
    );
    let output = kin_cmd()
        .args(["wt", "temp", "-b", "created", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        dir.path()
            .join(".git/kindra-worktrees/temp/created")
            .is_dir()
    );
    assert!(!agents.join("created").exists());
    assert!(!external.path().join("created").exists());
}

#[test]
fn cleanup_locations_remove_merged_worktrees_and_respect_dirty_and_unmerged_state() {
    let dir = setup_worktree_repo();
    let external = tempfile::tempdir().unwrap();
    let clean = external.path().join("clean");
    let dirty = external.path().join("dirty");
    let unmerged = external.path().join("unmerged");
    add_worktree(dir.path(), &clean, "clean-agent");
    add_worktree(dir.path(), &dirty, "dirty-agent");
    run_ok(
        "git",
        &["worktree", "add", unmerged.to_str().unwrap(), "feature-a"],
        dir.path(),
    );
    fs::write(dirty.join("file.txt"), "pending edits").unwrap();
    write_repo_config(
        dir.path(),
        &format!(
            r#"
[worktrees]
root = "."
[worktrees.main]
path = "."
[worktrees.temp]
enabled = false
delete_merged = false
[[worktrees.cleanup]]
path = {}
"#,
            serde_json::to_string(&external.path().to_string_lossy()).unwrap()
        ),
    );
    let listing = list(dir.path());
    assert!(row(&listing, "clean-agent").contains("merged"), "{listing}");
    assert!(
        !row(&listing, "feature-a")
            .split_whitespace()
            .nth(2)
            .unwrap()
            .split(',')
            .any(|state| state == "merged"),
        "{listing}"
    );
    let output = kin_cmd()
        .args(["wt", "cleanup", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!clean.exists());
    assert!(!branch_exists(dir.path(), "clean-agent"));
    assert!(dirty.exists());
    assert!(branch_exists(dir.path(), "dirty-agent"));
    assert!(unmerged.exists());
    assert!(branch_exists(dir.path(), "feature-a"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("cleanup dirty-agent"));
    let output = kin_cmd()
        .args(["wt", "cleanup", "--yes", "--force", "--keep-branch"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!dirty.exists());
    assert!(branch_exists(dir.path(), "dirty-agent"));
    assert!(unmerged.exists());
}

#[test]
fn cleanup_locations_exclude_persistent_roles_prefix_neighbors_missing_and_detached_worktrees() {
    let dir = setup_worktree_repo();
    let agents = dir.path().join(".agents");
    let review = agents.join("review");
    let plain = dir.path().join(".agents-other/plain");
    let missing = agents.join("missing");
    let detached = agents.join("detached");
    add_worktree(dir.path(), &review, "review-branch");
    add_worktree(dir.path(), &plain, "plain-branch");
    add_worktree(dir.path(), &missing, "missing-branch");
    run_ok(
        "git",
        &[
            "worktree",
            "add",
            "--detach",
            detached.to_str().unwrap(),
            "main",
        ],
        dir.path(),
    );
    fs::remove_dir_all(&missing).unwrap();
    write_repo_config(
        dir.path(),
        r#"
[worktrees]
root = "."
[worktrees.main]
path = "."
[worktrees.review]
path = ".agents/review"
[worktrees.temp]
enabled = false
[[worktrees.cleanup]]
path = ".agents"
"#,
    );
    let listing = list(dir.path());
    assert!(row(&listing, "main").starts_with("main "), "{listing}");
    assert!(
        row(&listing, "review-branch").starts_with("review "),
        "{listing}"
    );
    assert!(row(&listing, "plain-branch").starts_with("- "), "{listing}");
    assert!(
        row(&listing, "missing-branch").contains("missing"),
        "{listing}"
    );
    let output = kin_cmd()
        .args(["wt", "cleanup", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("No worktrees are eligible"));
    assert!(review.is_dir());
    assert!(plain.is_dir());
    assert!(detached.is_dir());
}

#[test]
fn cleanup_locations_do_not_override_temp_policy_or_enable_creation_when_temp_is_disabled() {
    let dir = setup_worktree_repo();
    let temp = dir.path().join(".git/kindra-worktrees/temp/existing");
    add_worktree(dir.path(), &temp, "temp-branch");
    write_repo_config(
        dir.path(),
        r#"
[worktrees.temp]
delete_merged = false
[[worktrees.cleanup]]
path = ".git/kindra-worktrees"
"#,
    );
    let listing = list(dir.path());
    assert!(
        row(&listing, "temp-branch").starts_with("temp "),
        "{listing}"
    );
    assert!(
        !row(&listing, "temp-branch").contains("merged"),
        "{listing}"
    );
    let output = kin_cmd()
        .args(["wt", "cleanup", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(temp.is_dir());
    write_repo_config(
        dir.path(),
        r#"
[worktrees.temp]
enabled = false
[[worktrees.cleanup]]
path = ".agents"
"#,
    );
    let output = kin_cmd()
        .args(["wt", "temp", "-b", "unexpected", "main"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!branch_exists(dir.path(), "unexpected"));
    assert!(!dir.path().join(".agents").exists());
}

#[test]
fn cleanup_locations_validate_paths_and_reject_creation_settings() {
    let dir = setup_worktree_repo();
    for entry in [
        "[[worktrees.cleanup]]\npath = ''",
        "[[worktrees.cleanup]]\npath = '  '",
        "[[worktrees.cleanup]]",
        "[[worktrees.cleanup]]\npath = '.agents'\npath_template = '.agents/{branch}'",
        "[worktrees.cleanup]\npath = '.agents'",
    ] {
        write_repo_config(dir.path(), entry);
        let output = kin_cmd()
            .args(["wt", "list"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(!output.status.success(), "{entry}: {output:?}");
    }
}

#[test]
fn cleanup_locations_explicit_remove_uses_cleanup_policy_independent_of_temp() {
    let dir = setup_worktree_repo();
    let agents = dir.path().join(".agents");
    let worktree = agents.join("merged");
    add_worktree(dir.path(), &worktree, "merged-agent");
    write_repo_config(
        dir.path(),
        r#"
[worktrees.temp]
delete_merged = false
[[worktrees.cleanup]]
path = ".agents"
"#,
    );
    let output = kin_cmd()
        .args(["wt", "remove", "merged-agent", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("cleanup"));
    assert!(!worktree.exists());
    assert!(!branch_exists(dir.path(), "merged-agent"));
}

#[cfg(unix)]
#[test]
fn cleanup_locations_run_global_remove_hooks_with_cleanup_role() {
    let dir = setup_worktree_repo();
    let agents = dir.path().join(".agents");
    let worktree = agents.join("merged");
    add_worktree(dir.path(), &worktree, "merged-agent");
    let marker = dir.path().join(".git/cleanup-hook-role");
    let hook = format!(
        "printf '%s' \"$KINDRA_WORKTREE_ROLE\" > '{}'",
        marker.display()
    );
    write_repo_config(
        dir.path(),
        &format!(
            r#"
[worktrees.hooks]
on_remove = [{}]
[[worktrees.cleanup]]
path = ".agents"
"#,
            serde_json::to_string(&hook).unwrap()
        ),
    );
    let output = kin_cmd()
        .args(["wt", "cleanup", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fs::read_to_string(marker).unwrap(), "cleanup");
}

#[cfg(unix)]
#[test]
fn cleanup_locations_resolve_symlinked_ancestors_before_directory_creation() {
    let dir = setup_worktree_repo();
    let external = tempfile::tempdir().unwrap();
    let alias = dir.path().join(".git/agent-alias");
    std::os::unix::fs::symlink(external.path(), &alias).unwrap();
    write_repo_config(
        dir.path(),
        r#"
[[worktrees.cleanup]]
path = ".git/agent-alias/not-created"
"#,
    );
    let repo = git2::Repository::open(dir.path()).unwrap();
    let config = kindra::worktree::config::load_worktree_config(&repo).unwrap();
    assert_eq!(
        config.cleanup[0].path,
        fs::canonicalize(external.path())
            .unwrap()
            .join("not-created")
    );
    assert!(!external.path().join("not-created").exists());
    let worktree = external.path().join("not-created/agent");
    add_worktree(dir.path(), &worktree, "alias-agent");
    let listing = list(dir.path());
    assert!(
        row(&listing, "alias-agent").starts_with("cleanup "),
        "{listing}"
    );
}

#[cfg(unix)]
#[test]
fn cleanup_locations_preserve_roles_configured_through_symlink_aliases() {
    let dir = setup_worktree_repo();
    let alias = dir.path().join(".git/role-alias");
    std::os::unix::fs::symlink(dir.path(), &alias).unwrap();
    let review = dir.path().join("agents/review");
    let temp = dir.path().join("agents/temp/retained");
    add_worktree(dir.path(), &review, "review-branch");
    add_worktree(dir.path(), &temp, "retained-temp");
    write_repo_config(
        dir.path(),
        r#"
[worktrees]
root = "."
[worktrees.main]
path = ".git/role-alias"
[worktrees.review]
path = ".git/role-alias/agents/review"
[worktrees.temp]
path_template = ".git/role-alias/agents/temp/{branch}"
delete_merged = false
[[worktrees.cleanup]]
path = "."
"#,
    );
    let listing = list(dir.path());
    assert!(row(&listing, "main").starts_with("main "), "{listing}");
    assert!(
        row(&listing, "review-branch").starts_with("review "),
        "{listing}"
    );
    assert!(
        row(&listing, "retained-temp").starts_with("temp "),
        "{listing}"
    );
    let output = kin_cmd()
        .args(["wt", "cleanup", "--yes"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("No worktrees are eligible"));
    assert!(review.is_dir());
    assert!(temp.is_dir());
    assert!(branch_exists(dir.path(), "review-branch"));
    assert!(branch_exists(dir.path(), "retained-temp"));
}
