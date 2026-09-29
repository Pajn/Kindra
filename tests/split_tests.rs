mod common;

use common::{kin_cmd, make_commit, rebase_state, rebase_state_file, repo_init};
use git2::{Repository, Signature};
use kindra::rebase_utils::{Operation, RebaseState, save_state};
use std::fs;
use tempfile::tempdir;

fn setup_repo() -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());
    repo.set_head("refs/heads/main").unwrap();

    let parent_id = make_commit(
        &repo,
        "refs/heads/main",
        "file.txt",
        "initial",
        "initial commit",
        &[],
    );

    let first_commit_id = parent_id;
    let mut current_parent_id = parent_id;

    // Create a stack of 3 commits
    for i in 1..=3 {
        let parent = repo.find_commit(current_parent_id).unwrap();
        current_parent_id = make_commit(
            &repo,
            "HEAD", // commit to HEAD (detached later)
            &format!("file{}.txt", i),
            &format!("content {}", i),
            &format!("commit {}", i),
            &[&parent],
        );
    }

    // Detach HEAD before moving main
    repo.set_head_detached(current_parent_id).unwrap();

    {
        // Reset main to the first commit
        let first_commit = repo.find_commit(first_commit_id).unwrap();
        repo.branch("main", &first_commit, true).unwrap();
    }

    {
        // Clean up working directory to avoid checkout conflicts
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
    }

    (dir, repo)
}

#[test]
fn test_split_move_branch() {
    let (dir, repo) = setup_repo();

    // Create an initial branch at the tip
    {
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature-x", &head, false).unwrap();
    }
    repo.set_head("refs/heads/feature-x").unwrap();

    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
perl -i -pe 's/.*branch feature-x.*\n?//g' "$file"
perl -i -pe 's/(commit 2)/$1\nbranch feature-x/' "$file"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&editor_script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&editor_script, perms).unwrap();
    }

    let mut cmd = kin_cmd();
    cmd.arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor_script)
        .assert()
        .success();

    // Verify branch moved
    let branch = repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap();
    let target = branch.get().target().unwrap();
    let commit = repo.find_commit(target).unwrap();
    assert_eq!(commit.summary().unwrap(), "commit 2");
}

#[test]
fn test_split_create_delete_branch() {
    let (dir, repo) = setup_repo();

    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
perl -i -pe 's/(commit 1)/$1\nbranch new-feat/' "$file"
perl -i -pe 's/(commit 3)/$1\nbranch another-feat/' "$file"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&editor_script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&editor_script, perms).unwrap();
    }

    let mut cmd = kin_cmd();
    cmd.arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor_script)
        .assert()
        .success();

    assert!(
        repo.find_branch("new-feat", git2::BranchType::Local)
            .is_ok()
    );
    assert!(
        repo.find_branch("another-feat", git2::BranchType::Local)
            .is_ok()
    );

    // Now delete 'new-feat'
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
perl -i -pe 's/.*branch new-feat.*\n?//g' "$file"
"#,
    )
    .unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor_script)
        .assert()
        .success();

    assert!(
        repo.find_branch("new-feat", git2::BranchType::Local)
            .is_err()
    );
    assert!(
        repo.find_branch("another-feat", git2::BranchType::Local)
            .is_ok()
    );
}

#[test]
fn test_split_error_on_commit_mod() {
    let (dir, _repo) = setup_repo();

    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
perl -i -pe 's/^[0-9a-f]{7}/deadbee/' "$file"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&editor_script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&editor_script, perms).unwrap();
    }

    let mut cmd = kin_cmd();
    cmd.arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor_script)
        .assert()
        .failure()
        .stderr(predicates::str::contains("modified or moved"));
}

#[test]
fn test_split_detach_head_on_delete() {
    let (dir, repo) = setup_repo();

    {
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("current", &head, false).unwrap();
    }
    repo.set_head("refs/heads/current").unwrap();

    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
perl -i -pe 's/.*branch current.*\n?//g' "$file"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&editor_script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&editor_script, perms).unwrap();
    }

    let mut cmd = kin_cmd();
    cmd.arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor_script)
        .assert()
        .success();

    assert!(repo.head_detached().unwrap());
    assert!(
        repo.find_branch("current", git2::BranchType::Local)
            .is_err()
    );
}

#[test]
fn test_split_checkout_branch_at_current_commit() {
    let (dir, repo) = setup_repo();

    let head = repo.head().unwrap().peel_to_commit().unwrap();
    let head_id = head.id();

    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
perl -i -pe 's/(commit 3)/$1\nbranch new-feat/' "$file"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&editor_script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&editor_script, perms).unwrap();
    }

    let mut cmd = kin_cmd();
    cmd.arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor_script)
        .assert()
        .success();

    assert!(!repo.head_detached().unwrap());
    let branch = repo
        .find_branch("new-feat", git2::BranchType::Local)
        .unwrap();
    assert_eq!(branch.get().target().unwrap(), head_id);
}

#[test]
fn test_push_multiple_remotes_no_origin_when_stack_empty() {
    let (dir, repo) = setup_repo();

    // Setup two remotes, neither is origin
    repo.remote("remote1", "http://example.com/r1.git").unwrap();
    repo.remote("remote2", "http://example.com/r2.git").unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("push")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("No branches in stack to push."));
}

#[test]
fn test_push_no_remotes_when_stack_empty() {
    let (dir, _repo) = setup_repo();
    // No remotes by default from setup_repo (except if we added any)

    let mut cmd = kin_cmd();
    cmd.arg("push")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("No branches in stack to push."));
}

#[test]
fn test_checkout_up_fork() {
    let (dir, repo) = setup_repo();

    // c1 is an ancestor.
    // We want to be on a branch at c1, and have two successors.
    let c1_id = repo.revparse_single("HEAD~2").unwrap().id();
    let c2_id = repo.revparse_single("HEAD~1").unwrap().id();
    let head_id = repo.head().unwrap().peel_to_commit().unwrap().id();

    // Create two independent paths from c1
    {
        let c1 = repo.find_commit(c1_id).unwrap();
        let c2 = repo.find_commit(c2_id).unwrap();
        let head = repo.find_commit(head_id).unwrap();

        // fork-a is head (descendant of head_id)
        repo.branch("fork-a", &head, false).unwrap();

        // fork-b is a NEW commit from c1
        let tree = c2.tree().unwrap();
        let sig = Signature::now("Test User", "test@example.com").unwrap();
        let fork_b_id = repo
            .commit(None, &sig, &sig, "fork-b commit", &tree, &[&c1])
            .unwrap();
        let fork_b = repo.find_commit(fork_b_id).unwrap();
        repo.branch("fork-b", &fork_b, false).unwrap();

        // Current branch is 'base' at c1
        repo.branch("base", &c1, false).unwrap();

        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
    }
    repo.set_head("refs/heads/base").unwrap();
    fs::remove_file(dir.path().join("file.txt")).unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("checkout")
        .arg("up")
        .current_dir(dir.path())
        .env("TERM", "dumb")
        .env("KIN_TEST_SELECTIONS", "0")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "test override: auto-selecting option",
        ));

    let new_head = repo.head().unwrap().shorthand().unwrap().to_string();
    assert!(
        new_head == "fork-a" || new_head == "fork-b",
        "Expected fork-a or fork-b, but got: {}",
        new_head
    );
}

#[test]
fn test_checkout_top_fork() {
    let (dir, repo) = setup_repo();

    // Create two tips
    {
        let c1_id = repo.revparse_single("HEAD~2").unwrap().id();
        let c2_id = repo.revparse_single("HEAD~1").unwrap().id();
        let head_id = repo.head().unwrap().peel_to_commit().unwrap().id();
        let c1 = repo.find_commit(c1_id).unwrap();
        let c2 = repo.find_commit(c2_id).unwrap();
        let head = repo.find_commit(head_id).unwrap();

        // tip-a is head
        repo.branch("tip-a", &head, false).unwrap();

        // tip-b is a NEW commit from c1
        let tree = c2.tree().unwrap();
        let sig = Signature::now("Test User", "test@example.com").unwrap();
        let tip_b_id = repo
            .commit(None, &sig, &sig, "tip-b commit", &tree, &[&c1])
            .unwrap();
        let tip_b = repo.find_commit(tip_b_id).unwrap();
        repo.branch("tip-b", &tip_b, false).unwrap();

        // Ensure working directory is clean
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();

        // Current branch is 'base' at c1
        repo.branch("base", &c1, false).unwrap();
    }
    repo.set_head("refs/heads/base").unwrap();
    fs::remove_file(dir.path().join("file.txt")).unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("checkout")
        .arg("top")
        .current_dir(dir.path())
        .env("TERM", "dumb")
        .env("KIN_TEST_SELECTIONS", "0")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "test override: auto-selecting option",
        ));

    let new_head = repo.head().unwrap().shorthand().unwrap().to_string();
    assert!(
        new_head == "tip-a" || new_head == "tip-b",
        "Expected tip-a or tip-b, but got: {}",
        new_head
    );
}

#[test]
fn test_checkout_down_moves_to_immediate_parent_branch() {
    let (dir, repo) = setup_repo();

    let feature_a_id = repo.revparse_single("HEAD~2").unwrap().id();
    let feature_b_id = repo.revparse_single("HEAD~1").unwrap().id();
    let feature_c_id = repo.head().unwrap().peel_to_commit().unwrap().id();

    let feature_a = repo.find_commit(feature_a_id).unwrap();
    let feature_b = repo.find_commit(feature_b_id).unwrap();
    let feature_c = repo.find_commit(feature_c_id).unwrap();

    repo.branch("feature-a", &feature_a, false).unwrap();
    repo.branch("feature-b", &feature_b, false).unwrap();
    repo.branch("feature-c", &feature_c, false).unwrap();

    repo.set_head("refs/heads/feature-b").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("checkout")
        .arg("down")
        .current_dir(dir.path())
        .assert()
        .success();

    let new_head = repo.head().unwrap().shorthand().unwrap().to_string();
    assert_eq!(new_head, "feature-a");
}

#[test]
fn test_checkout_down_from_bottom_branch_goes_to_upstream() {
    let (dir, repo) = setup_repo();

    let feature_a_id = repo.revparse_single("HEAD~2").unwrap().id();
    let feature_b_id = repo.revparse_single("HEAD~1").unwrap().id();
    let feature_c_id = repo.head().unwrap().peel_to_commit().unwrap().id();

    let feature_a = repo.find_commit(feature_a_id).unwrap();
    let feature_b = repo.find_commit(feature_b_id).unwrap();
    let feature_c = repo.find_commit(feature_c_id).unwrap();

    repo.branch("feature-a", &feature_a, false).unwrap();
    repo.branch("feature-b", &feature_b, false).unwrap();
    repo.branch("feature-c", &feature_c, false).unwrap();

    repo.set_head("refs/heads/feature-a").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("checkout")
        .arg("down")
        .current_dir(dir.path())
        .assert()
        .success();

    let new_head = repo.head().unwrap().shorthand().unwrap().to_string();
    assert_eq!(new_head, "main");
}

#[test]
fn test_split_fork_selection() {
    let (dir, repo) = setup_repo();

    // Create two tips
    {
        let c1_id = repo.revparse_single("HEAD~2").unwrap().id();
        let c2_id = repo.revparse_single("HEAD~1").unwrap().id();
        let head_id = repo.head().unwrap().peel_to_commit().unwrap().id();
        let head = repo.find_commit(head_id).unwrap();
        let c2 = repo.find_commit(c2_id).unwrap();
        let c1 = repo.find_commit(c1_id).unwrap();

        // path-a is head
        repo.branch("path-a", &head, false).unwrap();

        // path-b is a NEW commit from c1
        let tree = c2.tree().unwrap();
        let sig = Signature::now("Test User", "test@example.com").unwrap();
        let path_b_id = repo
            .commit(None, &sig, &sig, "path-b commit", &tree, &[&c1])
            .unwrap();
        let path_b = repo.find_commit(path_b_id).unwrap();
        repo.branch("path-b", &path_b, false).unwrap();

        // Ensure we are at base (c1) to see both tips
        repo.set_head_detached(c1.id()).unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
    }

    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&editor_script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&editor_script, perms).unwrap();
    }

    let mut cmd = kin_cmd();
    cmd.arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor_script)
        .env("TERM", "dumb")
        .env("KIN_TEST_SELECTIONS", "0")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "test override: auto-selecting option",
        ));
}

#[test]
fn test_checkout_all_works_without_main() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());
    let signature = Signature::now("Test User", "test@example.com").unwrap();

    fs::write(dir.path().join("file.txt"), "initial").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("file.txt")).unwrap();
    let oid = index.write_tree().unwrap();
    let tree = repo.find_tree(oid).unwrap();
    repo.commit(
        Some("refs/heads/trunk"),
        &signature,
        &signature,
        "initial commit",
        &tree,
        &[],
    )
    .unwrap();

    repo.set_head("refs/heads/trunk").unwrap();
    fs::remove_file(dir.path().join("file.txt")).unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("checkout")
        .arg("--all")
        .current_dir(dir.path())
        .env("TERM", "dumb")
        .assert()
        .success()
        .stdout(predicates::str::contains("only one option available"));

    let new_head = repo.head().unwrap().shorthand().unwrap().to_string();
    assert_eq!(new_head, "trunk");
}

#[test]
fn test_checkout_all_detached_no_main() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());
    let signature = Signature::now("Test User", "test@example.com").unwrap();

    fs::write(dir.path().join("file.txt"), "initial").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("file.txt")).unwrap();
    let oid = index.write_tree().unwrap();
    let tree = repo.find_tree(oid).unwrap();
    let commit_id = repo
        .commit(
            Some("refs/heads/trunk"),
            &signature,
            &signature,
            "initial commit",
            &tree,
            &[],
        )
        .unwrap();

    // Detach HEAD
    repo.set_head_detached(commit_id).unwrap();
    fs::remove_file(dir.path().join("file.txt")).unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("checkout")
        .arg("--all")
        .current_dir(dir.path())
        .env("TERM", "dumb")
        .assert()
        .success()
        .stdout(predicates::str::contains("only one option available"));

    let new_head = repo.head().unwrap().shorthand().unwrap().to_string();
    assert_eq!(new_head, "trunk");
}

#[test]
fn test_checkout_all_ignores_kin_test_selection_override() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());
    let signature = Signature::now("Test User", "test@example.com").unwrap();

    fs::write(dir.path().join("file.txt"), "initial").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("file.txt")).unwrap();
    let oid = index.write_tree().unwrap();
    let tree = repo.find_tree(oid).unwrap();
    let trunk_id = repo
        .commit(
            Some("refs/heads/trunk"),
            &signature,
            &signature,
            "initial commit",
            &tree,
            &[],
        )
        .unwrap();
    let trunk_commit = repo.find_commit(trunk_id).unwrap();
    repo.branch("zzz-side", &trunk_commit, false).unwrap();

    repo.set_head("refs/heads/trunk").unwrap();
    fs::remove_file(dir.path().join("file.txt")).unwrap();

    let mut cmd = kin_cmd();
    cmd.arg("checkout")
        .arg("--all")
        .current_dir(dir.path())
        .env("KIN_TEST_SELECTION", "1")
        .env("TERM", "dumb")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Cannot choose between 2 options without a terminal",
        ));

    // The singular KIN_TEST_SELECTION var is not honored (only the plural
    // KIN_TEST_SELECTIONS is), so with two branches and no terminal the command
    // refuses to guess and leaves HEAD untouched instead of picking zzz-side.
    let new_head = repo.head().unwrap().shorthand().unwrap().to_string();
    assert_eq!(new_head, "trunk");
}

#[test]
fn test_split_invalid_edit_validation() {
    let (dir, _repo) = setup_repo();

    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
# Put a branch at the very top of the file, before any commits
echo "branch invalid-move" > "$file.tmp"
cat "$file" >> "$file.tmp"
mv "$file.tmp" "$file"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&editor_script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&editor_script, perms).unwrap();
    }

    let mut cmd = kin_cmd();
    cmd.arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor_script)
        .assert()
        .failure()
        .stderr(predicates::str::contains("must follow a commit line"));

    // Verify state file does NOT exist
    assert!(!rebase_state_file(dir.path()).exists());
}

#[test]
fn test_split_refuses_when_kindra_operation_in_progress() {
    let (dir, repo) = setup_repo();

    // A branch that split would delete if it were allowed to run.
    {
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature-b", &head, false).unwrap();
    }
    repo.set_head("refs/heads/feature-b").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Simulate an interrupted Kindra operation. With no parent maps recorded,
    // reconciliation cannot prove the branch is done, so the state is treated as
    // active and any mutating command must refuse.
    let state = RebaseState {
        remaining_branches: vec!["feature-b".to_string()],
        in_progress_branch: Some("feature-b".to_string()),
        ..rebase_state(Operation::Move, "feature-b", "main")
    };
    save_state(&repo, &state).unwrap();

    // An editor that would happily rewrite the buffer if split ever reached it.
    let editor_script = dir.path().join("editor.sh");
    fs::write(&editor_script, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&editor_script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&editor_script, perms).unwrap();
    }

    kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor_script)
        .env("TERM", "dumb")
        .assert()
        .failure()
        .stderr(predicates::str::contains("already in progress"));

    // The interrupted operation's state is untouched and no branches were changed.
    assert!(rebase_state_file(dir.path()).exists());
    assert!(
        repo.find_branch("feature-b", git2::BranchType::Local)
            .is_ok()
    );
}

#[test]
fn test_split_does_not_reattach_head_to_skipped_overwrite_branch() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let m = make_commit(&repo, "refs/heads/main", "m.txt", "m", "init", &[]);
    let mc = repo.find_commit(m).unwrap();
    let c1 = make_commit(&repo, "refs/heads/current", "c1.txt", "1", "c1", &[&mc]);
    let c1c = repo.find_commit(c1).unwrap();
    make_commit(&repo, "refs/heads/current", "c2.txt", "2", "c2", &[&c1c]);

    // A sibling branch off main, unrelated to the stack, whose desired commit in
    // the editor will be c2 (== the detached HEAD) even though it will be skipped.
    let outside_id = make_commit(&repo, "refs/heads/outside", "o.txt", "o", "outside", &[&mc]);

    repo.set_head("refs/heads/current").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    // Move 'current' from c2 down to c1, and assign the out-of-stack 'outside' to
    // c2. 'outside' is unsafe to overwrite, so in a non-interactive run it is
    // skipped — but its editor entry still points at c2 (the detached HEAD).
    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
perl -i -ne 'print unless /^branch current$/' "$file"
perl -i -pe 's/^([0-9a-f]{7} c1)$/$1\nbranch current/' "$file"
perl -i -pe 's/^([0-9a-f]{7} c2)$/$1\nbranch outside/' "$file"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&editor_script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&editor_script, perms).unwrap();
    }

    kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor_script)
        .env("TERM", "dumb")
        .assert()
        .success()
        .stdout(predicates::str::contains("Skipping branch 'outside'"));

    // HEAD must NOT be reattached to the skipped 'outside' branch just because its
    // desired commit equalled the detached HEAD; it should stay detached.
    assert!(
        repo.head_detached().unwrap(),
        "HEAD should remain detached rather than attach to the skipped 'outside' branch"
    );
    let outside = repo
        .find_branch("outside", git2::BranchType::Local)
        .unwrap();
    assert_eq!(
        outside.get().target().unwrap(),
        outside_id,
        "skipped 'outside' branch should still point at its original commit"
    );
    let current = repo
        .find_branch("current", git2::BranchType::Local)
        .unwrap();
    assert_eq!(
        current.get().target().unwrap(),
        c1,
        "'current' should have been moved down to c1"
    );
}

/// Locate the split recovery draft (named `split-*.md`) if present.
fn split_draft_file(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let drafts = dir.join(".git").join("kindra-drafts");
    for entry in fs::read_dir(&drafts).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("split-") && name.ends_with(".md") {
            return Some(entry.path());
        }
    }
    None
}

fn make_executable(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }
}

#[test]
fn split_success_discards_draft() {
    let (dir, repo) = setup_repo();

    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
perl -i -pe 's/(commit 1)/$1\nbranch new-feat/' "$file"
"#,
    )
    .unwrap();
    make_executable(&editor_script);

    kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor_script)
        .assert()
        .success();

    assert!(
        repo.find_branch("new-feat", git2::BranchType::Local)
            .is_ok(),
        "the split should have created the branch"
    );
    assert!(
        split_draft_file(dir.path()).is_none(),
        "split draft should be discarded after a successful split"
    );
}

#[test]
fn split_failure_preserves_draft_and_prints_guidance() {
    let (dir, _repo) = setup_repo();

    // Mutating a commit SHA makes split_from_buffer's validation fail.
    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
perl -i -pe 's/^[0-9a-f]{7}/deadbee/' "$file"
"#,
    )
    .unwrap();
    make_executable(&editor_script);

    let output = kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor_script)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "split should fail on a modified commit"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("re-run `kin split`"),
        "failure should print rerun guidance, got:\n{stderr}"
    );
    assert!(
        split_draft_file(dir.path()).is_some(),
        "split draft should be preserved on failure for recovery"
    );
}

/// An editor script kept in the Git directory, so it is not part of the
/// working tree the tests compare.
fn git_dir_editor(repo: &Repository, body: &str) -> std::path::PathBuf {
    let path = repo.path().join("editor.sh");
    fs::write(&path, format!("#!/bin/sh\nfile=$1\n{body}")).unwrap();
    make_executable(&path);
    path
}

fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
    let output = common::git_command(dir).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The index entries, the staged and unstaged diffs, the untracked files'
/// contents and the stash stack, byte for byte.
fn dirty_tree_snapshot(dir: &std::path::Path) -> String {
    format!(
        "index:\n{}staged:\n{}unstaged:\n{}status:\n{}untracked: {:?}\nstashes:\n{}",
        git_out(dir, &["ls-files", "--stage"]),
        git_out(dir, &["diff", "--cached", "--binary"]),
        git_out(dir, &["diff", "--binary"]),
        git_out(dir, &["status", "--porcelain=v1", "--untracked-files=all"]),
        fs::read(dir.join("untracked.txt")).unwrap(),
        git_out(dir, &["stash", "list"]),
    )
}

/// Every ref and its reflog, what HEAD names and its reflog, and the undo log.
fn refs_snapshot(dir: &std::path::Path) -> String {
    let repo = Repository::open(dir).unwrap();
    let head = if repo.head_detached().unwrap() {
        git_out(dir, &["rev-parse", "HEAD"])
    } else {
        git_out(dir, &["symbolic-ref", "HEAD"])
    };
    let mut snapshot = format!("HEAD: {head}");
    let refs = git_out(dir, &["for-each-ref", "--format=%(refname)"]);
    for name in std::iter::once("HEAD").chain(refs.lines()) {
        snapshot.push_str(&format!(
            "{name} {}{}",
            git_out(dir, &["rev-parse", name]),
            git_out(dir, &["reflog", "show", "--format=%H %gs", name, "--"]),
        ));
    }
    for file in ["kindra_oplog.json", "kindra_oplog_pending.json"] {
        let content = fs::read_to_string(repo.path().join(file)).ok();
        snapshot.push_str(&format!("{file}: {content:?}\n"));
    }
    snapshot
}

/// `setup_repo`'s stack with `feature-x` checked out at its tip, a staged
/// change, an unstaged change on top of it and in another file, and an
/// untracked file.
fn dirty_feature_repo() -> (tempfile::TempDir, Repository) {
    let (dir, repo) = setup_repo();
    {
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature-x", &head, false).unwrap();
    }
    repo.set_head("refs/heads/feature-x").unwrap();
    fs::write(dir.path().join("file.txt"), "staged").unwrap();
    common::run_ok("git", &["add", "file.txt"], dir.path());
    fs::write(dir.path().join("file.txt"), "staged\nunstaged on top").unwrap();
    fs::write(dir.path().join("file1.txt"), "unstaged").unwrap();
    fs::write(dir.path().join("untracked.txt"), "untracked").unwrap();
    (dir, repo)
}

/// Split only changes refs at HEAD's own commit, so it leaves uncommitted
/// changes exactly as they are: staged changes stay staged, and nothing is
/// set aside. `flags` are the autostash flags, which have no effect.
fn assert_split_keeps_dirty_tree(flags: &[&str], configured_autostash: bool) -> String {
    let (dir, repo) = dirty_feature_repo();
    repo.config()
        .unwrap()
        .set_bool("rebase.autostash", configured_autostash)
        .unwrap();
    let before = dirty_tree_snapshot(dir.path());
    let editor = git_dir_editor(
        &repo,
        "perl -i -pe 's/.*branch feature-x.*\\n?//g' \"$file\"\n\
         perl -i -pe 's/(commit 2)/$1\\nbranch feature-x/' \"$file\"\n",
    );

    let output = kin_cmd()
        .arg("split")
        .args(flags)
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{stderr}");

    let branch = repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap();
    let commit = repo.find_commit(branch.get().target().unwrap()).unwrap();
    assert_eq!(commit.summary().unwrap(), "commit 2");
    assert_eq!(dirty_tree_snapshot(dir.path()), before);
    assert!(!rebase_state_file(dir.path()).exists());
    stderr
}

const AUTOSTASH_NOTE: &str = "has no effect: kin split never touches uncommitted changes";

/// A dirty tree is no longer refused, and needs no permission to stay as it
/// is: `rebase.autostash` is ignored.
#[test]
fn test_split_leaves_a_dirty_working_tree_as_it_is() {
    let stderr = assert_split_keeps_dirty_tree(&[], false);
    assert!(!stderr.contains(AUTOSTASH_NOTE), "{stderr}");
    assert_split_keeps_dirty_tree(&[], true);
}

/// `--autostash` used to stash the tree and restore it plainly, turning staged
/// changes into unstaged ones. It is now accepted with a note and does nothing.
#[test]
fn test_split_autostash_flag_is_a_no_op_with_a_note() {
    let stderr = assert_split_keeps_dirty_tree(&["--autostash"], false);
    assert!(
        stderr.contains(&format!("Note: --autostash {AUTOSTASH_NOTE}.")),
        "{stderr}"
    );
}

/// `--no-autostash` used to refuse a dirty tree even with autostash
/// configured. It is now accepted with a note and does nothing.
#[test]
fn test_split_no_autostash_flag_is_a_no_op_with_a_note() {
    let stderr = assert_split_keeps_dirty_tree(&["--no-autostash"], true);
    assert!(
        stderr.contains(&format!("Note: --no-autostash {AUTOSTASH_NOTE}.")),
        "{stderr}"
    );
}

/// A branch that cannot be created next to an existing ref (`foo/bar` while
/// `foo` exists) is found before any ref changes, so the split fails with
/// nothing to roll back and the dirty tree untouched.
#[test]
fn split_ref_conflict_fails_before_changing_anything() {
    let (dir, repo) = dirty_feature_repo();
    let main_commit = repo
        .revparse_single("main")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    repo.branch("foo", &main_commit, false).unwrap();
    let before = refs_snapshot(dir.path());
    let tree_before = dirty_tree_snapshot(dir.path());
    let editor = git_dir_editor(
        &repo,
        "perl -i -pe 's/.*branch feature-x.*\\n?//g' \"$file\"\n\
         perl -i -pe 's/(commit 2)/$1\\nbranch feature-x/' \"$file\"\n\
         perl -i -pe 's{(commit 1)}{$1\\nbranch foo/bar}' \"$file\"\n",
    );

    let output = kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stdout}{stderr}");
    assert!(
        stderr.contains("Cannot create branch 'foo/bar': it conflicts with 'refs/heads/foo'"),
        "{stderr}"
    );
    assert!(stderr.contains("No branch was changed."), "{stderr}");
    assert!(!stdout.contains("Moved branch"), "{stdout}");
    assert_eq!(refs_snapshot(dir.path()), before);
    assert_eq!(dirty_tree_snapshot(dir.path()), tree_before);
}

/// Every ref a split changes is locked before any is written, so a ref that
/// cannot be locked (another Git process holds `<ref>.lock`) fails the split
/// with every branch, HEAD and their reflogs as they were.
#[test]
fn split_locked_ref_leaves_every_ref_and_head_unchanged() {
    let (dir, repo) = dirty_feature_repo();
    let commit_1 = repo
        .revparse_single("feature-x~2")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    repo.branch("feature-old", &commit_1, false).unwrap();
    // Held by "another process": the split has to delete `feature-old`.
    fs::write(repo.path().join("refs/heads/feature-old.lock"), "").unwrap();

    let before = refs_snapshot(dir.path());
    let tree_before = dirty_tree_snapshot(dir.path());
    // Move the checked-out `feature-x` (which detaches HEAD) and delete
    // `feature-old`.
    let editor = git_dir_editor(
        &repo,
        "perl -i -pe 's/.*branch feature-(x|old).*\\n?//g' \"$file\"\n\
         perl -i -pe 's/(commit 2)/$1\\nbranch feature-x/' \"$file\"\n",
    );

    let output = kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stdout}{stderr}");
    assert!(stderr.contains("No branch was changed."), "{stderr}");
    assert!(!stdout.contains("Moved branch"), "{stdout}");
    assert_eq!(refs_snapshot(dir.path()), before);
    assert_eq!(dirty_tree_snapshot(dir.path()), tree_before);
    assert!(repo.path().join("refs/heads/feature-old.lock").exists());
}

/// HEAD leaving its moved branch is logged once, as the checkout Git would
/// log, and the moved branch logs the split. A deleted branch leaves no
/// reflog or `branch.<name>` configuration behind, as with `git branch -D`.
#[test]
fn split_logs_ref_changes_and_forgets_deleted_branches() {
    let (dir, repo) = setup_repo();
    let tip = repo.head().unwrap().peel_to_commit().unwrap().id();
    repo.branch("feature-x", &repo.find_commit(tip).unwrap(), false)
        .unwrap();
    repo.set_head("refs/heads/feature-x").unwrap();
    let head_log = |dir: &std::path::Path| {
        git_out(dir, &["reflog", "show", "--format=%H %gs", "HEAD"])
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let head_log_before = head_log(dir.path());
    let editor = git_dir_editor(
        &repo,
        "perl -i -pe 's/.*branch feature-x.*\\n?//g' \"$file\"\n\
         perl -i -pe 's/(commit 2)/$1\\nbranch feature-x/' \"$file\"\n\
         perl -i -pe 's/(commit 1)/$1\\nbranch part-1/' \"$file\"\n",
    );
    kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor)
        .assert()
        .success()
        .stdout(predicates::str::contains("HEAD is detached at"));

    let commit_2 = repo.revparse_single("feature-x").unwrap().id();
    let head_log_after = head_log(dir.path());
    assert_eq!(head_log_after.len(), head_log_before.len() + 1);
    assert_eq!(
        head_log_after[0],
        format!("{tip} checkout: moving from feature-x to {tip}")
    );
    assert_eq!(
        git_out(
            dir.path(),
            &["reflog", "show", "-1", "--format=%H %gs", "feature-x"]
        )
        .trim(),
        format!("{commit_2} kin split: move branch at {commit_2}")
    );

    common::run_ok(
        "git",
        &["config", "branch.part-1.remote", "origin"],
        dir.path(),
    );
    let editor = git_dir_editor(
        &repo,
        "perl -i -pe 's/.*branch part-1.*\\n?//g' \"$file\"\n",
    );
    kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor)
        .assert()
        .success()
        .stdout(predicates::str::contains("Deleted branch: part-1"));
    assert!(repo.find_branch("part-1", git2::BranchType::Local).is_err());
    assert!(!repo.path().join("logs/refs/heads/part-1").exists());
    let config = Repository::open(dir.path()).unwrap().config().unwrap();
    assert!(config.get_string("branch.part-1.remote").is_err());
}

/// A split is recorded for `kin undo`, which puts the branches back.
#[test]
fn split_can_be_undone() {
    let (dir, repo) = setup_repo();
    let tip = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature-x", &tip, false).unwrap();
    repo.set_head("refs/heads/feature-x").unwrap();
    let editor = git_dir_editor(
        &repo,
        "perl -i -pe 's/.*branch feature-x.*\\n?//g' \"$file\"\n\
         perl -i -pe 's/(commit 2)/$1\\nbranch feature-x/' \"$file\"\n\
         perl -i -pe 's/(commit 1)/$1\\nbranch part-1/' \"$file\"\n",
    );

    kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor)
        .assert()
        .success();
    assert_ne!(
        repo.revparse_single("feature-x").unwrap().id(),
        tip.id(),
        "the split should have moved feature-x"
    );

    kin_cmd()
        .arg("undo")
        .current_dir(dir.path())
        .assert()
        .success();
    assert_eq!(repo.revparse_single("feature-x").unwrap().id(), tip.id());
    assert!(repo.find_branch("part-1", git2::BranchType::Local).is_err());
}

/// Git refuses to delete a branch another worktree has checked out; split
/// checks that before changing anything.
#[test]
fn split_refuses_to_delete_a_branch_checked_out_in_another_worktree() {
    let (dir, repo) = setup_repo();
    let commit_1 = repo
        .revparse_single("HEAD~2")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    repo.branch("elsewhere", &commit_1, false).unwrap();
    let tip = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature-x", &tip, false).unwrap();
    repo.set_head("refs/heads/feature-x").unwrap();
    let other = tempdir().unwrap();
    let other_path = other.path().join("other");
    common::run_ok(
        "git",
        &["worktree", "add", other_path.to_str().unwrap(), "elsewhere"],
        dir.path(),
    );
    let before = refs_snapshot(dir.path());
    let editor = git_dir_editor(
        &repo,
        "perl -i -pe 's/.*branch (elsewhere|feature-x).*\\n?//g' \"$file\"\n\
         perl -i -pe 's/(commit 2)/$1\\nbranch feature-x/' \"$file\"\n",
    );

    let output = kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("Cannot delete branch 'elsewhere': it is checked out in"),
        "{stderr}"
    );
    assert_eq!(refs_snapshot(dir.path()), before);
}

/// Check `branch` out in a new linked worktree, kept until the returned
/// directory is dropped.
fn check_out_elsewhere(dir: &std::path::Path, branch: &str) -> tempfile::TempDir {
    let other = tempdir().unwrap();
    let path = other.path().join("other");
    common::run_ok(
        "git",
        &["worktree", "add", path.to_str().unwrap(), branch],
        dir,
    );
    other
}

/// Moving a branch another worktree has checked out would change that
/// worktree's HEAD commit underneath it, so split refuses before changing
/// anything.
#[test]
fn split_refuses_to_move_a_branch_checked_out_in_another_worktree() {
    let (dir, repo) = setup_repo();
    let commit_1 = repo
        .revparse_single("HEAD~2")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    repo.branch("elsewhere", &commit_1, false).unwrap();
    let tip = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature-x", &tip, false).unwrap();
    repo.set_head("refs/heads/feature-x").unwrap();
    let _other = check_out_elsewhere(dir.path(), "elsewhere");
    let before = refs_snapshot(dir.path());
    let editor = git_dir_editor(
        &repo,
        "perl -i -pe 's/.*branch elsewhere.*\\n?//g' \"$file\"\n\
         perl -i -pe 's/(commit 2)/$1\\nbranch elsewhere/' \"$file\"\n",
    );

    let output = kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("Cannot update branch 'elsewhere': it is checked out in"),
        "{stderr}"
    );
    assert!(stderr.contains("No branch was changed."), "{stderr}");
    assert_eq!(refs_snapshot(dir.path()), before);
}

/// A detached HEAD attaches to a branch at its commit, but not to one another
/// worktree has checked out: Git never has two worktrees on one branch.
#[test]
fn split_refuses_to_attach_head_to_a_branch_checked_out_in_another_worktree() {
    let (dir, repo) = setup_repo();
    assert!(repo.head_detached().unwrap());
    let tip = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("held", &tip, false).unwrap();
    let _other = check_out_elsewhere(dir.path(), "held");
    let before = refs_snapshot(dir.path());
    let editor = git_dir_editor(
        &repo,
        "perl -i -pe 's/(commit 1)/$1\\nbranch part-1/' \"$file\"\n",
    );

    let output = kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("Cannot attach HEAD to branch 'held': it is checked out in"),
        "{stderr}"
    );
    assert!(stderr.contains("No branch was changed."), "{stderr}");
    assert_eq!(refs_snapshot(dir.path()), before);
    assert!(repo.head_detached().unwrap());
}

/// Every ref is locked before any is written, but writing can still fail part
/// way: deleting a packed branch rewrites `packed-refs`, whose lock another
/// Git process may hold. The refs written by then stay changed, the split is
/// recorded, and `kin undo` puts them back.
///
/// libgit2 writes a transaction's refs in the order of its hash map, so the
/// split creates many branches to have some written before the delete fails.
#[test]
fn split_that_fails_while_writing_refs_can_be_undone() {
    let (dir, repo) = setup_repo();
    let tip = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch("feature-x", &tip, false).unwrap();
    repo.set_head("refs/heads/feature-x").unwrap();
    let commit_1 = repo
        .revparse_single("HEAD~2")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    repo.branch("packed", &commit_1, false).unwrap();
    common::run_ok("git", &["pack-refs", "--all"], dir.path());
    let branches = |dir: &std::path::Path| {
        git_out(
            dir,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads",
            ],
        )
    };
    let before = branches(dir.path());
    let editor = git_dir_editor(
        &repo,
        "perl -i -pe 's/.*branch packed.*\\n?//g' \"$file\"\n\
         perl -i -pe 's/(commit 1)$/$1 . join(\"\", map { \"\\nbranch new-$_\" } 1..20)/e' \"$file\"\n",
    );
    let packed_lock = repo.path().join("packed-refs.lock");
    fs::write(&packed_lock, "").unwrap();

    let output = kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor)
        .output()
        .unwrap();
    fs::remove_file(&packed_lock).unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains(
            "Some branches may have changed; the split is recorded, so 'kin undo' restores them."
        ),
        "{stderr}"
    );
    let after = branches(dir.path());
    assert!(
        after.contains("refs/heads/new-") && after.contains("refs/heads/packed "),
        "some new branches should have been written before the delete failed:\n{after}"
    );

    kin_cmd()
        .arg("undo")
        .current_dir(dir.path())
        .assert()
        .success();
    assert_eq!(branches(dir.path()), before);
}

/// A deleted branch's configuration goes with it, however its values are
/// spread over the config file: a key can have several values, and a section
/// can appear more than once.
#[test]
fn split_forgets_every_config_value_of_a_deleted_branch() {
    let (dir, repo) = setup_repo();
    let commit_1 = repo
        .revparse_single("HEAD~2")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    repo.branch("part-1", &commit_1, false).unwrap();
    let config_path = repo.path().join("config");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str(
        "[branch \"part-1\"]\n\tremote = origin\n\tmerge = refs/heads/a\n\tmerge = refs/heads/b\n\
         [branch \"other\"]\n\tremote = origin\n\
         [branch \"part-1\"]\n\tremote = upstream\n\tdescription = kept apart\n",
    );
    fs::write(&config_path, config).unwrap();
    let editor = git_dir_editor(
        &repo,
        "perl -i -pe 's/.*branch part-1.*\\n?//g' \"$file\"\n",
    );

    let output = kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains("Warning"), "{stderr}");
    assert_eq!(
        git_out(dir.path(), &["config", "--list", "--local"])
            .lines()
            .filter(|line| line.starts_with("branch."))
            .collect::<Vec<_>>(),
        vec!["branch.other.remote=origin"]
    );
}

/// A bare `branch` row (no name) is auto-named by slugifying the commit it sits
/// on. Here the row under "commit 2" produces a branch named `commit-2`.
#[test]
fn test_split_auto_names_branch_from_commit() {
    let (dir, repo) = setup_repo();

    let editor_script = dir.path().join("editor.sh");
    fs::write(
        &editor_script,
        r#"#!/bin/sh
file=$1
perl -i -pe 's/(commit 2)/$1\nbranch/' "$file"
"#,
    )
    .unwrap();
    make_executable(&editor_script);

    let mut cmd = kin_cmd();
    cmd.arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor_script)
        .assert()
        .success();

    let branch = repo
        .find_branch("commit-2", git2::BranchType::Local)
        .expect("bare 'branch' row should auto-create a branch slugged from the commit");
    let commit = repo.find_commit(branch.get().target().unwrap()).unwrap();
    assert_eq!(commit.summary().unwrap(), "commit 2");
}

/// Build a repo with `main` at "initial" and a linear `feature` branch carrying
/// one commit per message (HEAD left on `feature`). Uses `make_commit` so commit
/// messages can be arbitrary, including duplicates or empty.
fn linear_feature_repo(messages: &[&str]) -> (tempfile::TempDir, Repository) {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());
    {
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test User").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();
    }

    let base = make_commit(&repo, "refs/heads/main", "base.txt", "base", "initial", &[]);
    let mut parent_id = base;
    for (i, message) in messages.iter().enumerate() {
        let parent = repo.find_commit(parent_id).unwrap();
        parent_id = make_commit(
            &repo,
            "refs/heads/feature",
            &format!("f{i}.txt"),
            &format!("c{i}"),
            message,
            &[&parent],
        );
    }

    repo.set_head("refs/heads/feature").unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
    (dir, repo)
}

fn write_editor(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let editor = dir.join("editor.sh");
    fs::write(&editor, format!("#!/bin/sh\n{body} \"$1\"\n")).unwrap();
    make_executable(&editor);
    editor
}

/// Two bare `branch` rows whose commits slug to the same base name are
/// disambiguated with numeric suffixes (`dup`, `dup-2`).
#[test]
fn test_split_auto_name_disambiguates_colliding_slugs() {
    let (dir, repo) = linear_feature_repo(&["dup", "dup"]);
    let editor = write_editor(dir.path(), "perl -i -pe 's/ dup$/ dup\\nbranch/'");

    kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor)
        .assert()
        .success();

    assert!(
        repo.find_branch("dup", git2::BranchType::Local).is_ok(),
        "first colliding slug should take the base name"
    );
    assert!(
        repo.find_branch("dup-2", git2::BranchType::Local).is_ok(),
        "second colliding slug should be disambiguated to dup-2"
    );
}

/// A bare `branch` row before any commit line is rejected.
#[test]
fn test_split_bare_branch_row_without_commit_errors() {
    let (dir, _repo) = linear_feature_repo(&["work one", "work two"]);
    // Prepend a bare `branch` row before the first commit line.
    let editor = write_editor(dir.path(), "perl -i -pe 'print \"branch\\n\" if $. == 1'");

    kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor)
        .assert()
        .failure()
        .stderr(predicates::str::contains("must follow a commit line"));
}

/// A bare `branch` row on a commit whose summary yields no slug is rejected.
#[test]
fn test_split_bare_branch_row_on_empty_summary_errors() {
    let (dir, _repo) = linear_feature_repo(&[""]);
    // The empty-summary commit renders as "<sha> " (trailing space); add a bare
    // `branch` row after it.
    let editor = write_editor(dir.path(), "perl -i -pe 's/^([0-9a-f]{7} )$/$1\\nbranch/'");

    kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor)
        .assert()
        .failure()
        .stderr(predicates::str::contains("Cannot derive a branch name"));
}

/// A bare `branch` row whose preceding commit id no longer resolves reports the
/// commit-resolution error, not a misleading "empty summary".
#[test]
fn test_split_bare_branch_row_on_unresolvable_commit_errors() {
    let (dir, _repo) = setup_repo();
    // Tamper the first commit's SHA to a non-matching prefix and drop a bare
    // `branch` row under it.
    let editor = write_editor(
        dir.path(),
        "perl -i -pe 's/^[0-9a-f]{7}( commit 1)$/0000000$1\\nbranch/'",
    );

    kin_cmd()
        .arg("split")
        .current_dir(dir.path())
        .env("GIT_EDITOR", &editor)
        .assert()
        .failure()
        .stderr(predicates::str::contains("was modified or moved"));
}
