mod common;

use common::{gits_cmd, make_commit};
use git2::Repository;
use gits::commands::find_upstream;
use predicates::str::contains;
use std::fs;
use tempfile::tempdir;

fn setup_repo_with_base(base_branch: &str) -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let refname = format!("refs/heads/{base_branch}");
    make_commit(&repo, &refname, "file.txt", "initial", "initial", &[]);
    repo.set_head(&refname).unwrap();

    (dir, repo)
}

#[test]
fn sync_allows_trunk_when_main_master_missing() {
    let (dir, repo) = setup_repo_with_base("trunk");
    repo.set_head("refs/heads/trunk").unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("sync").current_dir(dir.path()).assert().success();
}

#[test]
fn sync_allows_init_default_branch_when_checked_out() {
    let (dir, repo) = setup_repo_with_base("main");
    let main_tip = repo
        .revparse_single("main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    repo.branch("trunk", &main_tip, false).unwrap();
    repo.set_head("refs/heads/trunk").unwrap();

    let mut cfg = repo.config().unwrap();
    cfg.set_str("init.defaultBranch", "trunk").unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("sync").current_dir(dir.path()).assert().success();
}

#[test]
fn sync_allows_repo_override_from_git_dir_config() {
    let (dir, repo) = setup_repo_with_base("main");
    let main_tip = repo
        .revparse_single("main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    repo.branch("develop", &main_tip, false).unwrap();
    repo.set_head("refs/heads/develop").unwrap();

    fs::write(
        repo.path().join("gits.toml"),
        r#"upstream_branch = "develop""#,
    )
    .unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("sync").current_dir(dir.path()).assert().success();
}

#[test]
fn sync_errors_when_repo_override_branch_missing() {
    let (dir, repo) = setup_repo_with_base("main");

    fs::write(
        repo.path().join("gits.toml"),
        r#"upstream_branch = "nonexistent""#,
    )
    .unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(contains(
            "Configured upstream branch 'nonexistent' in .git/gits.toml was not found",
        ));
}

#[test]
fn sync_errors_when_repo_override_is_not_a_branch() {
    let (dir, repo) = setup_repo_with_base("main");
    let main_tip = repo
        .revparse_single("main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    repo.tag_lightweight("not-a-branch", main_tip.as_object(), false)
        .unwrap();

    fs::write(
        repo.path().join("gits.toml"),
        r#"upstream_branch = "not-a-branch""#,
    )
    .unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("sync")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(contains(
            "Configured upstream branch 'not-a-branch' in .git/gits.toml was not found",
        ));
}

#[test]
fn upstream_detection_slash_default_branch_exists_only_remotely() {
    let (_dir, repo) = setup_repo_with_base("work");

    let work_tip = repo
        .revparse_single("work")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    repo.reference(
        "refs/remotes/origin/feature/base",
        work_tip,
        true,
        "test remote default branch",
    )
    .unwrap();

    let mut cfg = repo.config().unwrap();
    cfg.set_str("init.defaultBranch", "feature/base").unwrap();

    let upstream = find_upstream(&repo).unwrap();
    assert_eq!(upstream, "origin/feature/base");
}

#[test]
fn upstream_override_slash_branch_exists_only_remotely() {
    let (_dir, repo) = setup_repo_with_base("work");

    let work_tip = repo
        .revparse_single("work")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    repo.reference(
        "refs/remotes/origin/feature/base",
        work_tip,
        true,
        "test remote override branch",
    )
    .unwrap();

    fs::write(
        repo.path().join("gits.toml"),
        r#"upstream_branch = "feature/base""#,
    )
    .unwrap();

    let upstream = find_upstream(&repo).unwrap();
    assert_eq!(upstream, "origin/feature/base");
}
