mod common;

use common::{git_command, kin_cmd, make_commit, repo_init, run_ok};
use git2::{BranchType, Repository};
use kindra::rebase_utils::{Operation, RebaseState, save_state};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn write_editor_script(dir: &Path, edited_content: &str) -> PathBuf {
    let edited_path = dir.join("edited-reorder.txt");
    fs::write(&edited_path, edited_content).unwrap();

    let script_path = dir.join("editor.sh");
    fs::write(
        &script_path,
        format!("#!/bin/sh\ncp \"{}\" \"$1\"\n", edited_path.display()),
    )
    .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).unwrap();
    }

    script_path
}

fn assert_direct_parent(repo: &Repository, branch_name: &str, parent_name: &str) {
    let branch_id = repo
        .find_branch(branch_name, BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let parent_id = repo.find_commit(branch_id).unwrap().parent_id(0).unwrap();
    let expected_parent_id = repo
        .find_branch(parent_name, BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    assert_eq!(
        parent_id, expected_parent_id,
        "branch '{}' did not end up directly on '{}'",
        branch_name, parent_name
    );
}

fn assert_direct_parent_id(repo: &Repository, branch_name: &str, expected_parent_id: git2::Oid) {
    let branch_id = repo
        .find_branch(branch_name, BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    let parent_id = repo.find_commit(branch_id).unwrap().parent_id(0).unwrap();
    assert_eq!(
        parent_id, expected_parent_id,
        "branch '{}' did not end up directly on '{}'",
        branch_name, expected_parent_id
    );
}

fn branch_tip(repo: &Repository, branch_name: &str) -> git2::Oid {
    repo.find_branch(branch_name, BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap()
}

#[test]
fn reorder_linear_stack() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(&repo, "refs/heads/main", "root.txt", "root", "root", &[]);
    let main = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(&repo, "refs/heads/feature-a", "a.txt", "a", "A", &[&main]);
    let a = repo.find_commit(a_id).unwrap();

    let b_id = make_commit(&repo, "refs/heads/feature-b", "b.txt", "b", "B", &[&a]);
    let b = repo.find_commit(b_id).unwrap();

    make_commit(&repo, "refs/heads/feature-c", "c.txt", "c", "C", &[&b]);

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    let editor = write_editor_script(
        dir.path(),
        "branch feature-c parent main\nbranch feature-a\nbranch feature-b\n",
    );

    kin_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &editor)
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();
    assert_direct_parent(&repo, "feature-c", "main");
    assert_direct_parent(&repo, "feature-a", "feature-c");
    assert_direct_parent(&repo, "feature-b", "feature-a");
}

#[test]
fn reorder_creates_fork() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(&repo, "refs/heads/main", "root.txt", "root", "root", &[]);
    let main = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(&repo, "refs/heads/feature-a", "a.txt", "a", "A", &[&main]);
    let a = repo.find_commit(a_id).unwrap();

    let b_id = make_commit(&repo, "refs/heads/feature-b", "b.txt", "b", "B", &[&a]);
    let b = repo.find_commit(b_id).unwrap();

    make_commit(&repo, "refs/heads/feature-c", "c.txt", "c", "C", &[&b]);

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    let editor = write_editor_script(
        dir.path(),
        "branch feature-a parent main\nbranch feature-b\nbranch feature-c parent feature-a\n",
    );

    kin_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &editor)
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();
    assert_direct_parent(&repo, "feature-a", "main");
    assert_direct_parent(&repo, "feature-b", "feature-a");
    assert_direct_parent(&repo, "feature-c", "feature-a");
}

#[test]
fn reorder_preserves_existing_fork() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(&repo, "refs/heads/main", "root.txt", "root", "root", &[]);
    let main = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(&repo, "refs/heads/feature-a", "a.txt", "a", "A", &[&main]);
    let a = repo.find_commit(a_id).unwrap();

    let b_id = make_commit(&repo, "refs/heads/feature-b", "b.txt", "b", "B", &[&a]);
    let b = repo.find_commit(b_id).unwrap();

    make_commit(&repo, "refs/heads/feature-d", "d.txt", "d", "D", &[&b]);
    make_commit(&repo, "refs/heads/feature-c", "c.txt", "c", "C", &[&a]);

    run_ok("git", &["checkout", "-f", "feature-b"], dir.path());
    let editor = write_editor_script(
        dir.path(),
        "branch feature-a parent main\nbranch feature-b\nbranch feature-c parent main\nbranch feature-d parent feature-b\n",
    );

    kin_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &editor)
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();
    assert_direct_parent(&repo, "feature-a", "main");
    assert_direct_parent(&repo, "feature-b", "feature-a");
    assert_direct_parent(&repo, "feature-c", "main");
    assert_direct_parent(&repo, "feature-d", "feature-b");
}

#[test]
fn reorder_restores_original_branch_when_run_from_middle() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(&repo, "refs/heads/main", "root.txt", "root", "root", &[]);
    let main = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(&repo, "refs/heads/feature-a", "a.txt", "a", "A", &[&main]);
    let a = repo.find_commit(a_id).unwrap();

    let b_id = make_commit(&repo, "refs/heads/feature-b", "b.txt", "b", "B", &[&a]);
    let b = repo.find_commit(b_id).unwrap();

    make_commit(&repo, "refs/heads/feature-c", "c.txt", "c", "C", &[&b]);

    run_ok("git", &["checkout", "-f", "feature-b"], dir.path());
    let editor = write_editor_script(
        dir.path(),
        "branch feature-c parent main\nbranch feature-a\nbranch feature-b\n",
    );

    kin_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &editor)
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();
    assert_eq!(repo.head().unwrap().shorthand(), Some("feature-b"));
    assert_direct_parent(&repo, "feature-c", "main");
    assert_direct_parent(&repo, "feature-a", "feature-c");
    assert_direct_parent(&repo, "feature-b", "feature-a");
}

#[test]
fn reorder_rejects_self_parent() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(&repo, "refs/heads/main", "root.txt", "root", "root", &[]);
    let main = repo.find_commit(main_id).unwrap();

    make_commit(&repo, "refs/heads/feature-a", "a.txt", "a", "A", &[&main]);

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    let editor = write_editor_script(dir.path(), "branch feature-a parent feature-a\n");

    kin_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &editor)
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "cannot list itself as its parent",
        ));
}

#[test]
fn reorder_rejects_shorthand_on_first_row() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(&repo, "refs/heads/main", "root.txt", "root", "root", &[]);
    let main = repo.find_commit(main_id).unwrap();

    make_commit(&repo, "refs/heads/feature-a", "a.txt", "a", "A", &[&main]);

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    let editor = write_editor_script(dir.path(), "branch feature-a\n");

    kin_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &editor)
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "first branch row must spell out its parent",
        ));
}

#[test]
fn reorder_rejects_unknown_parent() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(&repo, "refs/heads/main", "root.txt", "root", "root", &[]);
    let main = repo.find_commit(main_id).unwrap();

    make_commit(&repo, "refs/heads/feature-a", "a.txt", "a", "A", &[&main]);

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    let editor = write_editor_script(dir.path(), "branch feature-a parent nope\n");

    kin_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &editor)
        .assert()
        .failure()
        .stderr(predicates::str::contains("unknown parent"));
}

#[test]
fn reorder_rejects_cycle() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(&repo, "refs/heads/main", "root.txt", "root", "root", &[]);
    let main = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(&repo, "refs/heads/feature-a", "a.txt", "a", "A", &[&main]);
    let a = repo.find_commit(a_id).unwrap();

    make_commit(&repo, "refs/heads/feature-b", "b.txt", "b", "B", &[&a]);

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    let editor = write_editor_script(
        dir.path(),
        "branch feature-a parent feature-b\nbranch feature-b parent feature-a\n",
    );

    kin_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &editor)
        .assert()
        .failure()
        .stderr(predicates::str::contains("contains a cycle"));
}

#[test]
fn reorder_rejects_duplicate_or_missing_rows() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(&repo, "refs/heads/main", "root.txt", "root", "root", &[]);
    let main = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(&repo, "refs/heads/feature-a", "a.txt", "a", "A", &[&main]);
    let a = repo.find_commit(a_id).unwrap();

    make_commit(&repo, "refs/heads/feature-b", "b.txt", "b", "B", &[&a]);

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());

    let duplicate_editor = write_editor_script(
        dir.path(),
        "branch feature-a parent main\nbranch feature-a parent main\n",
    );
    kin_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &duplicate_editor)
        .assert()
        .failure()
        .stderr(predicates::str::contains("Duplicate branch row"));

    let missing_editor = write_editor_script(dir.path(), "branch feature-a parent main\n");
    kin_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &missing_editor)
        .assert()
        .failure()
        .stderr(predicates::str::contains("missing branch rows"));
}

#[test]
fn reorder_conflict_and_continue() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "1\n2\n3\n",
        "base",
        &[],
    );
    let main = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "file.txt",
        "1\nfeature-a\n3\n",
        "A",
        &[&main],
    );
    let a = repo.find_commit(a_id).unwrap();

    let b_id = make_commit(&repo, "refs/heads/feature-b", "b.txt", "b", "B", &[&a]);
    let b = repo.find_commit(b_id).unwrap();

    make_commit(&repo, "refs/heads/feature-c", "c.txt", "c", "C", &[&b]);

    run_ok("git", &["checkout", "-f", "main"], dir.path());
    fs::write(dir.path().join("file.txt"), "1\nmain\n3\n").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok("git", &["commit", "-m", "main commit"], dir.path());

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    let editor = write_editor_script(
        dir.path(),
        "branch feature-c parent main\nbranch feature-a\nbranch feature-b\n",
    );

    kin_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &editor)
        .assert()
        .failure()
        .stderr(predicates::str::contains("Resolve conflicts"));

    fs::write(dir.path().join("file.txt"), "1\nresolved\n3\n").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());

    kin_cmd()
        .arg("continue")
        .current_dir(dir.path())
        .env("GIT_EDITOR", "true")
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();
    assert_direct_parent(&repo, "feature-c", "main");
    assert_direct_parent(&repo, "feature-a", "feature-c");
    assert_direct_parent(&repo, "feature-b", "feature-a");
}

#[test]
fn reorder_conflict_and_abort_restores_original_graph_and_cleans_up() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "1\n2\n3\n",
        "base",
        &[],
    );
    let main = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "file.txt",
        "1\nfeature-a\n3\n",
        "A",
        &[&main],
    );
    let a = repo.find_commit(a_id).unwrap();

    let b_id = make_commit(&repo, "refs/heads/feature-b", "b.txt", "b", "B", &[&a]);
    let b = repo.find_commit(b_id).unwrap();

    let c_id = make_commit(&repo, "refs/heads/feature-c", "c.txt", "c", "C", &[&b]);
    let original_parent_feature_a = a.parent_id(0).unwrap();
    let original_parent_feature_b = b.parent_id(0).unwrap();
    let original_parent_feature_c = repo.find_commit(c_id).unwrap().parent_id(0).unwrap();

    run_ok("git", &["checkout", "-f", "main"], dir.path());
    fs::write(dir.path().join("file.txt"), "1\nmain\n3\n").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok("git", &["commit", "-m", "main commit"], dir.path());

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    let editor = write_editor_script(
        dir.path(),
        "branch feature-c parent main\nbranch feature-a\nbranch feature-b\n",
    );

    kin_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &editor)
        .assert()
        .failure()
        .stderr(predicates::str::contains("Resolve conflicts"));

    kin_cmd()
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();
    assert_direct_parent_id(&repo, "feature-c", original_parent_feature_c);
    assert_direct_parent_id(&repo, "feature-a", original_parent_feature_a);
    assert_direct_parent_id(&repo, "feature-b", original_parent_feature_b);
    assert!(!dir.path().join(".git/gits_rebase_state.json").exists());
    assert!(!dir.path().join(".git/rebase-merge").exists());
    assert!(!dir.path().join(".git/rebase-apply").exists());
}

#[test]
fn reorder_abort_restores_extra_local_refs_moved_by_update_refs() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(&repo, "refs/heads/main", "base.txt", "base\n", "base", &[]);
    let main = repo.find_commit(main_id).unwrap();

    let target_id = make_commit(
        &repo,
        "refs/heads/target",
        "target.txt",
        "target\n",
        "target",
        &[&main],
    );
    let target = repo.find_commit(target_id).unwrap();

    let feature_a_c1_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a1.txt",
        "a1\n",
        "feature a1",
        &[&main],
    );
    let feature_a_c1 = repo.find_commit(feature_a_c1_id).unwrap();
    let feature_a_tip_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "a2.txt",
        "a2\n",
        "feature a2",
        &[&feature_a_c1],
    );
    let feature_a_tip = repo.find_commit(feature_a_tip_id).unwrap();
    let feature_b_tip_id = make_commit(
        &repo,
        "refs/heads/feature-b",
        "b.txt",
        "b\n",
        "feature b",
        &[&feature_a_tip],
    );
    repo.branch("feature-bookmark", &feature_a_c1, false)
        .unwrap();

    let alias_tip_before = branch_tip(&repo, "feature-bookmark");

    let mut state = RebaseState {
        operation: Operation::Reorder,
        original_branch: "feature-a".to_string(),
        target_branch: "target".to_string(),
        caller_branch: None,
        remaining_branches: vec!["feature-a".to_string(), "feature-b".to_string()],
        in_progress_branch: Some("feature-a".to_string()),
        parent_id_map: HashMap::from([
            ("feature-a".to_string(), main_id.to_string()),
            ("feature-b".to_string(), feature_a_tip_id.to_string()),
        ]),
        parent_name_map: HashMap::from([("feature-b".to_string(), "feature-a".to_string())]),
        new_base_map: HashMap::new(),
        original_commit_count_map: HashMap::new(),
        original_tip_map: HashMap::from([
            ("feature-a".to_string(), feature_a_tip_id.to_string()),
            ("feature-b".to_string(), feature_b_tip_id.to_string()),
        ]),
        owned_tip_map: HashMap::new(),
        stash_ref: None,
        unstage_on_restore: false,
        autostash: false,
        cleanup_merged_branches: Vec::new(),
        cleanup_checkout_fallback: None,
    };
    save_state(&repo, &state).unwrap();

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    let first_rebase = git_command(dir.path())
        .args([
            "rebase",
            "--no-ff",
            "--no-autostash",
            "--update-refs",
            "--onto",
            "target",
            &main_id.to_string(),
            "feature-a",
        ])
        .output()
        .unwrap();
    assert!(
        first_rebase.status.success(),
        "first rebase should succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&first_rebase.stdout),
        String::from_utf8_lossy(&first_rebase.stderr)
    );

    let repo = Repository::open(dir.path()).unwrap();
    let alias_tip_after_first_rebase = branch_tip(&repo, "feature-bookmark");
    assert_ne!(
        alias_tip_after_first_rebase, alias_tip_before,
        "feature-bookmark should move with --update-refs before abort"
    );
    assert!(
        repo.graph_descendant_of(alias_tip_after_first_rebase, target.id())
            .unwrap()
    );

    state.remaining_branches = vec!["feature-b".to_string()];
    state.in_progress_branch = None;
    save_state(&repo, &state).unwrap();
    state.in_progress_branch = Some("feature-b".to_string());
    save_state(&repo, &state).unwrap();

    run_ok("git", &["checkout", "-f", "feature-b"], dir.path());
    let second_rebase = git_command(dir.path())
        .args([
            "rebase",
            "--no-ff",
            "--no-autostash",
            "--update-refs",
            "--exec",
            "false",
            "--onto",
            "feature-a",
            &feature_a_tip_id.to_string(),
            "feature-b",
        ])
        .output()
        .unwrap();
    assert!(
        !second_rebase.status.success(),
        "second rebase should stop mid-operation\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&second_rebase.stdout),
        String::from_utf8_lossy(&second_rebase.stderr)
    );
    assert!(
        dir.path().join(".git/rebase-merge").exists()
            || dir.path().join(".git/rebase-apply").exists(),
        "second rebase should remain in progress after --exec false"
    );

    kin_cmd()
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success();

    let repo = Repository::open(dir.path()).unwrap();
    assert_eq!(branch_tip(&repo, "feature-bookmark"), alias_tip_before);
    assert!(!dir.path().join(".git/gits_rebase_state.json").exists());
    assert!(!dir.path().join(".git/rebase-merge").exists());
    assert!(!dir.path().join(".git/rebase-apply").exists());
}

#[test]
fn reorder_manual_git_continue_then_abort_clears_state_without_rewinding_refs() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "1\n2\n3\n",
        "base",
        &[],
    );
    let main = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(
        &repo,
        "refs/heads/feature-a",
        "file.txt",
        "1\nfeature-a\n3\n",
        "A",
        &[&main],
    );
    let a = repo.find_commit(a_id).unwrap();

    let b_id = make_commit(&repo, "refs/heads/feature-b", "b.txt", "b", "B", &[&a]);
    let b = repo.find_commit(b_id).unwrap();

    make_commit(&repo, "refs/heads/feature-c", "c.txt", "c", "C", &[&b]);

    run_ok("git", &["checkout", "-f", "main"], dir.path());
    fs::write(dir.path().join("file.txt"), "1\nmain\n3\n").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok("git", &["commit", "-m", "main commit"], dir.path());

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    let editor = write_editor_script(
        dir.path(),
        "branch feature-c parent main\nbranch feature-a\nbranch feature-b\n",
    );

    kin_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &editor)
        .assert()
        .failure()
        .stderr(predicates::str::contains("Resolve conflicts"));

    fs::write(dir.path().join("file.txt"), "1\nresolved\n3\n").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok(
        "git",
        &["-c", "core.editor=true", "rebase", "--continue"],
        dir.path(),
    );

    let repo = Repository::open(dir.path()).unwrap();
    let feature_a_tip_before_abort = branch_tip(&repo, "feature-a");
    let feature_b_tip_before_abort = branch_tip(&repo, "feature-b");
    let feature_c_tip_before_abort = branch_tip(&repo, "feature-c");

    kin_cmd()
        .arg("abort")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "Kindra state cleared without restoring refs",
        ));

    assert_eq!(branch_tip(&repo, "feature-a"), feature_a_tip_before_abort);
    assert_eq!(branch_tip(&repo, "feature-b"), feature_b_tip_before_abort);
    assert_eq!(branch_tip(&repo, "feature-c"), feature_c_tip_before_abort);
    assert!(!dir.path().join(".git/gits_rebase_state.json").exists());
    assert!(!dir.path().join(".git/rebase-merge").exists());
    assert!(!dir.path().join(".git/rebase-apply").exists());
}

#[test]
fn reorder_checks_worktrees() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let main_id = make_commit(&repo, "refs/heads/main", "root.txt", "root", "root", &[]);
    let main = repo.find_commit(main_id).unwrap();

    let a_id = make_commit(&repo, "refs/heads/feature-a", "a.txt", "a", "A", &[&main]);
    let a = repo.find_commit(a_id).unwrap();

    let b_id = make_commit(&repo, "refs/heads/feature-b", "b.txt", "b", "B", &[&a]);
    let b = repo.find_commit(b_id).unwrap();

    make_commit(&repo, "refs/heads/feature-c", "c.txt", "c", "C", &[&b]);

    let wt_dir = tempdir().unwrap();
    run_ok(
        "git",
        &[
            "worktree",
            "add",
            wt_dir.path().to_str().unwrap(),
            "feature-c",
        ],
        dir.path(),
    );

    run_ok("git", &["checkout", "-f", "feature-a"], dir.path());
    let editor = write_editor_script(
        dir.path(),
        "branch feature-a parent feature-c\nbranch feature-b parent feature-a\nbranch feature-c parent main\n",
    );

    kin_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &editor)
        .assert()
        .failure()
        .stderr(predicates::str::contains("feature-c is checked out in"));
}
