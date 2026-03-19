mod common;

use common::{gits_cmd, make_commit, repo_init, run_ok};
use git2::{BranchType, Repository};
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

    gits_cmd()
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

    gits_cmd()
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

    gits_cmd()
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

    gits_cmd()
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

    gits_cmd()
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

    gits_cmd()
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

    gits_cmd()
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

    gits_cmd()
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
    gits_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &duplicate_editor)
        .assert()
        .failure()
        .stderr(predicates::str::contains("Duplicate branch row"));

    let missing_editor = write_editor_script(dir.path(), "branch feature-a parent main\n");
    gits_cmd()
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

    gits_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &editor)
        .assert()
        .failure()
        .stderr(predicates::str::contains("Resolve conflicts"));

    fs::write(dir.path().join("file.txt"), "1\nresolved\n3\n").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());

    gits_cmd()
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

    gits_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &editor)
        .assert()
        .failure()
        .stderr(predicates::str::contains("Resolve conflicts"));

    gits_cmd()
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

    gits_cmd()
        .arg("reorder")
        .current_dir(dir.path())
        .env("EDITOR", &editor)
        .assert()
        .failure()
        .stderr(predicates::str::contains("feature-c is checked out in"));
}
