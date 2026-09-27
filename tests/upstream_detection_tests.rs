mod common;

use common::{
    add_linked_worktree, kin_cmd, make_commit, repo_config_path, repo_init, write_repo_config,
};
use git2::Repository;
use kindra::commands::find_upstream;
use predicates::str::contains;
use std::fs;
use tempfile::tempdir;

fn setup_repo_with_base(base_branch: &str) -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let refname = format!("refs/heads/{base_branch}");
    make_commit(&repo, &refname, "file.txt", "initial", "initial", &[]);
    repo.set_head(&refname).unwrap();

    (dir, repo)
}

#[test]
fn sync_allows_trunk_when_main_master_missing() {
    let (dir, repo) = setup_repo_with_base("trunk");
    repo.set_head("refs/heads/trunk").unwrap();

    let mut cmd = kin_cmd();
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

    let mut cmd = kin_cmd();
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
        repo.path().join("kindra.toml"),
        r#"upstream_branch = "develop""#,
    )
    .unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("sync").current_dir(dir.path()).assert().success();
}

/// Repository config lives in the common Git directory, so a trunk configured
/// there must be honoured from linked worktrees too — and a stray
/// `kindra.toml` in a linked worktree's private Git directory must not be.
#[test]
fn repo_trunk_override_applies_in_linked_worktrees() {
    let (dir, repo) = setup_repo_with_base("dev");
    let dev_tip = repo
        .revparse_single("dev")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    make_commit(
        &repo,
        "refs/heads/feature",
        "feature.txt",
        "feature",
        "feature work",
        &[&dev_tip],
    );
    write_repo_config(dir.path(), r#"upstream_branch = "dev""#);
    let (_wt_parent, wt) = add_linked_worktree(dir.path(), "feature");
    let linked = Repository::open(&wt).unwrap();
    fs::write(
        linked.path().join("kindra.toml"),
        r#"upstream_branch = "nonexistent""#,
    )
    .unwrap();

    for cwd in [dir.path(), wt.as_path()] {
        let output = kin_cmd().arg("tree").current_dir(cwd).output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "kin tree failed in {}\nstdout:\n{stdout}\nstderr:\n{}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            stdout.lines().next().unwrap_or("").contains("dev"),
            "the configured trunk should be the tree root in {}:\n{stdout}",
            cwd.display(),
        );
        assert!(stdout.contains("feature"), "missing feature:\n{stdout}");
    }

    kin_cmd().arg("sync").current_dir(&wt).assert().success();
}

/// A malformed repository config fails every worktree identically, naming the
/// shared config file, instead of being ignored in linked worktrees.
#[test]
fn malformed_repo_config_fails_the_same_in_every_worktree() {
    let (dir, repo) = setup_repo_with_base("main");
    let main_tip = repo
        .revparse_single("main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    make_commit(
        &repo,
        "refs/heads/feature",
        "feature.txt",
        "feature",
        "feature work",
        &[&main_tip],
    );
    write_repo_config(dir.path(), "upstream_branch = \n");
    let (_wt_parent, wt) = add_linked_worktree(dir.path(), "feature");

    let stderr_in = |cwd: &std::path::Path| {
        let output = kin_cmd().arg("tree").current_dir(cwd).output().unwrap();
        assert!(
            !output.status.success(),
            "kin tree should fail on malformed config in {}\nstdout:\n{}",
            cwd.display(),
            String::from_utf8_lossy(&output.stdout),
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    };
    let main_stderr = stderr_in(dir.path());
    let linked_stderr = stderr_in(&wt);
    assert!(
        main_stderr.contains(&format!(
            "Failed to parse repository config at {}",
            repo_config_path(dir.path()).display()
        )),
        "unexpected error:\n{main_stderr}"
    );
    assert_eq!(main_stderr, linked_stderr);
}

/// Unknown top-level keys are reported, not fatal, so a typo is visible while
/// the rest of the file still applies. Keys owned by Kindra modules are not
/// reported.
#[test]
fn unknown_repo_config_keys_warn_without_failing() {
    let (dir, repo) = setup_repo_with_base("main");
    let main_tip = repo
        .revparse_single("main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    repo.branch("develop", &main_tip, false).unwrap();
    repo.set_head("refs/heads/develop").unwrap();
    write_repo_config(
        dir.path(),
        "upstream_branch = \"develop\"\nupstream_brnch = \"main\"\n\n\
         [rebase]\nautostash = false\n\n[restack]\nhistory_limit = 10\n\n\
         [worktrees]\ntrunk = \"develop\"\n\n[restak]\nhistory_limit = 1\n",
    );

    let output = kin_cmd()
        .arg("tree")
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "kin tree failed:\n{stderr}");
    assert!(
        stdout.starts_with("develop"),
        "known keys should still apply:\n{stdout}"
    );
    let path = repo_config_path(dir.path());
    for key in ["upstream_brnch", "restak"] {
        assert!(
            stderr.contains(&format!(
                "warning: ignoring unknown key `{key}` in repository config at {}",
                path.display()
            )),
            "missing warning for `{key}`:\n{stderr}"
        );
    }
    assert_eq!(
        stderr.matches("warning:").count(),
        2,
        "only unknown keys should be reported:\n{stderr}"
    );
}

#[test]
fn sync_errors_when_repo_override_branch_missing() {
    let (dir, repo) = setup_repo_with_base("main");

    fs::write(
        repo.path().join("kindra.toml"),
        r#"upstream_branch = "nonexistent""#,
    )
    .unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("sync")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(contains(format!(
            "Configured upstream branch 'nonexistent' in {} was not found",
            repo_config_path(dir.path()).display()
        )));
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
        repo.path().join("kindra.toml"),
        r#"upstream_branch = "not-a-branch""#,
    )
    .unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("sync")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(contains(format!(
            "Configured upstream branch 'not-a-branch' in {} was not found",
            repo_config_path(dir.path()).display()
        )));
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

    let upstream = find_upstream(&repo).unwrap().unwrap();
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
        repo.path().join("kindra.toml"),
        r#"upstream_branch = "feature/base""#,
    )
    .unwrap();

    let upstream = find_upstream(&repo).unwrap().unwrap();
    assert_eq!(upstream, "origin/feature/base");
}
