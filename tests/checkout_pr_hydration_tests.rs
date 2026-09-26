#![cfg(unix)]

mod common;

use common::{current_branch, kin_cmd, repo_init, run_ok};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn configure_repo(path: &Path) {
    run_ok("git", &["config", "user.name", "Test User"], path);
    run_ok("git", &["config", "user.email", "test@example.com"], path);
}

fn write_file(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
}

fn commit_all(path: &Path, message: &str) {
    run_ok("git", &["add", "."], path);
    run_ok("git", &["commit", "-m", message], path);
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }
}

fn write_gh_mock(repo_path: &Path, script: &str) {
    let gh_mock = repo_path.join("gh");
    let script = script.replacen("#!/bin/sh\n", "#!/bin/sh\nif [ \"$1\" = \"repo\" ] && [ \"$2\" = \"view\" ]; then printf '{\"url\":\"https://github.com/test/project\"}'; exit 0; fi\n", 1);
    fs::write(&gh_mock, script).unwrap();
    make_executable(&gh_mock);
}

fn mocked_path(repo_path: &Path) -> std::ffi::OsString {
    let mut paths = vec![repo_path.to_path_buf()];
    paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    env::join_paths(paths).unwrap()
}

fn create_bare_remote(repo_path: &Path) -> PathBuf {
    let remote_dir = repo_path.join("remote.git");
    fs::create_dir_all(&remote_dir).unwrap();
    run_ok("git", &["init", "--bare"], &remote_dir);
    run_ok(
        "git",
        &["remote", "add", "origin", remote_dir.to_str().unwrap()],
        repo_path,
    );
    run_ok(
        "git",
        &[
            "config",
            &format!("url.{}.insteadOf", remote_dir.display()),
            "https://github.com/test/project.git",
        ],
        repo_path,
    );
    run_ok(
        "git",
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/test/project.git",
        ],
        repo_path,
    );
    remote_dir
}

fn rev_parse(path: &Path, reference: &str) -> String {
    let output = std::process::Command::new("git")
        .args(["rev-parse", reference])
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git rev-parse failed for {}\nstdout:\n{}\nstderr:\n{}",
        reference,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn local_branch_exists(path: &Path, branch: &str) -> bool {
    std::process::Command::new("git")
        .args(["show-ref", "--verify", &format!("refs/heads/{branch}")])
        .current_dir(path)
        .output()
        .unwrap()
        .status
        .success()
}

#[test]
fn checkout_branch_hydrates_ancestors_and_descendants() {
    let dir = tempdir().unwrap();
    let _repo = repo_init(dir.path());
    configure_repo(dir.path());

    write_file(&dir.path().join("README.md"), "main\n");
    commit_all(dir.path(), "initial");

    run_ok("git", &["checkout", "-b", "feature-a"], dir.path());
    write_file(&dir.path().join("a.txt"), "a\n");
    commit_all(dir.path(), "feature-a");

    run_ok("git", &["checkout", "-b", "feature-b"], dir.path());
    write_file(&dir.path().join("b.txt"), "b\n");
    commit_all(dir.path(), "feature-b");

    run_ok("git", &["checkout", "-b", "feature-c"], dir.path());
    write_file(&dir.path().join("c.txt"), "c\n");
    commit_all(dir.path(), "feature-c");

    run_ok("git", &["checkout", "feature-b"], dir.path());
    run_ok("git", &["checkout", "-b", "feature-d"], dir.path());
    write_file(&dir.path().join("d.txt"), "d\n");
    commit_all(dir.path(), "feature-d");

    run_ok("git", &["checkout", "feature-c"], dir.path());
    run_ok("git", &["checkout", "-b", "feature-e"], dir.path());
    write_file(&dir.path().join("e.txt"), "e\n");
    commit_all(dir.path(), "feature-e");

    create_bare_remote(dir.path());
    run_ok(
        "git",
        &[
            "push",
            "-u",
            "origin",
            "main",
            "feature-a",
            "feature-b",
            "feature-c",
            "feature-d",
            "feature-e",
        ],
        dir.path(),
    );

    run_ok("git", &["checkout", "feature-a"], dir.path());
    write_file(&dir.path().join("local.txt"), "local work\n");
    run_ok("git", &["add", "local.txt"], dir.path());
    run_ok("git", &["commit", "-m", "local work"], dir.path());
    let feature_a_before = rev_parse(dir.path(), "feature-a");

    run_ok("git", &["checkout", "main"], dir.path());
    run_ok("git", &["branch", "-D", "feature-b"], dir.path());
    run_ok("git", &["branch", "-D", "feature-c"], dir.path());
    run_ok("git", &["branch", "-D", "feature-d"], dir.path());
    run_ok("git", &["branch", "-D", "feature-e"], dir.path());

    write_gh_mock(
        dir.path(),
        r#"#!/bin/sh
if [ "$1" = "auth" ] && [ "$2" = "token" ]; then exit 0; fi
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  printf '%s\n' '[{"number": 1, "headRefName": "feature-a", "baseRefName": "main"}, {"number": 2, "headRefName": "feature-b", "baseRefName": "feature-a"}, {"number": 3, "headRefName": "feature-c", "baseRefName": "feature-b"}, {"number": 4, "headRefName": "feature-d", "baseRefName": "feature-b"}, {"number": 5, "headRefName": "feature-e", "baseRefName": "feature-c"}, {"number": 6, "headRefName": "fork-child", "baseRefName": "feature-b", "isCrossRepository": true}, {"number": 7, "headRefName": "sibling", "baseRefName": "feature-a"}]'
  exit 0
fi
exit 1
"#,
    );

    kin_cmd()
        .args(["checkout", "feature-b"])
        .current_dir(dir.path())
        .env("PATH", mocked_path(dir.path()))
        .assert()
        .success();

    assert_eq!(current_branch(dir.path()), "feature-b");
    assert_eq!(rev_parse(dir.path(), "feature-a"), feature_a_before);
    for branch in ["feature-b", "feature-c", "feature-d", "feature-e"] {
        assert!(
            local_branch_exists(dir.path(), branch),
            "expected local branch {} to exist",
            branch
        );
        assert_eq!(
            rev_parse(dir.path(), &format!("{branch}@{{upstream}}")),
            rev_parse(dir.path(), &format!("origin/{branch}"))
        );
    }
    assert!(!local_branch_exists(dir.path(), "fork-child"));
    assert!(!local_branch_exists(dir.path(), "sibling"));
}

#[test]
fn checkout_branch_without_pr_still_hydrates_children() {
    let dir = tempdir().unwrap();
    let _repo = repo_init(dir.path());
    configure_repo(dir.path());

    write_file(&dir.path().join("README.md"), "main\n");
    commit_all(dir.path(), "initial");

    run_ok("git", &["checkout", "-b", "standalone"], dir.path());
    write_file(&dir.path().join("standalone.txt"), "standalone\n");
    commit_all(dir.path(), "standalone");

    run_ok("git", &["checkout", "-b", "standalone-child"], dir.path());
    write_file(&dir.path().join("standalone-child.txt"), "child\n");
    commit_all(dir.path(), "standalone child");

    create_bare_remote(dir.path());
    run_ok(
        "git",
        &[
            "push",
            "-u",
            "origin",
            "main",
            "standalone",
            "standalone-child",
        ],
        dir.path(),
    );

    run_ok("git", &["checkout", "main"], dir.path());
    run_ok("git", &["branch", "-D", "standalone"], dir.path());
    run_ok("git", &["branch", "-D", "standalone-child"], dir.path());

    write_gh_mock(
        dir.path(),
        r#"#!/bin/sh
if [ "$1" = "auth" ] && [ "$2" = "token" ]; then exit 0; fi
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  printf '%s\n' '[{"number": 1, "headRefName": "standalone-child", "baseRefName": "standalone"}]'
  exit 0
fi
exit 1
"#,
    );

    kin_cmd()
        .args(["co", "standalone"])
        .current_dir(dir.path())
        .env("PATH", mocked_path(dir.path()))
        .assert()
        .success();

    assert_eq!(current_branch(dir.path()), "standalone");
    assert!(local_branch_exists(dir.path(), "standalone-child"));
}

#[test]
fn checkout_branch_fails_when_discovered_remote_branch_is_missing() {
    let dir = tempdir().unwrap();
    let _repo = repo_init(dir.path());
    configure_repo(dir.path());

    write_file(&dir.path().join("README.md"), "main\n");
    commit_all(dir.path(), "initial");

    run_ok("git", &["checkout", "-b", "feature-a"], dir.path());
    write_file(&dir.path().join("a.txt"), "a\n");
    commit_all(dir.path(), "feature-a");

    create_bare_remote(dir.path());
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a"],
        dir.path(),
    );
    run_ok("git", &["checkout", "main"], dir.path());

    write_gh_mock(
        dir.path(),
        r#"#!/bin/sh
if [ "$1" = "auth" ] && [ "$2" = "token" ]; then exit 0; fi
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  printf '%s\n' '[{"number": 1, "headRefName": "feature-a", "baseRefName": "ghost-base"}]'
  exit 0
fi
exit 1
"#,
    );

    kin_cmd()
        .args(["checkout", "feature-a"])
        .current_dir(dir.path())
        .env("PATH", mocked_path(dir.path()))
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "No remote-tracking branch found for discovered stack branch 'ghost-base'",
        ));

    assert_eq!(current_branch(dir.path()), "main");
}

#[test]
fn checkout_branch_fails_on_pr_cycle_without_switching() {
    let dir = tempdir().unwrap();
    let _repo = repo_init(dir.path());
    configure_repo(dir.path());

    write_file(&dir.path().join("README.md"), "main\n");
    commit_all(dir.path(), "initial");

    run_ok("git", &["checkout", "-b", "feature-a"], dir.path());
    write_file(&dir.path().join("a.txt"), "a\n");
    commit_all(dir.path(), "feature-a");

    run_ok("git", &["checkout", "-b", "feature-b"], dir.path());
    write_file(&dir.path().join("b.txt"), "b\n");
    commit_all(dir.path(), "feature-b");

    create_bare_remote(dir.path());
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a", "feature-b"],
        dir.path(),
    );
    run_ok("git", &["checkout", "main"], dir.path());

    write_gh_mock(
        dir.path(),
        r#"#!/bin/sh
if [ "$1" = "auth" ] && [ "$2" = "token" ]; then exit 0; fi
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  printf '%s\n' '[{"number": 1, "headRefName": "feature-a", "baseRefName": "feature-b"}, {"number": 2, "headRefName": "feature-b", "baseRefName": "feature-a"}]'
  exit 0
fi
exit 1
"#,
    );

    kin_cmd()
        .args(["checkout", "feature-a"])
        .current_dir(dir.path())
        .env("PATH", mocked_path(dir.path()))
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Detected PR base cycle: feature-a -> feature-b -> feature-a",
        ));

    assert_eq!(current_branch(dir.path()), "main");
}

#[test]
fn checkout_branch_reports_gh_auth_failure() {
    let dir = tempdir().unwrap();
    let _repo = repo_init(dir.path());
    configure_repo(dir.path());

    write_file(&dir.path().join("README.md"), "main\n");
    commit_all(dir.path(), "initial");

    run_ok("git", &["checkout", "-b", "feature-a"], dir.path());
    write_file(&dir.path().join("a.txt"), "a\n");
    commit_all(dir.path(), "feature-a");

    create_bare_remote(dir.path());
    run_ok(
        "git",
        &["push", "-u", "origin", "main", "feature-a"],
        dir.path(),
    );
    run_ok("git", &["checkout", "main"], dir.path());

    write_gh_mock(
        dir.path(),
        r#"#!/bin/sh
if [ "$1" = "auth" ] && [ "$2" = "token" ]; then
  exit 1
fi
printf '%s\n' "unexpected gh invocation: $*" >&2
exit 1
"#,
    );

    kin_cmd()
        .args(["checkout", "feature-a"])
        .current_dir(dir.path())
        .env("PATH", mocked_path(dir.path()))
        .assert()
        .failure()
        .stderr(predicates::str::contains("gh auth login"));

    assert_eq!(current_branch(dir.path()), "main");
}

#[test]
fn checkout_named_branch_suspends_overrides_and_preserves_staged_edits() {
    let dir = tempdir().unwrap();
    repo_init(dir.path());
    write_file(&dir.path().join("AGENTS.md"), "base\n");
    commit_all(dir.path(), "initial");
    run_ok("git", &["checkout", "-b", "feature"], dir.path());
    write_file(&dir.path().join("AGENTS.md"), "feature\n");
    commit_all(dir.path(), "feature");
    run_ok("git", &["checkout", "main"], dir.path());
    fs::write(
        dir.path().join(".git/apply.sh"),
        "printf 'local override\\n' > AGENTS.md\n",
    )
    .unwrap();
    fs::write(dir.path().join(".git/kindra.toml"), "[overrides]\npaths = ['AGENTS.md']\napply = ['sh \"$(git rev-parse --git-common-dir)/apply.sh\"']\n").unwrap();
    write_file(&dir.path().join("AGENTS.md"), "local override\n");
    run_ok(
        "git",
        &["update-index", "--skip-worktree", "AGENTS.md"],
        dir.path(),
    );
    write_file(&dir.path().join("staged.txt"), "keep staged\n");
    run_ok("git", &["add", "staged.txt"], dir.path());
    write_gh_mock(
        dir.path(),
        r#"#!/bin/sh
if [ "$1" = "auth" ] && [ "$2" = "token" ]; then exit 0; fi
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then printf '[]'; exit 0; fi
exit 1
"#,
    );
    kin_cmd()
        .args(["co", "feature"])
        .current_dir(dir.path())
        .env("PATH", mocked_path(dir.path()))
        .assert()
        .success();
    assert_eq!(current_branch(dir.path()), "feature");
    assert_eq!(
        fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
        "local override\n"
    );
    let repo = git2::Repository::open(dir.path()).unwrap();
    assert!(
        repo.status_file(Path::new("staged.txt"))
            .unwrap()
            .is_index_new()
    );
    assert!(!repo.path().join("kindra_overrides_state.json").exists());
}

#[test]
fn checkout_named_branch_conflicts_with_all() {
    kin_cmd()
        .args(["co", "feature", "--all"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot be used with"));
}

fn hydration_fixture() -> tempfile::TempDir {
    let dir = tempdir().unwrap();
    repo_init(dir.path());
    write_file(&dir.path().join("file.txt"), "main\n");
    commit_all(dir.path(), "initial");
    run_ok("git", &["checkout", "-b", "parent"], dir.path());
    write_file(&dir.path().join("file.txt"), "parent\n");
    commit_all(dir.path(), "parent");
    run_ok("git", &["branch", "child"], dir.path());
    create_bare_remote(dir.path());
    run_ok(
        "git",
        &["push", "origin", "main", "parent", "child"],
        dir.path(),
    );
    run_ok("git", &["checkout", "main"], dir.path());
    run_ok("git", &["branch", "-D", "parent", "child"], dir.path());
    write_gh_mock(
        dir.path(),
        r#"#!/bin/sh
if [ "$1" = "auth" ] && [ "$2" = "token" ]; then exit 0; fi
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  printf '%s' '[{"number":1,"headRefName":"parent","baseRefName":"main"},{"number":2,"headRefName":"child","baseRefName":"parent"}]'
  exit 0
fi
exit 1
"#,
    );
    dir
}

#[test]
fn checkout_uses_the_pr_repository_instead_of_origin() {
    let dir = hydration_fixture();
    run_ok(
        "git",
        &["remote", "rename", "origin", "upstream"],
        dir.path(),
    );
    let fork = dir.path().join(".git/fork.git");
    fs::create_dir(&fork).unwrap();
    run_ok("git", &["init", "--bare"], &fork);
    run_ok(
        "git",
        &[
            "config",
            &format!("url.{}.insteadOf", fork.display()),
            "https://github.com/test/fork.git",
        ],
        dir.path(),
    );
    run_ok(
        "git",
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/test/fork.git",
        ],
        dir.path(),
    );
    run_ok(
        "git",
        &["push", "origin", "main:parent", "main:child"],
        dir.path(),
    );
    kin_cmd()
        .args(["co", "child"])
        .current_dir(dir.path())
        .env("PATH", mocked_path(dir.path()))
        .assert()
        .success();
    assert_eq!(
        rev_parse(dir.path(), "child"),
        rev_parse(dir.path(), "upstream/child")
    );
    assert_eq!(
        rev_parse(dir.path(), "parent"),
        rev_parse(dir.path(), "upstream/parent")
    );
    let repo = git2::Repository::open(dir.path()).unwrap();
    assert_eq!(
        repo.find_branch("child", git2::BranchType::Local)
            .unwrap()
            .upstream()
            .unwrap()
            .name()
            .unwrap(),
        Some("upstream/child")
    );
}

#[test]
fn checkout_rejects_unidentified_pr_repository() {
    let dir = hydration_fixture();
    // The refs exist, but no configured remote identifies the selected repository.
    let remote = dir.path().join("remote.git");
    run_ok(
        "git",
        &["remote", "set-url", "origin", remote.to_str().unwrap()],
        dir.path(),
    );
    kin_cmd()
        .args(["co", "child"])
        .current_dir(dir.path())
        .env("PATH", mocked_path(dir.path()))
        .assert()
        .failure()
        .stderr(predicates::str::contains("PR repository"));
    assert!(!local_branch_exists(dir.path(), "parent"));
    assert_eq!(current_branch(dir.path()), "main");
}

#[test]
fn checkout_normalizes_remote_qualified_branch_before_hydrating() {
    let dir = hydration_fixture();
    kin_cmd()
        .args(["co", "origin/parent"])
        .current_dir(dir.path())
        .env("PATH", mocked_path(dir.path()))
        .assert()
        .success();
    assert_eq!(current_branch(dir.path()), "parent");
    assert!(local_branch_exists(dir.path(), "child"));
    assert!(!local_branch_exists(dir.path(), "origin/parent"));
}

#[test]
fn checkout_checkpoints_partial_hydration_and_continues() {
    let dir = hydration_fixture();
    let lock = dir.path().join(".git/refs/heads/child.lock");
    fs::write(&lock, "locked").unwrap();
    kin_cmd()
        .args(["co", "child"])
        .current_dir(dir.path())
        .env("PATH", mocked_path(dir.path()))
        .assert()
        .failure();
    assert!(local_branch_exists(dir.path(), "parent"));
    assert!(!local_branch_exists(dir.path(), "child"));
    let path = dir.path().join(".git/kindra_checkout_state.json");
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(state["branch"], "child");
    let steps = state["steps"].as_array().unwrap();
    assert_eq!(
        steps
            .iter()
            .find(|step| step["branch"] == "parent")
            .unwrap()["completed"],
        true
    );
    assert_eq!(
        steps.iter().find(|step| step["branch"] == "child").unwrap()["completed"],
        false
    );
    kin_cmd()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("Checkout"));
    kin_cmd()
        .args(["co", "main"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("in progress"));
    kin_cmd()
        .args(["commit", "-m", "blocked"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("in progress"));
    fs::remove_file(lock).unwrap();
    // Recovery uses the recorded plan without another GitHub query.
    fs::remove_file(dir.path().join("gh")).unwrap();
    kin_cmd()
        .arg("continue")
        .current_dir(dir.path())
        .assert()
        .success();
    assert_eq!(current_branch(dir.path()), "child");
    assert!(!path.exists());
}

#[test]
fn checkout_abort_clears_partial_hydration_without_deleting_local_work() {
    let dir = hydration_fixture();
    fs::write(dir.path().join(".git/refs/heads/child.lock"), "locked").unwrap();
    kin_cmd()
        .args(["co", "child"])
        .current_dir(dir.path())
        .env("PATH", mocked_path(dir.path()))
        .assert()
        .failure();
    let path = dir.path().join(".git/kindra_checkout_state.json");
    assert!(path.exists());
    kin_cmd()
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();
    assert!(!path.exists());
    assert!(local_branch_exists(dir.path(), "parent"));
    assert_eq!(current_branch(dir.path()), "main");
}

#[test]
fn interrupted_hydration_keeps_overrides_applied_until_it_checks_out() {
    for recovery in ["continue", "abort"] {
        let dir = hydration_fixture();
        fs::write(
            dir.path().join(".git/apply.sh"),
            "printf 'overlay\\n' > file.txt\n",
        )
        .unwrap();
        fs::write(dir.path().join(".git/kindra.toml"), "[overrides]\npaths = ['file.txt']\napply = ['sh \"$(git rev-parse --git-common-dir)/apply.sh\"']\n").unwrap();
        write_file(&dir.path().join("file.txt"), "overlay\n");
        run_ok(
            "git",
            &["update-index", "--skip-worktree", "file.txt"],
            dir.path(),
        );
        let lock = dir.path().join(".git/refs/heads/child.lock");
        fs::write(&lock, "locked").unwrap();
        kin_cmd()
            .args(["co", "child"])
            .current_dir(dir.path())
            .env("PATH", mocked_path(dir.path()))
            .assert()
            .failure();
        // Branch creation stopped before the checkout that changes file.txt.
        let overlay_state = dir.path().join(".git/kindra_overrides_state.json");
        assert!(!overlay_state.exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("file.txt")).unwrap(),
            "overlay\n"
        );
        fs::remove_file(lock).unwrap();
        kin_cmd()
            .arg(recovery)
            .current_dir(dir.path())
            .assert()
            .success();
        assert_eq!(
            fs::read_to_string(dir.path().join("file.txt")).unwrap(),
            "overlay\n"
        );
        assert!(!overlay_state.exists());
        assert!(!dir.path().join(".git/kindra_checkout_state.json").exists());
    }
}

#[test]
fn checkout_continue_retries_uncheckpointed_creation_without_overwriting_edits() {
    for changed in [false, true] {
        let dir = hydration_fixture();
        let lock = dir.path().join(".git/refs/heads/child.lock");
        fs::write(&lock, "locked").unwrap();
        kin_cmd()
            .args(["co", "child"])
            .current_dir(dir.path())
            .env("PATH", mocked_path(dir.path()))
            .assert()
            .failure();
        let path = dir.path().join(".git/kindra_checkout_state.json");
        let mut state: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        // Simulate a crash after creating parent but before its checkpoint.
        for step in state["steps"].as_array_mut().unwrap() {
            if step["branch"] == "parent" {
                step["completed"] = false.into();
            }
        }
        fs::write(&path, serde_json::to_string(&state).unwrap()).unwrap();
        fs::remove_file(lock).unwrap();
        if changed {
            run_ok("git", &["branch", "-f", "parent", "main"], dir.path());
            kin_cmd()
                .arg("continue")
                .current_dir(dir.path())
                .assert()
                .failure()
                .stderr(predicates::str::contains("refusing to overwrite"));
            assert_eq!(
                rev_parse(dir.path(), "parent"),
                rev_parse(dir.path(), "main")
            );
            assert!(path.exists());
        } else {
            kin_cmd()
                .arg("continue")
                .current_dir(dir.path())
                .assert()
                .success();
            assert_eq!(current_branch(dir.path()), "child");
            assert!(!path.exists());
        }
    }
}

#[test]
fn checkout_recovery_preserves_native_git_operation() {
    let dir = hydration_fixture();
    let lock = dir.path().join(".git/refs/heads/child.lock");
    fs::write(lock, "locked").unwrap();
    kin_cmd()
        .args(["co", "child"])
        .current_dir(dir.path())
        .env("PATH", mocked_path(dir.path()))
        .assert()
        .failure();
    let merge_head = dir.path().join(".git/MERGE_HEAD");
    fs::write(&merge_head, rev_parse(dir.path(), "main")).unwrap();
    kin_cmd()
        .arg("continue")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("native git operation"));
    assert!(dir.path().join(".git/kindra_checkout_state.json").exists());
    kin_cmd()
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();
    assert!(merge_head.exists());
    assert!(!dir.path().join(".git/kindra_checkout_state.json").exists());
}

#[test]
fn checkout_identifies_scp_and_ssh_remote_urls() {
    for url in [
        "github.com:test/project.git",
        "git@github.com:test/project.git",
        "ssh://git@github.com/test/project.git",
    ] {
        let dir = hydration_fixture();
        let remote = dir.path().join("remote.git");
        run_ok(
            "git",
            &[
                "config",
                "--add",
                &format!("url.{}.insteadOf", remote.display()),
                url,
            ],
            dir.path(),
        );
        run_ok("git", &["remote", "set-url", "origin", url], dir.path());
        kin_cmd()
            .args(["co", "child"])
            .current_dir(dir.path())
            .env("PATH", mocked_path(dir.path()))
            .assert()
            .success();
        assert_eq!(
            rev_parse(dir.path(), "child"),
            rev_parse(dir.path(), "origin/child")
        );
    }
}
