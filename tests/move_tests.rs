mod common;
use common::{
    StateFile, kin_cmd, make_commit, rebase_state, rebase_state_file, repo_init, run_ok, state_file,
};
use git2::{Oid, Repository};
use kindra::rebase_utils::{Operation, RebaseState, save_state};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn setup_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

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

fn write_rebase_state_fixture(repo: &Repository, _state_path: &Path, feature_tip: Oid) {
    let state = RebaseState {
        owned_tip_map: HashMap::from([("feature".to_string(), feature_tip.to_string())]),
        ..rebase_state(Operation::Move, "feature", "target")
    };

    save_state(repo, &state).unwrap();
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

    let mut cmd = kin_cmd();
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

    let mut cmd = kin_cmd();
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

    let mut cmd = kin_cmd();
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
    let repo = repo_init(dir.path());

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

    let mut cmd = kin_cmd();
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

    let mut cmd_cont = kin_cmd();
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
#[cfg(unix)]
fn test_move_manual_git_continue_then_kin_continue_resumes_next_branch() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

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

    let feature_a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "file.txt",
        "feature-a",
        "feature-a",
        &[&base],
    );
    let feature_a = repo.find_commit(feature_a_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-b",
        "feature-b.txt",
        "feature-b",
        "feature-b",
        &[&feature_a],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_tree(
        feature_a.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

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

    kin_cmd()
        .arg("move")
        .arg("--onto")
        .arg("target")
        .current_dir(dir.path())
        .env("PATH", &new_path)
        .assert()
        .failure()
        .stderr(predicates::str::contains("Resolve conflicts"));

    fs::write(dir.path().join("file.txt"), "resolved").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok(
        "git",
        &["-c", "core.editor=true", "rebase", "--continue"],
        dir.path(),
    );

    kin_cmd()
        .arg("continue")
        .current_dir(dir.path())
        .env("PATH", &new_path)
        .env("GIT_EDITOR", "true")
        .assert()
        .success();

    let log_after = fs::read_to_string(&log_path).unwrap();
    let feature_a_rebases = log_after
        .lines()
        .filter(|line| line.contains("rebase --no-ff") && line.ends_with(" feature-a"))
        .count();
    let feature_b_rebases = log_after
        .lines()
        .filter(|line| line.contains("rebase --no-ff") && line.ends_with(" feature-b"))
        .count();
    assert_eq!(
        feature_a_rebases, 1,
        "kin continue should not re-run the manually completed feature-a rebase"
    );
    assert_eq!(feature_b_rebases, 1, "kin continue should rebase feature-b");

    let repo = Repository::open(dir.path()).unwrap();
    let feature_a_tip = repo
        .find_branch("feature-a", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let feature_b_tip = repo
        .find_branch("feature-b", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let target_tip = repo.revparse_single("target").unwrap().id();
    assert!(repo.graph_descendant_of(feature_a_tip, target_tip).unwrap());
    assert!(
        repo.graph_descendant_of(feature_b_tip, feature_a_tip)
            .unwrap()
    );
    assert!(!rebase_state_file(dir.path()).exists());
}

#[test]
fn test_move_manual_git_continue_completed_state_clears_on_status() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

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

    let feature_id = make_commit(
        &repo,
        "refs/heads/feature",
        "file.txt",
        "feature",
        "feature",
        &[&base],
    );
    let feature = repo.find_commit(feature_id).unwrap();

    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_tree(
        feature.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    kin_cmd()
        .arg("move")
        .arg("--onto")
        .arg("target")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("Resolve conflicts"));

    fs::write(dir.path().join("file.txt"), "resolved").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok(
        "git",
        &["-c", "core.editor=true", "rebase", "--continue"],
        dir.path(),
    );

    kin_cmd()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("No Kindra operation active."));

    assert!(!rebase_state_file(dir.path()).exists());

    kin_cmd()
        .arg("move")
        .arg("--onto")
        .arg("target")
        .current_dir(dir.path())
        .assert()
        .success();
}

#[test]
fn test_move_passive_reconcile_keeps_completed_state_with_pending_finalizer() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let base_id = make_commit(&repo, "refs/heads/main", "file.txt", "base", "base", &[]);
    let base = repo.find_commit(base_id).unwrap();
    let feature_id = make_commit(
        &repo,
        "refs/heads/feature",
        "file.txt",
        "feature",
        "feature",
        &[&base],
    );
    let feature = repo.find_commit(feature_id).unwrap();

    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_tree(
        feature.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    let state = RebaseState {
        original_tip_map: HashMap::from([("feature".to_string(), feature_id.to_string())]),
        unstage_on_restore: true,
        ..rebase_state(Operation::Move, "feature", "main")
    };
    save_state(&repo, &state).unwrap();

    kin_cmd()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("Move in progress"));

    assert!(rebase_state_file(dir.path()).exists());

    kin_cmd()
        .arg("continue")
        .current_dir(dir.path())
        .assert()
        .success();

    assert!(!rebase_state_file(dir.path()).exists());
}

#[test]
fn test_move_continue_forwards_editor_env_to_git() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

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

    kin_cmd()
        .arg("move")
        .arg("--onto")
        .arg("target")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("Resolve conflicts"));

    fs::write(dir.path().join("file.txt"), "resolved content").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());

    // With no GIT_EDITOR, `kin continue` must resolve the editor the same way
    // the rest of the CLI does and forward it to git — here that falls through
    // to $EDITOR. A marker-writing editor proves it was actually invoked. A
    // local empty `core.editor` neutralizes any machine-level `core.editor` (and
    // VISUAL is cleared) so it can't shadow $EDITOR and make the fallback
    // ambiguous; nothing else about the environment is altered.
    run_ok("git", &["config", "core.editor", ""], dir.path());
    let marker = dir.path().join("editor-was-invoked");
    let editor = dir.path().join("fake-editor.sh");
    fs::write(
        &editor,
        format!("#!/bin/sh\ntouch \"{}\"\n", marker.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
    }

    kin_cmd()
        .arg("continue")
        .current_dir(dir.path())
        .env_remove("GIT_EDITOR")
        .env_remove("VISUAL")
        .env("EDITOR", &editor)
        .assert()
        .success();

    assert!(
        marker.exists(),
        "kin continue should resolve and invoke $EDITOR when GIT_EDITOR is unset"
    );

    let feature_new = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap();
    let target = repo.find_commit(target_id).unwrap();
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

    let mut cmd = kin_cmd();
    cmd.arg("move")
        .arg("--onto")
        .arg("target")
        .current_dir(dir.path())
        .assert()
        .success();

    let mut cmd_abort = kin_cmd();
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

    let mut cmd = kin_cmd();
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

    let mut cmd = kin_cmd();
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
    let repo = repo_init(dir.path());

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

    let mut cmd = kin_cmd();
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
    let repo = repo_init(dir.path());

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

    let fb_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "b commit",
        &[&fa],
    );
    let fb = repo.find_commit(fb_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-c",
        "c.txt",
        "c",
        "c commit",
        &[&fb],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_tree(
        fa.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("move")
        .arg("--onto")
        .arg("feature-b")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    let main = repo.find_branch("main", git2::BranchType::Local).unwrap();
    let feature_a = repo
        .find_branch("feature-a", git2::BranchType::Local)
        .unwrap();
    let feature_b = repo
        .find_branch("feature-b", git2::BranchType::Local)
        .unwrap();
    let feature_c = repo
        .find_branch("feature-c", git2::BranchType::Local)
        .unwrap();
    let feature_a_commit = repo.find_commit(feature_a.get().target().unwrap()).unwrap();
    let feature_b_commit = repo.find_commit(feature_b.get().target().unwrap()).unwrap();
    let feature_c_commit = repo.find_commit(feature_c.get().target().unwrap()).unwrap();

    assert_eq!(
        feature_b_commit.parent_id(0).unwrap(),
        main.get().target().unwrap()
    );
    assert_eq!(
        feature_a_commit.parent_id(0).unwrap(),
        feature_c.get().target().unwrap()
    );
    assert_eq!(
        feature_c_commit.parent_id(0).unwrap(),
        feature_b.get().target().unwrap()
    );
}

#[test]
fn test_move_onto_descendant_reorders_target_descendants_too() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

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

    let fb_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "b commit",
        &[&fa],
    );
    let fb = repo.find_commit(fb_id).unwrap();

    let fc_id = make_commit(
        &repo,
        "refs/heads/feature-c",
        "c.txt",
        "c",
        "c commit",
        &[&fb],
    );
    let fc = repo.find_commit(fc_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-d",
        "d.txt",
        "d",
        "d commit",
        &[&fc],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_tree(
        fa.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("move")
        .arg("--onto")
        .arg("feature-c")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();

    let main = repo.find_branch("main", git2::BranchType::Local).unwrap();
    let feature_a = repo
        .find_branch("feature-a", git2::BranchType::Local)
        .unwrap();
    let feature_b = repo
        .find_branch("feature-b", git2::BranchType::Local)
        .unwrap();
    let feature_c = repo
        .find_branch("feature-c", git2::BranchType::Local)
        .unwrap();
    let feature_d = repo
        .find_branch("feature-d", git2::BranchType::Local)
        .unwrap();
    let feature_a_commit = repo.find_commit(feature_a.get().target().unwrap()).unwrap();
    let feature_b_commit = repo.find_commit(feature_b.get().target().unwrap()).unwrap();
    let feature_c_commit = repo.find_commit(feature_c.get().target().unwrap()).unwrap();
    let feature_d_commit = repo.find_commit(feature_d.get().target().unwrap()).unwrap();

    assert_eq!(
        feature_c_commit.parent_id(0).unwrap(),
        main.get().target().unwrap()
    );
    assert_eq!(
        feature_d_commit.parent_id(0).unwrap(),
        feature_c.get().target().unwrap()
    );
    assert_eq!(
        feature_a_commit.parent_id(0).unwrap(),
        feature_d.get().target().unwrap()
    );
    assert_eq!(
        feature_b_commit.parent_id(0).unwrap(),
        feature_a.get().target().unwrap()
    );
}

#[test]
fn test_move_onto_descendant_preserves_forked_subtree() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

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

    let fb_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "b commit",
        &[&fa],
    );
    let fb = repo.find_commit(fb_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-c",
        "c.txt",
        "c",
        "c commit",
        &[&fb],
    );
    make_commit(
        &repo,
        "refs/heads/feature-d",
        "d.txt",
        "d",
        "d commit",
        &[&fb],
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_tree(
        fa.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("move")
        .arg("--onto")
        .arg("feature-b")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .success();
    for (branch, parent) in [
        ("feature-b", "main"),
        ("feature-a", "feature-b"),
        ("feature-c", "feature-b"),
        ("feature-d", "feature-b"),
    ] {
        let commit = repo
            .revparse_single(branch)
            .unwrap()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            commit.parent_id(0).unwrap(),
            repo.revparse_single(parent).unwrap().id()
        );
    }
}

#[test]
fn test_move_onto_descendant_rejects_fork_then_merge_subtree() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

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

    let fb_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "b commit",
        &[&fa],
    );
    let fb = repo.find_commit(fb_id).unwrap();

    let fc_id = make_commit(
        &repo,
        "refs/heads/feature-c",
        "c.txt",
        "c",
        "c commit",
        &[&fb],
    );

    repo.branch("feature-c-old", &repo.find_commit(fc_id).unwrap(), false)
        .unwrap();
    repo.branch("feature-d", &repo.find_commit(fb_id).unwrap(), false)
        .unwrap();

    run_ok("git", &["checkout", "-f", "feature-d"], dir.path());
    fs::write(dir.path().join("d.txt"), "d").unwrap();
    run_ok("git", &["add", "d.txt"], dir.path());
    run_ok("git", &["commit", "-m", "d commit"], dir.path());

    run_ok("git", &["checkout", "-f", "feature-c"], dir.path());
    run_ok(
        "git",
        &["merge", "--no-ff", "feature-d", "-m", "merge d"],
        dir.path(),
    );

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_tree(
        fa.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    let mut cmd = kin_cmd();
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
        .stderr(predicates::str::contains("affected subtree is forked"));
}

#[test]
fn test_move_onto_descendant_conflict_and_continue() {
    descendant_tree_conflict(false);
}

#[test]
fn test_move_onto_descendant_tree_conflict_and_abort() {
    descendant_tree_conflict(true);
}

fn descendant_tree_conflict(abort: bool) {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "1\n2\n3\n",
        "base",
        &[],
    );
    let base = repo.find_commit(base_id).unwrap();

    let fa_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "file.txt",
        "1\nfeature-a\n3\n",
        "a commit",
        &[&base],
    );
    let fa = repo.find_commit(fa_id).unwrap();

    let fb_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "b commit",
        &[&fa],
    );
    let fb = repo.find_commit(fb_id).unwrap();

    let fc_id = make_commit(
        &repo,
        "refs/heads/feature-c",
        "c.txt",
        "c",
        "c commit",
        &[&fb],
    );
    let _fc = repo.find_commit(fc_id).unwrap();

    let side_id = make_commit(&repo, "refs/heads/side", "side.txt", "side", "side", &[&fb]);

    run_ok("git", &["checkout", "-f", "main"], dir.path());
    fs::write(dir.path().join("file.txt"), "1\nmain\n3\n").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok("git", &["commit", "-m", "main commit"], dir.path());

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_tree(
        fa.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("move")
        .arg("--onto")
        .arg("feature-c")
        .current_dir(dir.path())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Resolve conflicts"));

    if abort {
        kin_cmd()
            .arg("abort")
            .current_dir(dir.path())
            .assert()
            .success();
        for (branch, original) in [
            ("feature-a", fa_id),
            ("feature-b", fb_id),
            ("feature-c", fc_id),
            ("side", side_id),
        ] {
            assert_eq!(repo.revparse_single(branch).unwrap().id(), original);
        }
        assert!(!StateFile::Rebase.in_git_dir(repo.path()).exists());
        assert_eq!(repo.state(), git2::RepositoryState::Clean);
        return;
    }

    fs::write(dir.path().join("file.txt"), "1\nresolved\n3\n").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());

    let mut cmd_cont = kin_cmd();
    cmd_cont
        .arg("continue")
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();
    let main = repo.find_branch("main", git2::BranchType::Local).unwrap();
    let feature_a = repo
        .find_branch("feature-a", git2::BranchType::Local)
        .unwrap();
    let feature_b = repo
        .find_branch("feature-b", git2::BranchType::Local)
        .unwrap();
    let feature_c = repo
        .find_branch("feature-c", git2::BranchType::Local)
        .unwrap();
    let feature_a_commit = repo.find_commit(feature_a.get().target().unwrap()).unwrap();
    let feature_b_commit = repo.find_commit(feature_b.get().target().unwrap()).unwrap();
    let feature_c_commit = repo.find_commit(feature_c.get().target().unwrap()).unwrap();

    assert_eq!(
        feature_c_commit.parent_id(0).unwrap(),
        main.get().target().unwrap()
    );
    assert_eq!(
        feature_a_commit.parent_id(0).unwrap(),
        feature_c.get().target().unwrap()
    );
    assert_eq!(
        feature_b_commit.parent_id(0).unwrap(),
        feature_a.get().target().unwrap()
    );
    let side = repo
        .revparse_single("side")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    assert_eq!(side.parent_id(0).unwrap(), feature_b_commit.id());
    assert!(!StateFile::Rebase.in_git_dir(repo.path()).exists());
}

#[test]
fn test_move_onto_descendant_prestart_failure_retries_first_reordered_branch() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

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

    let fb_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b",
        "b commit",
        &[&fa],
    );
    let fb = repo.find_commit(fb_id).unwrap();

    let fc_id = make_commit(
        &repo,
        "refs/heads/feature-c",
        "c.txt",
        "c",
        "c commit",
        &[&fb],
    );
    let fc = repo.find_commit(fc_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/feature-d",
        "d.txt",
        "d",
        "d commit",
        &[&fc],
    );

    make_commit(&repo, "refs/heads/side", "side.txt", "side", "side", &[&fb]);

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_tree(
        fa.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    let git_path = which::which("git").expect("git not found");
    let git_wrapper = dir.path().join("git");
    let log_path = dir.path().join("git.log");
    let fail_rebase_path = dir.path().join("fail_rebase");
    fs::write(&log_path, "").unwrap();
    fs::write(&fail_rebase_path, "").unwrap();
    fs::write(
        &git_wrapper,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{}"
if [ "$1" = "rebase" ] && [ "$2" = "--no-ff" ] && [ -f "{}" ]; then
    rm "{}"
    echo "Mock rebase failure" >&2
    exit 1
fi
exec "{}" "$@"
"#,
            log_path.to_str().unwrap(),
            fail_rebase_path.to_str().unwrap(),
            fail_rebase_path.to_str().unwrap(),
            git_path.to_str().unwrap()
        ),
    )
    .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&git_wrapper).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&git_wrapper, perms).unwrap();
    }

    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = dir.path().to_path_buf().into_os_string();
    new_path.push(":");
    new_path.push(old_path);

    let mut cmd = kin_cmd();
    cmd.arg("move")
        .arg("--onto")
        .arg("feature-c")
        .current_dir(dir.path())
        .env("PATH", &new_path)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .assert()
        .failure();

    let state_content = fs::read_to_string(rebase_state_file(dir.path())).unwrap();
    assert!(
        state_content.contains("\"in_progress_branch\": null"),
        "Pre-start failure should clear in_progress_branch but got: {state_content}"
    );

    let mut cmd_cont = kin_cmd();
    cmd_cont
        .arg("continue")
        .current_dir(dir.path())
        .env("PATH", &new_path)
        .assert()
        .success();

    let log = fs::read_to_string(&log_path).unwrap();
    let feature_c_rebase_calls = log
        .lines()
        .filter(|line| line.contains("rebase --no-ff") && line.ends_with(" feature-c"))
        .count();
    assert_eq!(
        feature_c_rebase_calls, 2,
        "feature-c should be retried after pre-start failure"
    );

    let repo = Repository::open(dir.path()).unwrap();
    let main = repo.find_branch("main", git2::BranchType::Local).unwrap();
    let feature_a = repo
        .find_branch("feature-a", git2::BranchType::Local)
        .unwrap();
    let feature_b = repo
        .find_branch("feature-b", git2::BranchType::Local)
        .unwrap();
    let feature_c = repo
        .find_branch("feature-c", git2::BranchType::Local)
        .unwrap();
    let feature_d = repo
        .find_branch("feature-d", git2::BranchType::Local)
        .unwrap();
    let feature_a_commit = repo.find_commit(feature_a.get().target().unwrap()).unwrap();
    let feature_b_commit = repo.find_commit(feature_b.get().target().unwrap()).unwrap();
    let feature_c_commit = repo.find_commit(feature_c.get().target().unwrap()).unwrap();
    let feature_d_commit = repo.find_commit(feature_d.get().target().unwrap()).unwrap();

    assert_eq!(
        feature_c_commit.parent_id(0).unwrap(),
        main.get().target().unwrap()
    );
    assert_eq!(
        feature_d_commit.parent_id(0).unwrap(),
        feature_c.get().target().unwrap()
    );
    assert_eq!(
        feature_a_commit.parent_id(0).unwrap(),
        feature_d.get().target().unwrap()
    );
    assert_eq!(
        feature_b_commit.parent_id(0).unwrap(),
        feature_a.get().target().unwrap()
    );
    let side = repo
        .revparse_single("side")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        side.parent_id(0).unwrap(),
        repo.revparse_single("feature-b").unwrap().id()
    );
}

#[test]
fn test_move_abort_cleans_up_git_rebase() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

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
    let mut cmd = kin_cmd();
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
    let mut cmd_abort = kin_cmd();
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
    let (dir, repo) = setup_abort_repo();
    let feature_tip = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();

    let state_path = rebase_state_file(dir.path());
    write_rebase_state_fixture(&repo, &state_path, feature_tip);

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

    // 4. Run kin move abort - it should fail because rebase --abort failed
    let mut cmd = kin_cmd();
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
fn test_move_abort_leaves_manual_rebase_when_owned_tip_map_mismatches() {
    let (dir, _repo) = setup_abort_repo();

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

    let state_path = rebase_state_file(dir.path());
    fs::write(
        &state_path,
        r#"{
  "operation": "Move",
  "original_branch": "feature",
  "target_branch": "target",
  "remaining_branches": [],
  "in_progress_branch": null,
  "parent_id_map": {},
  "parent_name_map": {},
  "owned_tip_map": {
    "feature": "0000000000000000000000000000000000000000"
  }
}"#,
    )
    .unwrap();

    kin_cmd()
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();

    assert!(!state_path.exists(), "State file should have been removed");
    assert!(
        dir.path().join(".git/rebase-merge").exists()
            || dir.path().join(".git/rebase-apply").exists(),
        "Native rebase should remain in progress when owned_tip_map does not match"
    );
}

#[test]
fn test_move_conflict_and_continue_no_re_rebase() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

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
    let mut cmd = kin_cmd();
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
    let mut cmd_cont = kin_cmd();
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

    let mut cmd = kin_cmd();
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
    assert!(!StateFile::Rebase.in_git_dir(repo.path()).exists());
}

#[test]
fn test_move_respects_git_rebase_autostash_config() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let base_id = make_commit(&repo, "refs/heads/main", "file.txt", "base\n", "base", &[]);
    let base = repo.find_commit(base_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/target",
        "file.txt",
        "base\ntarget\n",
        "target",
        &[&base],
    );

    let feature_id = make_commit(
        &repo,
        "refs/heads/feature",
        "file.txt",
        "base\nfeature\n",
        "feature",
        &[&base],
    );
    let feature = repo.find_commit(feature_id).unwrap();

    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_tree(
        feature.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    run_ok("git", &["config", "rebase.autostash", "true"], dir.path());
    fs::write(dir.path().join("file.txt"), "base\nfeature\ndirty\n").unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("move")
        .arg("--onto")
        .arg("target")
        .current_dir(dir.path())
        .assert()
        .failure();

    // Verify autostash worked: rebase started (proving git config is respected)
    assert!(
        dir.path().join(".git/rebase-merge").exists()
            || dir.path().join(".git/rebase-apply").exists(),
        "git config rebase.autostash should allow move to start rebasing"
    );

    // Kindra owns the stash for the whole operation, including abort.
    kin_cmd()
        .current_dir(dir.path())
        .arg("abort")
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(dir.path().join("file.txt")).unwrap(),
        "base\nfeature\ndirty\n",
        "dirty changes should be preserved after abort"
    );
}

#[test]
fn test_move_repo_config_enables_autostash_and_persists_in_state() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let base_id = make_commit(&repo, "refs/heads/main", "file.txt", "base\n", "base", &[]);
    let base = repo.find_commit(base_id).unwrap();

    make_commit(
        &repo,
        "refs/heads/target",
        "file.txt",
        "base\ntarget\n",
        "target",
        &[&base],
    );

    let feature_id = make_commit(
        &repo,
        "refs/heads/feature",
        "file.txt",
        "base\nfeature\n",
        "feature",
        &[&base],
    );
    let feature = repo.find_commit(feature_id).unwrap();

    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_tree(
        feature.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();

    std::fs::write(
        repo.path().join("kindra.toml"),
        "[rebase]\nautostash = true\n",
    )
    .unwrap();
    fs::write(dir.path().join("file.txt"), "base\nfeature\ndirty\n").unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("move")
        .arg("--onto")
        .arg("target")
        .current_dir(dir.path())
        .assert()
        .failure();

    assert!(
        dir.path().join(".git/rebase-merge").exists()
            || dir.path().join(".git/rebase-apply").exists(),
        "repo config should allow move to start rebasing with autostash"
    );

    let state = kindra::rebase_utils::load_state(&repo).unwrap();
    assert!(
        state.set_asides.changes().is_some(),
        "operation must own the autostash"
    );
    assert!(
        !state.autostash,
        "Git must not restore edits on each branch"
    );
}

/// Repository config is shared by every worktree, so `[rebase] autostash` must
/// also apply to a move started from a linked worktree.
#[test]
fn test_move_repo_config_enables_autostash_in_linked_worktree() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let base_id = make_commit(&repo, "refs/heads/main", "file.txt", "base\n", "base", &[]);
    let base = repo.find_commit(base_id).unwrap();
    make_commit(
        &repo,
        "refs/heads/target",
        "file.txt",
        "base\ntarget\n",
        "target",
        &[&base],
    );
    make_commit(
        &repo,
        "refs/heads/feature",
        "file.txt",
        "base\nfeature\n",
        "feature",
        &[&base],
    );

    common::write_repo_config(dir.path(), "[rebase]\nautostash = true\n");
    let (_wt_parent, wt) = common::add_linked_worktree(dir.path(), "feature");
    fs::write(wt.join("file.txt"), "base\nfeature\ndirty\n").unwrap();

    let output = kin_cmd()
        .arg("move")
        .arg("--onto")
        .arg("target")
        .current_dir(&wt)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    let linked = Repository::open(&wt).unwrap();
    assert!(
        linked.path().join("rebase-merge").exists() || linked.path().join("rebase-apply").exists(),
        "move should start rebasing with autostash\nstderr:\n{stderr}"
    );
    let state = kindra::rebase_utils::load_state(&linked).unwrap();
    assert!(
        state.set_asides.changes().is_some(),
        "operation must own the autostash"
    );
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

    let mut cmd = kin_cmd();
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
    let state_path = rebase_state_file(dir.path());
    assert!(state_path.exists(), "State file should exist");
    let state = kindra::rebase_utils::load_state(&Repository::open(dir.path()).unwrap()).unwrap();
    assert_eq!(
        state.remaining_branches,
        ["feature"],
        "State should still contain 'feature' in remaining_branches"
    );

    // Remove the failure trigger
    fs::remove_file(dir.path().join("fail_rebase")).unwrap();

    // Continue move
    let mut cmd_cont = kin_cmd();
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
    let repo = repo_init(dir.path());

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

    // Run kin move abort
    let mut cmd = kin_cmd();
    cmd.arg("abort").current_dir(dir.path()).assert().success();

    // Verify rebase is STILL in progress
    assert!(
        dir.path().join(".git/rebase-merge").exists()
            || dir.path().join(".git/rebase-apply").exists(),
        "Manual rebase should NOT have been aborted by 'kin move abort'"
    );
}

#[test]
fn test_move_abort_cleans_up_rebase_when_state_exists() {
    let (dir, repo) = setup_abort_repo();
    let feature_tip = repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();

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

    let state_path = rebase_state_file(dir.path());
    write_rebase_state_fixture(&repo, &state_path, feature_tip);

    // Run kin move abort
    let mut cmd = kin_cmd();
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

#[test]
fn test_move_blocked_by_stale_run_state() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());
    let main_id = make_commit(&repo, "refs/heads/main", "file.txt", "x", "initial", &[]);
    let main_commit = repo.find_commit(main_id).unwrap();
    make_commit(
        &repo,
        "refs/heads/feature",
        "f.txt",
        "f",
        "feat",
        &[&main_commit],
    );
    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // An interrupted `kin run` left run state behind (no RepoLock is held).
    std::fs::write(
        state_file(dir.path(), StateFile::Run),
        r#"{"target_branches":["feature"],"current_index":0,"args":{"command":"false","continue_on_failure":false},"original_branch":"feature","original_head_id":"0000000000000000000000000000000000000000","status":"failed"}"#,
    )
    .unwrap();

    kin_cmd()
        .arg("move")
        .arg("--onto")
        .arg("main")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("already in progress"));

    assert!(state_file(dir.path(), StateFile::Run).exists());
}

#[test]
fn move_refuses_dirty_working_tree_with_no_autostash() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let base_id = make_commit(
        &repo,
        "refs/heads/main",
        "shared.txt",
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

    run_ok("git", &["checkout", "-f", "feature-b"], dir.path());
    fs::write(dir.path().join("shared.txt"), "dirty").unwrap();

    // With --no-autostash and a dirty tree, `move` refuses up front — before
    // checking out anything or writing operation state — the same guard `sync`
    // enforces, rather than moving HEAD and then failing inside git rebase.
    kin_cmd()
        .args(["move", "--onto", "main", "--no-autostash"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("uncommitted changes"));

    let repo = Repository::open(dir.path()).unwrap();
    assert_eq!(repo.state(), git2::RepositoryState::Clean);
    // HEAD is untouched and the dirty change is preserved, so there is nothing to
    // continue or abort.
    assert_eq!(repo.head().unwrap().shorthand(), Some("feature-b"));
    assert_eq!(
        fs::read_to_string(dir.path().join("shared.txt")).unwrap(),
        "dirty"
    );
    assert!(!rebase_state_file(dir.path()).exists());
    assert!(!dir.path().join(".git/rebase-merge").exists());
    assert!(!dir.path().join(".git/rebase-apply").exists());
}

fn dirty_parent_with_child() -> tempfile::TempDir {
    let dir = common::setup_repo();
    fs::write(dir.path().join("file.txt"), "child version\n").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok(
        "git",
        &["commit", "-m", "child changes shared file"],
        dir.path(),
    );
    run_ok("git", &["checkout", "feature-a"], dir.path());
    fs::write(dir.path().join("file.txt"), "uncommitted parent edit\n").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    dir
}

fn assert_parent_edits_restored(dir: &Path) {
    assert_eq!(common::current_branch(dir), "feature-a");
    assert_eq!(
        fs::read_to_string(dir.join("file.txt")).unwrap(),
        "uncommitted parent edit\n"
    );
    let status = common::git_command(dir)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(status.status.success());
    assert_eq!(String::from_utf8_lossy(&status.stdout), "M  file.txt\n");
    let stashes = common::git_command(dir)
        .args(["stash", "list"])
        .output()
        .unwrap();
    assert!(stashes.status.success());
    assert!(stashes.stdout.is_empty());
    assert!(!rebase_state_file(dir).exists());
}

#[test]
fn move_autostash_restores_edits_only_on_caller() {
    let dir = dirty_parent_with_child();
    kin_cmd()
        .current_dir(dir.path())
        .args(["move", "--onto", "main", "--autostash"])
        .assert()
        .success();
    assert_parent_edits_restored(dir.path());
    let repo = Repository::open(dir.path()).unwrap();
    let parent = repo.revparse_single("feature-a").unwrap().id();
    let child = repo
        .revparse_single("feature-b")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    assert!(repo.graph_descendant_of(child.id(), parent).unwrap());
    assert_eq!(
        repo.find_blob(child.tree().unwrap().get_name("file.txt").unwrap().id())
            .unwrap()
            .content(),
        b"child version\n"
    );
}

#[test]
fn move_autostash_abort_restores_staged_caller_edits() {
    let dir = dirty_parent_with_child();
    let repo = Repository::open(dir.path()).unwrap();
    let original_parent = repo.revparse_single("feature-a").unwrap().id();
    let original_child = repo.revparse_single("feature-b").unwrap().id();
    // A commit-tree target avoids disturbing the caller's staged edits.
    let main = repo
        .revparse_single("main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let blob = repo.blob(b"conflicting target content\n").unwrap();
    let mut tree = repo.treebuilder(Some(&main.tree().unwrap())).unwrap();
    tree.insert("feature.txt", blob, 0o100644).unwrap();
    let tree_id = tree.write().unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    repo.commit(
        Some("refs/heads/target"),
        &sig,
        &sig,
        "target",
        &repo.find_tree(tree_id).unwrap(),
        &[&main],
    )
    .unwrap();
    kin_cmd()
        .current_dir(dir.path())
        .args(["move", "--onto", "target", "--autostash"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Resolve conflicts"));
    kin_cmd()
        .current_dir(dir.path())
        .arg("abort")
        .assert()
        .success();
    assert_parent_edits_restored(dir.path());
    assert_eq!(
        repo.revparse_single("feature-a").unwrap().id(),
        original_parent
    );
    assert_eq!(
        repo.revparse_single("feature-b").unwrap().id(),
        original_child
    );
}

#[test]
fn move_onto_descendant_allows_merge_before_subtree() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());
    let base_id = make_commit(&repo, "refs/heads/main", "root.txt", "root", "base", &[]);
    let base = repo.find_commit(base_id).unwrap();
    let left_id = make_commit(
        &repo,
        "refs/heads/parent",
        "left.txt",
        "left",
        "left",
        &[&base],
    );
    let right_id = make_commit(
        &repo,
        "refs/heads/merged-side",
        "right.txt",
        "right",
        "right",
        &[&base],
    );
    let left = repo.find_commit(left_id).unwrap();
    let right = repo.find_commit(right_id).unwrap();
    let parent_id = make_commit(
        &repo,
        "refs/heads/parent",
        "right.txt",
        "right",
        "ancestor merge",
        &[&left, &right],
    );
    let parent = repo.find_commit(parent_id).unwrap();
    let a_id = make_commit(&repo, "refs/heads/feature-a", "a.txt", "a", "a", &[&parent]);
    let a = repo.find_commit(a_id).unwrap();
    make_commit(&repo, "refs/heads/feature-b", "b.txt", "b", "b", &[&a]);
    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    kin_cmd()
        .args(["move", "--onto", "feature-b"])
        .current_dir(dir.path())
        .assert()
        .success();

    let b = repo
        .revparse_single("feature-b")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let a = repo
        .revparse_single("feature-a")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    assert_eq!(b.parent_id(0).unwrap(), parent_id);
    assert_eq!(a.parent_id(0).unwrap(), b.id());
    assert_eq!(repo.revparse_single("parent").unwrap().id(), parent_id);
    assert_eq!(repo.revparse_single("merged-side").unwrap().id(), right_id);
    assert_eq!(repo.head().unwrap().shorthand(), Some("feature-a"));
    assert_eq!(repo.state(), git2::RepositoryState::Clean);
    assert!(!StateFile::Rebase.in_git_dir(repo.path()).exists());
}

/// Kindra's autostash follows `rebase.autostash`; set it in the test repository
/// so the outcome never depends on the developer's global Git config.
fn set_repo_autostash(cwd: &Path, enabled: bool) {
    run_ok(
        "git",
        &[
            "config",
            "rebase.autostash",
            if enabled { "true" } else { "false" },
        ],
        cwd,
    );
}

fn git_path_exists(cwd: &Path, name: &str) -> bool {
    Repository::open(cwd).unwrap().path().join(name).exists()
}

#[test]
fn move_refuses_resolved_native_merge_and_cherry_pick_with_autostash() {
    for (op, head_file, hint) in [
        (
            common::NativeStop::Merge,
            "MERGE_HEAD",
            "git merge --continue",
        ),
        (
            common::NativeStop::CherryPick,
            "CHERRY_PICK_HEAD",
            "git cherry-pick --continue",
        ),
    ] {
        let dir = common::setup_repo();
        set_repo_autostash(dir.path(), true);
        common::stop_native_operation(dir.path(), op, true);

        kin_cmd()
            .args(["move", "--onto", "main"])
            .current_dir(dir.path())
            .assert()
            .failure()
            .stderr(predicates::str::contains(hint));

        // The resolution is still staged and Git can still finish the operation.
        assert!(
            git_path_exists(dir.path(), head_file),
            "{op:?}: {head_file} lost"
        );
        let staged = common::git_command(dir.path())
            .args(["show", &format!(":{}", common::NATIVE_CONFLICT_FILE)])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&staged.stdout), "resolved\n");
        common::assert_no_kindra_operation(dir.path());
    }
}

#[test]
fn move_refused_by_unresolved_native_merge_leaves_no_operation_behind() {
    let dir = common::setup_repo();
    set_repo_autostash(dir.path(), true);
    common::stop_native_operation(dir.path(), common::NativeStop::Merge, false);

    kin_cmd()
        .args(["move", "--onto", "main"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("git merge --abort"));

    common::assert_no_kindra_operation(dir.path());
    let status = kin_cmd()
        .arg("status")
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(status.status.success());
    assert!(stdout.contains("No Kindra operation active."), "{stdout}");
    assert!(!stdout.contains("Move in progress"), "{stdout}");
}

#[test]
fn move_refuses_during_native_bisect() {
    let dir = common::setup_repo();
    common::start_native_bisect(dir.path(), "feature-b", "main");
    run_ok("git", &["checkout", "-q", "feature-b"], dir.path());

    kin_cmd()
        .args(["move", "--onto", "main"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("git bisect reset"));
    common::assert_no_kindra_operation(dir.path());
}

/// A move paused on a conflict, whose rebase the user then aborted with Git.
fn paused_move_without_native_rebase() -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());
    let base_id = make_commit(&repo, "refs/heads/main", "file.txt", "base", "initial", &[]);
    let base = repo.find_commit(base_id).unwrap();
    make_commit(
        &repo,
        "refs/heads/target",
        "file.txt",
        "target content",
        "target commit",
        &[&base],
    );
    make_commit(
        &repo,
        "refs/heads/feature",
        "file.txt",
        "feature content",
        "feature commit",
        &[&base],
    );
    run_ok("git", &["checkout", "-q", "-f", "feature"], dir.path());
    kin_cmd()
        .args(["move", "--onto", "target"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("Resolve conflicts"));
    run_ok("git", &["rebase", "--abort"], dir.path());
    let reopened = Repository::open(dir.path()).unwrap();
    (dir, reopened)
}

/// Stop `git am` applying target's commit onto the current branch, without
/// committing anything.
fn stop_git_am_on_target_patch(cwd: &Path) {
    let patch = common::git_command(cwd)
        .args(["format-patch", "-1", "--stdout", "target"])
        .output()
        .unwrap();
    assert!(patch.status.success());
    let patch_file = tempfile::NamedTempFile::new().unwrap();
    fs::write(patch_file.path(), &patch.stdout).unwrap();
    let am = common::git_command(cwd)
        .args(["am", patch_file.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!am.status.success(), "git am was expected to stop");
    assert_eq!(
        Repository::open(cwd).unwrap().state(),
        git2::RepositoryState::ApplyMailbox
    );
}

#[test]
fn continue_during_git_am_advises_git_am_instead_of_retrying_a_rebase() {
    let (dir, repo) = paused_move_without_native_rebase();
    stop_git_am_on_target_patch(dir.path());

    let output = kin_cmd()
        .arg("continue")
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("git am --continue"), "stderr: {stderr}");
    assert!(
        !stderr.contains("git rebase --continue failed"),
        "stderr: {stderr}"
    );
    // Neither the paused move nor the am was disturbed.
    assert!(StateFile::Rebase.in_git_dir(repo.path()).exists());
    assert_eq!(repo.state(), git2::RepositoryState::ApplyMailbox);

    kin_cmd()
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("git am --abort"));
    assert!(StateFile::Rebase.in_git_dir(repo.path()).exists());
    assert_eq!(repo.state(), git2::RepositoryState::ApplyMailbox);

    // Once the am is gone, the paused move can be aborted as usual.
    run_ok("git", &["am", "--abort"], dir.path());
    kin_cmd()
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();
    assert!(!StateFile::Rebase.in_git_dir(repo.path()).exists());
}

#[test]
fn continue_and_abort_name_git_am_when_only_am_is_in_progress() {
    let (dir, repo) = paused_move_without_native_rebase();
    kin_cmd()
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();
    stop_git_am_on_target_patch(dir.path());

    for command in ["continue", "abort"] {
        let output = kin_cmd()
            .arg(command)
            .current_dir(dir.path())
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            text.contains(&format!("git am --{command}")),
            "{command}: {text}"
        );
        assert!(!text.contains("git rebase"), "{command}: {text}");
        assert_eq!(repo.state(), git2::RepositoryState::ApplyMailbox);
    }
}

/// `main` with `feature-a` -> `feature-b` stacked on it, and `other` changing
/// feature-b's file so rebasing feature-b onto it conflicts. The main
/// worktree is on feature-a; the linked worktree (`<returned dir>/linked`) is
/// on feature-b.
fn stack_with_linked_worktree() -> (tempfile::TempDir, tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());
    let base_id = make_commit(&repo, "refs/heads/main", "file.txt", "base", "initial", &[]);
    let base = repo.find_commit(base_id).unwrap();
    let a_id = make_commit(&repo, "refs/heads/feature-a", "a.txt", "a", "a", &[&base]);
    let a = repo.find_commit(a_id).unwrap();
    make_commit(&repo, "refs/heads/feature-b", "b.txt", "b", "b", &[&a]);
    make_commit(
        &repo,
        "refs/heads/other",
        "b.txt",
        "other",
        "other",
        &[&base],
    );
    run_ok("git", &["checkout", "-q", "-f", "feature-a"], dir.path());

    let linked = tempdir().unwrap();
    let linked_path = linked.path().join("linked");
    run_ok(
        "git",
        &[
            "worktree",
            "add",
            linked_path.to_str().unwrap(),
            "feature-b",
        ],
        dir.path(),
    );
    let reopened = Repository::open(dir.path()).unwrap();
    (dir, linked, reopened)
}

#[test]
fn move_refuses_branch_being_rebased_or_bisected_in_another_worktree() {
    for activity in ["rebased", "bisected"] {
        let (dir, linked, repo) = stack_with_linked_worktree();
        let linked_path = linked.path().join("linked");
        if activity == "rebased" {
            let rebase = common::git_command(&linked_path)
                .args(["rebase", "other"])
                .output()
                .unwrap();
            assert!(!rebase.status.success(), "rebase was expected to conflict");
        } else {
            common::start_native_bisect(&linked_path, "feature-b", "main");
        }
        // Git lists the linked worktree as detached while it works.
        let list = common::git_command(dir.path())
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        assert!(!String::from_utf8_lossy(&list.stdout).contains("refs/heads/feature-b"));

        let tips = |repo: &Repository| -> Vec<Oid> {
            ["feature-a", "feature-b"]
                .iter()
                .map(|name| repo.revparse_single(name).unwrap().id())
                .collect()
        };
        let tips_before = tips(&repo);
        kin_cmd()
            .args(["move", "--onto", "other"])
            .current_dir(dir.path())
            .assert()
            .failure()
            .stderr(predicates::str::contains(format!(
                "feature-b is being {activity} in"
            )));
        assert_eq!(
            tips_before,
            tips(&repo),
            "{activity}: branches were rewritten"
        );
        common::assert_no_kindra_operation(dir.path());
    }
}
