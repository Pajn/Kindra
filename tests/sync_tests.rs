mod common;

use common::{gits_cmd, make_commit, run_ok};
use git2::{BranchType, Repository};
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;
use tempfile::tempdir;

fn test_global_config_dir(root: &std::path::Path) -> std::path::PathBuf {
    if cfg!(target_os = "macos") {
        return root
            .join("Library")
            .join("Application Support")
            .join("gits");
    }
    if cfg!(target_os = "windows") {
        return root.join("AppData").join("Roaming").join("gits");
    }

    root.join(".config").join("gits")
}

fn apply_global_config_env(cmd: &mut assert_cmd::Command, root: &std::path::Path) {
    cmd.env("HOME", root);

    if cfg!(target_os = "linux") || cfg!(target_os = "freebsd") || cfg!(target_os = "openbsd") {
        cmd.env("XDG_CONFIG_HOME", root.join(".config"));
    }

    if cfg!(target_os = "windows") {
        cmd.env("APPDATA", root.join("AppData").join("Roaming"));
        cmd.env("LOCALAPPDATA", root.join("AppData").join("Local"));
    }
}

#[test]
fn sync_handles_rebased_lower_branch() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&base],
    );
    let a = repo.find_commit(a_id).unwrap();

    let b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feature b",
        &[&a],
    );
    let b = repo.find_commit(b_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-c",
        "c.txt",
        "c",
        "feature c",
        &[&b],
    );

    run_ok("git", &["checkout", "-f", "main"], dir.path());
    run_ok("git", &["cherry-pick", &a_id.to_string()], dir.path());
    run_ok("git", &["checkout", "-f", "feature-b"], dir.path());

    let old_feature_a = repo
        .find_branch("feature-a", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let old_feature_b = repo
        .find_branch("feature-b", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .arg("--no-delete")
        .current_dir(dir.path())
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();

    assert_eq!(repo.head().unwrap().shorthand(), Some("feature-c"));

    let new_feature_a = repo
        .find_branch("feature-a", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let new_feature_b = repo
        .find_branch("feature-b", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let new_feature_c = repo
        .find_branch("feature-c", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let main_tip = repo
        .find_branch("main", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();

    assert_eq!(new_feature_a, old_feature_a);
    assert_ne!(new_feature_b, old_feature_b);
    assert!(repo.graph_descendant_of(new_feature_b, main_tip).unwrap());
    assert!(
        repo.graph_descendant_of(new_feature_c, new_feature_b)
            .unwrap()
    );
}

#[test]
fn sync_handles_squashed_lower_branch() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    let a1_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a1",
        "feature a1",
        &[&base],
    );
    let a1 = repo.find_commit(a1_id).unwrap();

    let a2_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a2.txt",
        "a2",
        "feature a2",
        &[&a1],
    );
    let a2 = repo.find_commit(a2_id).unwrap();

    let old_feature_b = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feature b",
        &[&a2],
    );

    run_ok("git", &["checkout", "-f", "main"], dir.path());
    let squash_range = format!("{}^..{}", a1_id, a2_id);
    run_ok(
        "git",
        &["cherry-pick", "--no-commit", &squash_range],
        dir.path(),
    );
    run_ok("git", &["commit", "-m", "squash feature-a"], dir.path());

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .arg("--no-delete")
        .current_dir(dir.path())
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();

    assert_eq!(repo.head().unwrap().shorthand(), Some("feature-b"));

    let new_feature_a = repo
        .find_branch("feature-a", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let new_feature_b = repo
        .find_branch("feature-b", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let main_tip = repo
        .find_branch("main", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();

    assert!(
        repo.graph_descendant_of(new_feature_a, main_tip).unwrap() || new_feature_a == main_tip
    );
    assert!(
        repo.graph_descendant_of(new_feature_b, new_feature_a)
            .unwrap()
            || new_feature_b == new_feature_a
    );
    assert_ne!(new_feature_b, old_feature_b);
    assert!(repo.graph_descendant_of(new_feature_b, main_tip).unwrap());
}

#[test]
fn sync_handles_merged_lower_branch() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&base],
    );
    let a = repo.find_commit(a_id).unwrap();

    let old_feature_b = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feature b",
        &[&a],
    );

    run_ok("git", &["checkout", "-f", "main"], dir.path());
    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());
    run_ok(
        "git",
        &["commit", "--allow-empty", "-m", "main advanced"],
        dir.path(),
    );

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    let old_feature_a = repo
        .find_branch("feature-a", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .arg("--no-delete")
        .current_dir(dir.path())
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();

    assert_eq!(repo.head().unwrap().shorthand(), Some("feature-b"));

    let new_feature_a = repo
        .find_branch("feature-a", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let new_feature_b = repo
        .find_branch("feature-b", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let main_tip = repo
        .find_branch("main", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();

    assert_eq!(new_feature_a, old_feature_a);
    assert_ne!(new_feature_b, old_feature_b);
    assert!(repo.graph_descendant_of(new_feature_b, main_tip).unwrap());
}

#[test]
fn sync_rebases_onto_remote_tracking_base_when_local_base_is_stale() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let remote_dir = dir.path().join("remote.git");
    fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();
    run_ok("git", &["push", "-u", "origin", "main:main"], dir.path());

    let feature_before = make_commit(
        &repo,
        "refs/heads/feature-a",
        "feature.txt",
        "feature",
        "feature a",
        &[&base],
    );

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

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    let local_main_before = repo
        .find_branch("main", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let origin_main_before = repo.revparse_single("origin/main").unwrap().id();
    assert_eq!(local_main_before, origin_main_before);

    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .arg("--no-delete")
        .current_dir(dir.path())
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();
    let origin_main_after_sync = repo.revparse_single("origin/main").unwrap().id();
    let feature_after = repo
        .find_branch("feature-a", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let feature_after_commit = repo.find_commit(feature_after).unwrap();

    assert_ne!(origin_main_before, origin_main_after_sync);
    assert_ne!(feature_after, feature_before);
    assert!(
        repo.graph_descendant_of(feature_after, origin_main_after_sync)
            .unwrap()
    );
    assert_eq!(
        feature_after_commit.parent_id(0).unwrap(),
        origin_main_after_sync
    );
}

#[test]
fn sync_treats_slashed_base_branch_name_as_local_before_remote() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let remote_dir = dir.path().join("upstream.git");
    fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "upstream", remote_dir.to_str().unwrap()],
        dir.path(),
    );

    let base_id = make_commit(
        &repo,
        "refs/heads/release/2026.03",
        "release.txt",
        "base",
        "release base",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();
    run_ok(
        "git",
        &["push", "-u", "upstream", "release/2026.03:release/2026.03"],
        dir.path(),
    );
    fs::write(
        repo.path().join("gits.toml"),
        r#"upstream_branch = "release/2026.03""#,
    )
    .unwrap();

    let feature_before = make_commit(
        &repo,
        "refs/heads/feature-a",
        "feature.txt",
        "feature",
        "feature a",
        &[&base],
    );

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
    run_ok(
        "git",
        &["checkout", "release/2026.03"],
        remote_worktree.path(),
    );
    fs::write(remote_worktree.path().join("remote.txt"), "remote release").unwrap();
    run_ok("git", &["add", "remote.txt"], remote_worktree.path());
    run_ok(
        "git",
        &["commit", "-m", "remote release advanced"],
        remote_worktree.path(),
    );
    run_ok(
        "git",
        &["push", "origin", "release/2026.03"],
        remote_worktree.path(),
    );

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    let upstream_before = repo
        .revparse_single("upstream/release/2026.03")
        .unwrap()
        .id();
    let local_release_before = repo
        .find_branch("release/2026.03", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    assert_eq!(upstream_before, local_release_before);

    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .arg("--no-delete")
        .current_dir(dir.path())
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();
    let upstream_after = repo
        .revparse_single("upstream/release/2026.03")
        .unwrap()
        .id();
    let feature_after = repo
        .find_branch("feature-a", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let feature_after_commit = repo.find_commit(feature_after).unwrap();

    assert_ne!(upstream_before, upstream_after);
    assert_ne!(feature_before, feature_after);
    assert_eq!(feature_after_commit.parent_id(0).unwrap(), upstream_after);
}

#[test]
fn sync_reports_rebase_conflict() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "file.txt",
        "feature change",
        "feature a",
        &[&base],
    );
    let a = repo.find_commit(a_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feature b",
        &[&a],
    );

    make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "main change",
        "main conflicting change",
        &[&base],
    );

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("rebase").or(predicate::str::contains("Resolve conflicts")),
        );

    assert!(
        dir.path().join(".git/rebase-merge").exists()
            || dir.path().join(".git/rebase-apply").exists(),
        "Expected git rebase state to remain after conflict"
    );
}

#[test]
fn sync_refuses_when_git_rebase_in_progress() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    let feature_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&base],
    );
    let feature = repo.find_commit(feature_id).unwrap();

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_tree(
        feature.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    std::fs::create_dir_all(dir.path().join(".git/rebase-merge")).unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("rebase").or(predicate::str::contains("in progress")));
}

#[test]
fn sync_refuses_to_auto_pick_tip_in_non_interactive_mode() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&base],
    );
    let a = repo.find_commit(a_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feature b",
        &[&a],
    );
    make_commit(
        &repo,
        "refs/heads/feature-c",
        "c.txt",
        "c",
        "feature c",
        &[&a],
    );

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("Multiple stack tips found"));
}

#[test]
fn sync_ignores_rebase_autostash_config() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "base\n",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-a",
        "file.txt",
        "base\nfeature\n",
        "feature a",
        &[&base],
    );

    make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "base\nmain\n",
        "main change",
        &[&base],
    );

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    run_ok("git", &["config", "rebase.autostash", "true"], dir.path());
    fs::write(dir.path().join("file.txt"), "base\nfeature\ndirty\n").unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("sync").current_dir(dir.path()).assert().failure();

    assert!(
        !dir.path().join(".git/rebase-merge").exists()
            && !dir.path().join(".git/rebase-apply").exists(),
        "sync should fail before starting a rebase when the worktree is dirty"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("file.txt")).unwrap(),
        "base\nfeature\ndirty\n"
    );
}

#[test]
fn sync_cli_no_autostash_overrides_repo_config() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "base\n",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-a",
        "file.txt",
        "base\nfeature\n",
        "feature a",
        &[&base],
    );

    make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "base\nmain\n",
        "main change",
        &[&base],
    );

    std::fs::write(
        repo.path().join("gits.toml"),
        "[rebase]\nautostash = true\n",
    )
    .unwrap();

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    fs::write(dir.path().join("file.txt"), "base\nfeature\ndirty\n").unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .arg("--no-autostash")
        .current_dir(dir.path())
        .assert()
        .failure();

    assert!(
        !dir.path().join(".git/rebase-merge").exists()
            && !dir.path().join(".git/rebase-apply").exists(),
        "CLI --no-autostash should override repo config"
    );
}

#[test]
fn sync_global_config_enables_autostash() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "base\n",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-a",
        "file.txt",
        "base\nfeature\n",
        "feature a",
        &[&base],
    );

    make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "base\nmain\n",
        "main change",
        &[&base],
    );

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    fs::write(dir.path().join("file.txt"), "base\nfeature\ndirty\n").unwrap();

    let global_config_root = TempDir::new().unwrap();
    let global_config_dir = test_global_config_dir(global_config_root.path());
    std::fs::create_dir_all(&global_config_dir).unwrap();
    std::fs::write(
        global_config_dir.join("config.toml"),
        "[rebase]\nautostash = true\n",
    )
    .unwrap();

    let mut cmd = gits_cmd();
    cmd.current_dir(dir.path());
    apply_global_config_env(&mut cmd, global_config_root.path());
    cmd.arg("sync").assert().failure();

    assert!(
        dir.path().join(".git/rebase-merge").exists()
            || dir.path().join(".git/rebase-apply").exists(),
        "global config should allow sync to start rebasing with autostash"
    );
}

#[cfg(unix)]
#[test]
fn sync_errors_when_git_too_old_for_update_refs() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&base],
    );
    let a = repo.find_commit(a_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feature b",
        &[&a],
    );

    run_ok("git", &["checkout", "-f", "main"], dir.path());
    run_ok("git", &["cherry-pick", &a_id.to_string()], dir.path());
    run_ok("git", &["checkout", "-f", "feature-b"], dir.path());

    let git_wrapper = dir.path().join("git");
    let real_git = which::which("git").unwrap();
    fs::write(
        &git_wrapper,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo \"git version 2.37.0\"\n  exit 0\nfi\nexec \"{}\" \"$@\"\n",
            real_git.to_string_lossy()
        ),
    )
    .unwrap();
    let mut perms = fs::metadata(&git_wrapper).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&git_wrapper, perms).unwrap();

    let old_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", dir.path().display(), old_path);

    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .current_dir(dir.path())
        .env("PATH", new_path)
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("operation requires Git >= 2.38.0")
                .and(predicate::str::contains("--update-refs")),
        );
}

#[cfg(unix)]
#[test]
fn sync_checkout_error_includes_branch_name() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&base],
    );
    let a = repo.find_commit(a_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feature b",
        &[&a],
    );

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    let git_wrapper = dir.path().join("git");
    let real_git = which::which("git").unwrap();
    fs::write(
        &git_wrapper,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"checkout\" ] && [ \"$2\" = \"feature-b\" ]; then\n  exit 1\nfi\nexec \"{}\" \"$@\"\n",
            real_git.to_string_lossy()
        ),
    )
    .unwrap();
    let mut perms = fs::metadata(&git_wrapper).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&git_wrapper, perms).unwrap();

    let old_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", dir.path().display(), old_path);

    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .current_dir(dir.path())
        .env("PATH", new_path)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "git checkout failed for branch 'feature-b'",
        ));
}

#[test]
fn sync_deletes_merged_branches() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&base],
    );
    let a = repo.find_commit(a_id).unwrap();

    let _b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feature b",
        &[&a],
    );

    // Merge feature-a into main
    run_ok("git", &["checkout", "-f", "main"], dir.path());
    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());

    run_ok("git", &["checkout", "-f", "feature-b"], dir.path());

    let mut cmd = gits_cmd();
    cmd.arg("sync").current_dir(dir.path()).assert().success();

    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo.find_branch("feature-a", BranchType::Local).is_err());
    assert!(repo.find_branch("feature-b", BranchType::Local).is_ok());

    // Check that feature-b is rebased onto main
    let main_tip = repo.revparse_single("main").unwrap().id();
    let feature_b_tip = repo.revparse_single("feature-b").unwrap().id();
    assert!(repo.graph_descendant_of(feature_b_tip, main_tip).unwrap());
}

#[test]
fn sync_on_main_rebases_to_latest_origin_main_and_deletes_merged_local_branches() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let remote_dir = dir.path().join("remote.git");
    fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();
    run_ok("git", &["push", "-u", "origin", "main:main"], dir.path());

    let feature_a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&base],
    );
    let feature_a = repo.find_commit(feature_a_id).unwrap();
    let _feature_b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feature b",
        &[&feature_a],
    );

    run_ok("git", &["checkout", "-f", "main"], dir.path());
    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());
    run_ok("git", &["push", "origin", "main"], dir.path());

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

    let local_main_before = repo
        .find_branch("main", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("sync").current_dir(dir.path()).assert().success();

    let repo = Repository::open(dir.path()).unwrap();
    let local_main_after = repo
        .find_branch("main", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let origin_main_after = repo.revparse_single("origin/main").unwrap().id();

    assert_ne!(local_main_before, local_main_after);
    assert_eq!(local_main_after, origin_main_after);
    assert!(repo.find_branch("feature-a", BranchType::Local).is_err());
    assert!(repo.find_branch("feature-b", BranchType::Local).is_ok());
    assert_eq!(repo.head().unwrap().shorthand(), Some("main"));
}

#[test]
fn sync_on_main_handles_rebase_conflict_and_preserves_state() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let remote_dir = dir.path().join("remote.git");
    fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "shared.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();
    run_ok("git", &["push", "-u", "origin", "main:main"], dir.path());

    let feature_a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&base],
    );
    run_ok("git", &["checkout", "-f", "main"], dir.path());
    run_ok(
        "git",
        &["merge", "--ff-only", &feature_a_id.to_string()],
        dir.path(),
    );

    let merged_main = repo.head().unwrap().peel_to_commit().unwrap();
    make_commit(
        &repo,
        "refs/heads/main",
        "shared.txt",
        "local main change",
        "local conflicting main change",
        &[&merged_main],
    );

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
    fs::write(
        remote_worktree.path().join("shared.txt"),
        "remote main change",
    )
    .unwrap();
    run_ok("git", &["add", "shared.txt"], remote_worktree.path());
    run_ok(
        "git",
        &["commit", "-m", "remote conflicting main change"],
        remote_worktree.path(),
    );
    run_ok("git", &["push", "origin", "main"], remote_worktree.path());

    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("rebase").or(predicate::str::contains("Resolve conflicts")),
        );

    let repo = Repository::open(dir.path()).unwrap();
    assert_ne!(repo.state(), git2::RepositoryState::Clean);
    assert!(
        dir.path().join(".git/rebase-merge").exists()
            || dir.path().join(".git/rebase-apply").exists(),
        "Expected git rebase state to remain after main sync conflict"
    );
    assert!(repo.find_branch("feature-a", BranchType::Local).is_ok());
    assert!(repo.head_detached().unwrap());
    assert!(repo.find_branch("main", BranchType::Local).is_ok());
}

#[test]
fn sync_deletes_current_branch_if_merged() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&base],
    );

    // Merge feature-a into main
    run_ok("git", &["checkout", "-f", "main"], dir.path());
    run_ok(
        "git",
        &["merge", "--ff-only", &a_id.to_string()],
        dir.path(),
    );

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    let mut cmd = gits_cmd();
    cmd.arg("sync").current_dir(dir.path()).assert().success();

    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo.find_branch("feature-a", BranchType::Local).is_err());
    assert_eq!(repo.head().unwrap().shorthand(), Some("main"));
}

#[test]
fn sync_no_delete_flag_works() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&base],
    );

    // Merge feature-a into main
    run_ok("git", &["checkout", "-f", "main"], dir.path());
    run_ok(
        "git",
        &["merge", "--ff-only", &a_id.to_string()],
        dir.path(),
    );

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .arg("--no-delete")
        .current_dir(dir.path())
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo.find_branch("feature-a", BranchType::Local).is_ok());
}

#[test]
fn sync_refuses_to_delete_branch_checked_out_in_other_worktree() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let _base = repo.find_commit(base_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&repo.find_commit(base_id).unwrap()],
    );

    let _b_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feature b",
        &[&repo.find_commit(a_id).unwrap()],
    );

    // Merge feature-a into main
    run_ok("git", &["checkout", "-f", "main"], dir.path());
    run_ok("git", &["merge", "--ff-only", "feature-a"], dir.path());

    // Create a worktree for feature-a
    let wt_dir = tempdir().unwrap();
    run_ok(
        "git",
        &[
            "worktree",
            "add",
            wt_dir.path().to_str().unwrap(),
            "feature-a",
        ],
        dir.path(),
    );

    run_ok("git", &["checkout", "-f", "feature-b"], dir.path());

    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("is checked out in"));

    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo.find_branch("feature-a", BranchType::Local).is_ok());

    // Force should proceed but git branch -D will still warn and skip deletion
    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .arg("--force")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Warning: Failed to delete merged branch: feature-a",
        ));

    let repo = Repository::open(dir.path()).unwrap();
    // It remains because git refused to delete it even with -D (it's checked out in another worktree)
    assert!(repo.find_branch("feature-a", BranchType::Local).is_ok());
}

#[test]
fn sync_on_main_does_not_delete_branch_when_only_tip_patch_matches() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "shared.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    let feature_first_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "feature-only.txt",
        "only on feature",
        "feature-only change",
        &[&base],
    );
    let feature_first = repo.find_commit(feature_first_id).unwrap();
    make_commit(
        &repo,
        "refs/heads/feature-a",
        "shared.txt",
        "tip patch",
        "feature tip patch",
        &[&feature_first],
    );

    make_commit(
        &repo,
        "refs/heads/main",
        "shared.txt",
        "tip patch",
        "main matches only tip patch",
        &[&base],
    );

    run_ok("git", &["checkout", "-f", "main"], dir.path());

    let mut cmd = gits_cmd();
    cmd.arg("sync").current_dir(dir.path()).assert().success();

    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo.find_branch("feature-a", BranchType::Local).is_ok());
}

#[test]
fn sync_falls_back_to_local_upstream_on_deletion() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let remote_dir = dir.path().join("remote.git");
    fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        dir.path(),
    );

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    run_ok("git", &["push", "-u", "origin", "main:main"], dir.path());

    let feature_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&repo.find_commit(base_id).unwrap()],
    );
    run_ok(
        "git",
        &["push", "origin", "feature-a:feature-a"],
        dir.path(),
    );

    // Merge feature-a into main on "remote"
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
    run_ok("git", &["fetch", "origin"], remote_worktree.path());
    run_ok(
        "git",
        &["merge", "--ff-only", &feature_id.to_string()],
        remote_worktree.path(),
    );
    run_ok("git", &["push", "origin", "main"], remote_worktree.path());

    // Local main is still at base_id, origin/main is advanced.
    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    let mut cmd = gits_cmd();
    cmd.arg("sync").current_dir(dir.path()).assert().success();

    let repo = Repository::open(dir.path()).unwrap();
    assert!(repo.find_branch("feature-a", BranchType::Local).is_err());
    // Should be on local main, not origin/main (detached)
    assert_eq!(repo.head().unwrap().shorthand(), Some("main"));
    assert!(!repo.head_detached().unwrap());
}

#[test]
fn sync_does_not_delete_branch_with_only_tree_match() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "base.txt",
        "base",
        "base commit",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    // Create feature-a: add a file
    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "feature a",
        &[&base],
    );
    let _a = repo.find_commit(a_id).unwrap();

    // Revert it on feature-a: tree becomes same as base
    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    run_ok("git", &["rm", "a.txt"], dir.path());
    run_ok("git", &["commit", "-m", "revert a"], dir.path());

    // feature-a tip tree is now same as main tip tree.
    // But it's not merged.

    run_ok("git", &["checkout", "-f", "main"], dir.path());
    let _other_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "feature b",
        &[&base],
    );

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    let mut cmd = gits_cmd();
    cmd.arg("sync").current_dir(dir.path()).assert().success();

    let repo = Repository::open(dir.path()).unwrap();
    // feature-a should NOT be deleted even though its tree matches main
    assert!(repo.find_branch("feature-a", BranchType::Local).is_ok());
}
