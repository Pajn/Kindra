mod common;

use common::{gits_cmd, make_commit, run_ok};
use git2::{BranchType, Repository};
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

#[test]
fn restack_handles_rebased_lower_branch() {
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
    cmd.arg("restack")
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
fn restack_handles_squashed_lower_branch() {
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
    cmd.arg("restack")
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
fn restack_handles_merged_lower_branch() {
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
    cmd.arg("restack")
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
fn restack_rebases_onto_remote_tracking_base_when_local_base_is_stale() {
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
    cmd.arg("restack")
        .current_dir(dir.path())
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();
    let origin_main_after_restack = repo.revparse_single("origin/main").unwrap().id();
    let feature_after = repo
        .find_branch("feature-a", BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let feature_after_commit = repo.find_commit(feature_after).unwrap();

    assert_ne!(origin_main_before, origin_main_after_restack);
    assert_ne!(feature_after, feature_before);
    assert!(
        repo.graph_descendant_of(feature_after, origin_main_after_restack)
            .unwrap()
    );
    assert_eq!(
        feature_after_commit.parent_id(0).unwrap(),
        origin_main_after_restack
    );
}

#[test]
fn restack_treats_slashed_base_branch_name_as_local_before_remote() {
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
    cmd.arg("restack")
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
fn restack_reports_rebase_conflict() {
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
    cmd.arg("restack")
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
fn restack_refuses_when_git_rebase_in_progress() {
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
    cmd.arg("restack")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("rebase").or(predicate::str::contains("in progress")));
}

#[test]
fn restack_refuses_to_auto_pick_tip_in_non_interactive_mode() {
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
    cmd.arg("restack")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("Multiple stack tips found"));
}

#[cfg(unix)]
#[test]
fn restack_errors_when_git_too_old_for_update_refs() {
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
    cmd.arg("restack")
        .current_dir(dir.path())
        .env("PATH", new_path)
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("restack requires Git >= 2.38.0")
                .and(predicate::str::contains("--update-refs")),
        );
}

#[cfg(unix)]
#[test]
fn restack_checkout_error_includes_branch_name() {
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
    cmd.arg("restack")
        .current_dir(dir.path())
        .env("PATH", new_path)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "git checkout failed for branch 'feature-b'",
        ));
}
