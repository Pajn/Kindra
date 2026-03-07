mod common;
use common::{gits_cmd, make_commit, run_ok};
use git2::Repository;
use std::fs;
use tempfile::tempdir;

fn setup_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let mut parent_id = make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "initial",
        "initial commit",
        &[],
    );

    let first_commit_id = parent_id;

    for i in 1..=3 {
        let parent = repo.find_commit(parent_id).unwrap();
        parent_id = make_commit(
            &repo,
            "refs/heads/temp",
            &format!("file{}.txt", i),
            &format!("content {}", i),
            &format!("commit {}", i),
            &[&parent],
        );
    }
    // Remove the temp branch created by make_commit loop
    repo.find_branch("temp", git2::BranchType::Local)
        .unwrap()
        .delete()
        .unwrap();

    repo.set_head_detached(parent_id).unwrap();

    {
        let first_commit = repo.find_commit(first_commit_id).unwrap();
        repo.branch("main", &first_commit, true).unwrap();
    }

    {
        let head_commit = repo.find_commit(parent_id).unwrap();
        repo.checkout_tree(
            head_commit.as_object(),
            Some(git2::build::CheckoutBuilder::new().force()),
        )
        .unwrap();
    }

    (dir, repo)
}

#[test]
fn test_move_stack() {
    let (dir, repo) = setup_repo();

    let c1_id = repo.revparse_single("HEAD~2").unwrap().id();
    let c2_id = repo.revparse_single("HEAD~1").unwrap().id();
    let c3_id = repo.head().unwrap().peel_to_commit().unwrap().id();

    let c1 = repo.find_commit(c1_id).unwrap();
    let c2 = repo.find_commit(c2_id).unwrap();
    let c3 = repo.find_commit(c3_id).unwrap();

    repo.branch("base", &c1, false).unwrap();
    repo.branch("feature-a", &c2, false).unwrap();
    repo.branch("feature-b", &c3, false).unwrap();
    repo.branch("independent", &c1, false).unwrap();

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_tree(
        c2.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    let mut cmd = gits_cmd();
    cmd.env("TERM", "xterm");
    cmd.arg("move")
        .arg("--onto")
        .arg("independent")
        .current_dir(dir.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    let fa = repo
        .find_branch("feature-a", git2::BranchType::Local)
        .unwrap();
    let indep = repo
        .find_branch("independent", git2::BranchType::Local)
        .unwrap();
    assert!(
        repo.graph_descendant_of(fa.get().target().unwrap(), indep.get().target().unwrap())
            .unwrap()
    );

    let fb = repo
        .find_branch("feature-b", git2::BranchType::Local)
        .unwrap();
    assert!(
        repo.graph_descendant_of(fb.get().target().unwrap(), fa.get().target().unwrap())
            .unwrap()
    );
}

#[test]
fn test_move_restore_checkout_failure() {
    let (dir, repo) = setup_repo();

    let c1_id = repo.revparse_single("HEAD~2").unwrap().id();
    let c1 = repo.find_commit(c1_id).unwrap();
    repo.branch("target", &c1, false).unwrap();

    let head_id = repo.head().unwrap().peel_to_commit().unwrap().id();
    let head = repo.find_commit(head_id).unwrap();
    repo.branch("feature", &head, false).unwrap();
    repo.set_head("refs/heads/feature").unwrap();

    repo.checkout_tree(
        head.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    let git_path = which::which("git").expect("git not found");

    let git_mock = dir.path().join("git");
    fs::write(
        &git_mock,
        format!(
            r#"#!/bin/sh
if [ "$1" = "checkout" ] && [ "$2" = "feature" ]; then
    echo "Mock checkout failure" >&2
    exit 1
fi
exec {} "$@"
"#,
            git_path.to_str().unwrap()
        ),
    )
    .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&git_mock).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&git_mock, perms).unwrap();
    }

    let mut cmd = gits_cmd();
    cmd.env("TERM", "xterm");

    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = dir.path().to_path_buf().into_os_string();
    new_path.push(":");
    new_path.push(old_path);

    cmd.arg("move")
        .arg("--onto")
        .arg("target")
        .current_dir(dir.path())
        .env("PATH", new_path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Failed to checkout back to original branch 'feature'",
        ));
}

#[test]
fn test_move_upstream_error() {
    let (dir, _repo) = setup_repo();

    run_ok("git", &["checkout", "main"], dir.path());

    let mut cmd = gits_cmd();
    cmd.arg("move")
        .arg("--onto")
        .arg("some-branch")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Branch 'main' is the upstream branch. Cannot move the upstream branch itself.",
        ));
}

#[test]
fn test_move_conflict_and_continue() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(&repo, "refs/heads/main", "file.txt", "base", "initial", &[]);
    let base = repo.find_commit(base_id).unwrap();

    let target_id = make_commit(
        &repo,
        "refs/heads/target",
        "file.txt",
        "target content",
        "target commit",
        &[&base],
    );
    let target = repo.find_commit(target_id).unwrap();

    let feature_id = make_commit(
        &repo,
        "refs/heads/feature",
        "file.txt",
        "feature content",
        "feature commit",
        &[&base],
    );
    let feature = repo.find_commit(feature_id).unwrap();

    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_tree(
        feature.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("move")
        .arg("--onto")
        .arg("target")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Resolve conflicts"));

    fs::write(dir.path().join("file.txt"), "resolved content").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());

    let mut cmd_cont = gits_cmd();
    cmd_cont
        .arg("continue")
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .assert()
        .success();

    let feature_new = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap();
    assert!(
        repo.graph_descendant_of(feature_new.get().target().unwrap(), target.id())
            .unwrap()
    );
}

#[test]
fn test_move_abort() {
    let (dir, repo) = setup_repo();

    let c1_id = repo.revparse_single("HEAD~2").unwrap().id();
    let c1 = repo.find_commit(c1_id).unwrap();
    repo.branch("target", &c1, false).unwrap();

    let head_id = repo.head().unwrap().peel_to_commit().unwrap().id();
    let head = repo.find_commit(head_id).unwrap();
    repo.branch("feature", &head, false).unwrap();
    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_tree(
        head.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("move")
        .arg("--onto")
        .arg("target")
        .current_dir(dir.path())
        .assert()
        .success();

    let mut cmd_abort = gits_cmd();
    cmd_abort
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("No operation in progress"));
}

#[test]
fn test_move_all_onto_main() {
    let (dir, repo) = setup_repo();

    let head_id = repo.head().unwrap().peel_to_commit().unwrap().id();
    let head = repo.find_commit(head_id).unwrap();
    repo.branch("feature", &head, false).unwrap();
    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_tree(
        head.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("move")
        .arg("--all")
        .arg("--onto")
        .arg("main")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    let feature = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap();
    let main = repo.find_branch("main", git2::BranchType::Local).unwrap();
    assert!(
        repo.graph_descendant_of(
            feature.get().target().unwrap(),
            main.get().target().unwrap()
        )
        .unwrap()
    );
}

#[test]
fn test_move_all_from_main_error() {
    let (dir, _repo) = setup_repo();

    run_ok("git", &["checkout", "main"], dir.path());

    let mut cmd = gits_cmd();
    cmd.arg("move")
        .arg("--all")
        .arg("--onto")
        .arg("feature-a")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Branch 'main' is the upstream branch. Cannot move the upstream branch itself.",
        ));
}

#[test]
fn test_move_all_between_stacks() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(&repo, "refs/heads/main", "root.txt", "root", "root", &[]);
    let base = repo.find_commit(base_id).unwrap();

    let s1a_id = make_commit(
        &repo,
        "refs/heads/s1-a",
        "s1.txt",
        "s1-a",
        "s1-a commit",
        &[&base],
    );
    let s1a = repo.find_commit(s1a_id).unwrap();

    let s1b_id = make_commit(
        &repo,
        "refs/heads/s1-b",
        "s1_other.txt",
        "s1-b",
        "s1-b commit",
        &[&s1a],
    );
    let _s1b = repo.find_commit(s1b_id).unwrap();

    let s2a_id = make_commit(
        &repo,
        "refs/heads/s2-a",
        "s2.txt",
        "s2-a",
        "s2-a commit",
        &[&base],
    );
    let _s2a = repo.find_commit(s2a_id).unwrap();

    repo.set_head("refs/heads/s1-a").unwrap();
    repo.checkout_tree(
        s1a.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("move")
        .arg("--all")
        .arg("--onto")
        .arg("s2-a")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    let s1a_new = repo.find_branch("s1-a", git2::BranchType::Local).unwrap();
    let s1b_new = repo.find_branch("s1-b", git2::BranchType::Local).unwrap();
    let s2a_ref = repo.find_branch("s2-a", git2::BranchType::Local).unwrap();
    assert!(
        repo.graph_descendant_of(
            s1a_new.get().target().unwrap(),
            s2a_ref.get().target().unwrap()
        )
        .unwrap()
            || s1a_new.get().target().unwrap() == s2a_ref.get().target().unwrap()
    );
    assert!(
        repo.graph_descendant_of(
            s1b_new.get().target().unwrap(),
            s1a_new.get().target().unwrap()
        )
        .unwrap()
            || s1b_new.get().target().unwrap() == s1a_new.get().target().unwrap()
    );
}

#[test]
fn test_move_onto_descendant() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let base_id = make_commit(&repo, "refs/heads/main", "root.txt", "root", "initial", &[]);
    let base = repo.find_commit(base_id).unwrap();

    let fa_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a.txt",
        "a",
        "a commit",
        &[&base],
    );
    let fa = repo.find_commit(fa_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "b commit",
        &[&fa],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_tree(
        fa.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("move")
        .arg("--onto")
        .arg("feature-b")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Target branch 'feature-b' is inside the subtree being moved.",
        ));
}

#[test]
fn test_move_abort_cleans_up_git_rebase() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    // 1. Initial commit
    let base_id = make_commit(&repo, "refs/heads/main", "file.txt", "base", "initial", &[]);
    let base = repo.find_commit(base_id).unwrap();

    // 2. Branch 'target'
    let target_id = make_commit(
        &repo,
        "refs/heads/target",
        "file.txt",
        "target content",
        "target commit",
        &[&base],
    );
    let _target = repo.find_commit(target_id).unwrap();

    // 3. Branch 'feature' (conflicts)
    let feature_id = make_commit(
        &repo,
        "refs/heads/feature",
        "file.txt",
        "feature content",
        "feature commit",
        &[&base],
    );
    let feature = repo.find_commit(feature_id).unwrap();

    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_tree(
        feature.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    // 4. Start move and hit conflict
    let mut cmd = gits_cmd();
    cmd.arg("move")
        .arg("--onto")
        .arg("target")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure();

    // Verify git rebase is in progress
    assert!(
        dir.path().join(".git/rebase-merge").exists()
            || dir.path().join(".git/rebase-apply").exists()
    );

    // 5. Abort move
    let mut cmd_abort = gits_cmd();
    cmd_abort
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();

    // Verify git rebase is ALSO aborted
    assert!(!dir.path().join(".git/rebase-merge").exists());
    assert!(!dir.path().join(".git/rebase-apply").exists());
}

#[test]
fn test_move_abort_preserves_state_on_rebase_abort_failure() {
    let (dir, _repo) = setup_abort_repo();

    // 1. Manually create a gits move state file
    let state_path = dir.path().join(".git/gits_rebase_state.json");
    fs::write(
        &state_path,
        r#"{
  "operation": "Move",
  "original_branch": "feature",
  "target_branch": "target",
  "remaining_branches": [],
  "in_progress_branch": null,
  "parent_id_map": {},
  "parent_name_map": {}
}"#,
    )
    .unwrap();

    // 2. Manually create a rebase-merge directory to simulate an active rebase
    fs::create_dir_all(dir.path().join(".git/rebase-merge")).unwrap();

    // 3. Mock git to fail ONLY on rebase --abort
    let git_path = which::which("git").expect("git not found");
    let git_mock = dir.path().join("git");
    fs::write(
        &git_mock,
        format!(
            r#"#!/bin/sh
if [ "$1" = "rebase" ] && [ "$2" = "--abort" ]; then
    echo "Mock rebase abort failure" >&2
    exit 1
fi
exec {} "$@"
"#,
            git_path.to_str().unwrap()
        ),
    )
    .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&git_mock).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&git_mock, perms).unwrap();
    }

    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = dir.path().to_path_buf().into_os_string();
    new_path.push(":");
    new_path.push(old_path);

    // 4. Run gits move abort - it should fail because rebase --abort failed
    let mut cmd = gits_cmd();
    cmd.arg("abort")
        .current_dir(dir.path())
        .env("PATH", &new_path)
        .assert()
        .failure();

    // 5. Verify state file STILL EXISTS because the abort didn't fully complete
    assert!(
        state_path.exists(),
        "State file should be preserved if git rebase --abort fails"
    );
}

#[test]
fn test_move_conflict_and_continue_no_re_rebase() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    // setup: main (file.txt: base) -> feature (file.txt: feat)
    // setup: target (file.txt: target)
    let base_id = make_commit(&repo, "refs/heads/main", "file.txt", "base", "base", &[]);
    let base = repo.find_commit(base_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/target",
        "file.txt",
        "target",
        "target",
        &[&base],
    );

    let feat_id = make_commit(
        &repo,
        "refs/heads/feature",
        "file.txt",
        "feat",
        "feat",
        &[&base],
    );
    let feat = repo.find_commit(feat_id).unwrap();

    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_tree(
        feat.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    // Create a fake git that logs calls to a file
    let log_path = dir.path().join("git_calls.log");
    let git_wrapper = dir.path().join("git");
    let real_git = which::which("git").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::write(
            &git_wrapper,
            format!(
                "#!/bin/sh\necho \"$@\" >> \"{}\"\nexec \"{}\" \"$@\"",
                log_path.to_str().unwrap(),
                real_git.to_str().unwrap()
            ),
        )
        .unwrap();
        let mut perms = fs::metadata(&git_wrapper).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&git_wrapper, perms).unwrap();
    }

    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = dir.path().to_path_buf().into_os_string();
    new_path.push(":");
    new_path.push(old_path);

    // 1. Start move -> should fail with conflict
    let mut cmd = gits_cmd();
    cmd.arg("move")
        .arg("--onto")
        .arg("target")
        .current_dir(dir.path())
        .env("PATH", &new_path)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure();

    // 2. Resolve conflict
    fs::write(dir.path().join("file.txt"), "resolved").unwrap();
    let out = std::process::Command::new("git")
        .arg("add")
        .arg("file.txt")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 3. Continue move
    let mut cmd_cont = gits_cmd();
    cmd_cont
        .arg("continue")
        .current_dir(dir.path())
        .env("PATH", &new_path)
        .env("GIT_EDITOR", "true")
        .assert()
        .success();

    // 4. Verify rebase was NOT called again for 'feature'
    let log_after = fs::read_to_string(&log_path).unwrap();
    let rebase_calls_after = log_after
        .lines()
        .filter(|l| l.contains("rebase --no-ff") && l.contains("--onto target"))
        .count();
    assert_eq!(
        rebase_calls_after, 1,
        "Should have called rebase exactly once for 'feature'"
    );
}

#[test]
fn test_move_invalid_onto() {
    let (dir, repo) = setup_repo();

    let head_id = repo.head().unwrap().peel_to_commit().unwrap().id();
    let head = repo.find_commit(head_id).unwrap();
    repo.branch("feature", &head, false).unwrap();
    repo.set_head("refs/heads/feature").unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("move")
        .arg("--onto")
        .arg("non-existent-branch")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Target 'non-existent-branch' not found.",
        ));

    // Verify no state file was created
    assert!(!repo.path().join("gits_rebase_state.json").exists());
}

#[test]
fn test_move_fails_immediately_does_not_skip_branch() {
    let (dir, repo) = setup_repo();

    let c1_id = repo.revparse_single("HEAD~2").unwrap().id();
    let c2_id = repo.revparse_single("HEAD~1").unwrap().id();
    let c1 = repo.find_commit(c1_id).unwrap();
    let c2 = repo.find_commit(c2_id).unwrap();

    repo.branch("target", &c1, false).unwrap();
    repo.branch("feature", &c2, false).unwrap();

    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_tree(
        c2.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    // Mock git to fail rebase if a file exists
    let git_path = which::which("git").expect("git not found");
    let git_mock = dir.path().join("git");
    fs::write(
        &git_mock,
        format!(
            r#"#!/bin/sh
if [ "$1" = "rebase" ] && [ "$2" = "--no-ff" ] && [ -f "{}/fail_rebase" ]; then
    echo "Mock rebase failure" >&2
    exit 1
fi
exec {} "$@"
"#,
            dir.path().to_str().unwrap(),
            git_path.to_str().unwrap()
        ),
    )
    .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&git_mock).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&git_mock, perms).unwrap();
    }

    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = dir.path().to_path_buf().into_os_string();
    new_path.push(":");
    new_path.push(old_path);

    // Create the failure trigger file
    fs::write(dir.path().join("fail_rebase"), "").unwrap();

    let mut cmd = gits_cmd();
    cmd.arg("move")
        .arg("--onto")
        .arg("target")
        .current_dir(dir.path())
        .env("PATH", &new_path)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure();

    // Verify state file exists and still contains the branch in remaining_branches
    let state_path = dir.path().join(".git/gits_rebase_state.json");
    assert!(state_path.exists(), "State file should exist");
    let state_content = fs::read_to_string(&state_path).unwrap();

    assert!(
        state_content.contains("\"remaining_branches\": [\n    \"feature\"\n  ]"),
        "State should still contain 'feature' in remaining_branches but got: {}",
        state_content
    );

    // Remove the failure trigger
    fs::remove_file(dir.path().join("fail_rebase")).unwrap();

    // Continue move
    let mut cmd_cont = gits_cmd();
    cmd_cont
        .arg("continue")
        .current_dir(dir.path())
        .env("PATH", &new_path)
        .assert()
        .success();

    // Verify feature IS moved
    let feature = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap();
    let target = repo.find_branch("target", git2::BranchType::Local).unwrap();
    assert!(
        repo.graph_descendant_of(
            feature.get().target().unwrap(),
            target.get().target().unwrap()
        )
        .unwrap()
    );
}

fn setup_abort_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    // Initial commit
    let main_commit_id = make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "initial",
        "initial commit",
        &[],
    );

    // Set HEAD to detached state so we can use repo.head() reliably
    repo.set_head_detached(main_commit_id).unwrap();

    // Branch 'feature'
    {
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        make_commit(
            &repo,
            "refs/heads/feature",
            "file.txt",
            "feature",
            "feature commit",
            &[&parent],
        );
    }

    // Branch 'target' with conflict
    {
        let parent = repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        make_commit(
            &repo,
            "refs/heads/target",
            "file.txt",
            "target",
            "target commit",
            &[&parent],
        );
    }

    (dir, repo)
}

#[test]
fn test_move_abort_does_not_abort_manual_rebase() {
    let (dir, _repo) = setup_abort_repo();

    // Start a manual git rebase that will conflict
    // feature has "feature", target has "target" in file.txt

    // Ensure we are on feature branch and everything is clean
    let out = std::process::Command::new("git")
        .arg("checkout")
        .arg("-f")
        .arg("feature")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git checkout failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let status = std::process::Command::new("git")
        .arg("rebase")
        .arg("target")
        .current_dir(dir.path())
        .output()
        .unwrap()
        .status;

    assert!(
        !status.success(),
        "Manual rebase should have failed due to conflict"
    );

    // Verify rebase is in progress
    assert!(
        dir.path().join(".git/rebase-merge").exists()
            || dir.path().join(".git/rebase-apply").exists()
    );

    // Run gits move abort
    let mut cmd = gits_cmd();
    cmd.arg("abort").current_dir(dir.path()).assert().success();

    // Verify rebase is STILL in progress
    assert!(
        dir.path().join(".git/rebase-merge").exists()
            || dir.path().join(".git/rebase-apply").exists(),
        "Manual rebase should NOT have been aborted by 'gits move abort'"
    );
}

#[test]
fn test_move_abort_cleans_up_rebase_when_state_exists() {
    let (dir, _repo) = setup_abort_repo();

    // Start a manual git rebase that will conflict
    let out = std::process::Command::new("git")
        .arg("checkout")
        .arg("-f")
        .arg("feature")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git checkout failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = std::process::Command::new("git")
        .arg("rebase")
        .arg("target")
        .current_dir(dir.path())
        .output()
        .unwrap();
    // This rebase is expected to fail with a conflict
    assert!(
        !out.status.success(),
        "rebase should have failed with conflict"
    );

    // Manually create a gits move state file
    let state_path = dir.path().join(".git/gits_rebase_state.json");
    fs::write(
        &state_path,
        r#"{
  "operation": "Move",
  "original_branch": "feature",
  "target_branch": "target",
  "remaining_branches": [],
  "in_progress_branch": null,
  "parent_id_map": {},
  "parent_name_map": {}
}"#,
    )
    .unwrap();

    // Run gits move abort
    let mut cmd = gits_cmd();
    cmd.arg("abort").current_dir(dir.path()).assert().success();

    // Verify state file is gone
    assert!(!state_path.exists(), "State file should have been removed");

    // Verify rebase is aborted
    assert!(
        !dir.path().join(".git/rebase-merge").exists()
            && !dir.path().join(".git/rebase-apply").exists(),
        "Rebase should have been aborted because state file existed"
    );
}
