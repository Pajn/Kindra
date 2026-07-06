use git2::Repository;
use tempfile::TempDir;

mod common;
use common::{assert_no_rebase_in_progress, kin_cmd, make_commit, repo_init, run_ok};

/// Build the standard fixture: main, then a stack review -> perf -> docs where
/// review's first commit introduces `code.txt`. Returns the repo handle.
fn setup_stack(repo_path: &std::path::Path) -> Repository {
    let repo = repo_init(repo_path);

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    let main_oid = make_commit(&repo, "HEAD", "a.txt", "A", "main: base", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);

    run_ok("git", &["checkout", "-b", "review"], repo_path);
    let code_oid = make_commit(
        &repo,
        "HEAD",
        "code.txt",
        "line1\nline2\nline3\n",
        "review: add code",
        &[&repo.find_commit(main_oid).unwrap()],
    );
    let extra_oid = make_commit(
        &repo,
        "HEAD",
        "extra.txt",
        "extra",
        "review: add extra",
        &[&repo.find_commit(code_oid).unwrap()],
    );

    run_ok("git", &["checkout", "-b", "perf"], repo_path);
    let perf_oid = make_commit(
        &repo,
        "HEAD",
        "perf.txt",
        "perf",
        "perf: work",
        &[&repo.find_commit(extra_oid).unwrap()],
    );

    run_ok("git", &["checkout", "-b", "docs"], repo_path);
    make_commit(
        &repo,
        "HEAD",
        "docs.txt",
        "docs",
        "docs: work",
        &[&repo.find_commit(perf_oid).unwrap()],
    );

    run_ok("git", &["checkout", "review"], repo_path);
    repo
}

fn tip(repo: &Repository, name: &str) -> git2::Oid {
    repo.find_branch(name, git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap()
}

fn first_parent(repo: &Repository, oid: git2::Oid) -> git2::Oid {
    repo.find_commit(oid).unwrap().parent_id(0).unwrap()
}

fn commit_summary(repo: &Repository, oid: git2::Oid) -> String {
    repo.find_commit(oid)
        .unwrap()
        .summary()
        .unwrap()
        .to_string()
}

fn file_in_commit(repo: &Repository, oid: git2::Oid, path: &str) -> String {
    let tree = repo.find_commit(oid).unwrap().tree().unwrap();
    let entry = tree.get_path(std::path::Path::new(path)).unwrap();
    let blob = repo.find_blob(entry.id()).unwrap();
    String::from_utf8_lossy(blob.content()).into_owned()
}

#[test]
fn test_absorb_folds_staged_change_and_restacks_dependents() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = setup_stack(repo_path);

    // Stage a change that belongs in "review: add code".
    std::fs::write(repo_path.join("code.txt"), "line1 FIXED\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], repo_path);

    let mut cmd = kin_cmd();
    let output = cmd.current_dir(repo_path).arg("absorb").output().unwrap();
    assert!(
        output.status.success(),
        "absorb failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_no_rebase_in_progress(repo_path);

    // The fold must leave review at exactly two commits, with the staged change
    // inside "review: add code" and no fixup! commit left behind.
    let review_tip = tip(&repo, "review");
    assert_eq!(commit_summary(&repo, review_tip), "review: add extra");
    let code_commit = first_parent(&repo, review_tip);
    assert_eq!(commit_summary(&repo, code_commit), "review: add code");
    assert_eq!(
        file_in_commit(&repo, code_commit, "code.txt"),
        "line1 FIXED\nline2\nline3\n"
    );
    assert_eq!(
        commit_summary(&repo, first_parent(&repo, code_commit)),
        "main: base"
    );

    // Dependents must follow the rewritten review linearly.
    let perf_tip = tip(&repo, "perf");
    let docs_tip = tip(&repo, "docs");
    assert_eq!(
        first_parent(&repo, perf_tip),
        review_tip,
        "perf must sit on the rewritten review"
    );
    assert_eq!(
        first_parent(&repo, docs_tip),
        perf_tip,
        "docs must sit on the restacked perf"
    );

    // We must end up back on review with a clean tree.
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "review");
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_path)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&status.stdout), "");
}

#[test]
fn test_absorb_nothing_staged_is_a_noop() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = setup_stack(repo_path);

    let tips_before = (tip(&repo, "review"), tip(&repo, "perf"), tip(&repo, "docs"));

    let mut cmd = kin_cmd();
    cmd.current_dir(repo_path).arg("absorb").assert().success();

    let tips_after = (tip(&repo, "review"), tip(&repo, "perf"), tip(&repo, "docs"));
    assert_eq!(tips_before, tips_after, "no branch may move");
    assert_no_rebase_in_progress(repo_path);
}

#[test]
fn test_absorb_dry_run_makes_no_changes() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = setup_stack(repo_path);

    std::fs::write(repo_path.join("code.txt"), "line1 FIXED\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], repo_path);

    let tips_before = (tip(&repo, "review"), tip(&repo, "perf"), tip(&repo, "docs"));

    let mut cmd = kin_cmd();
    cmd.current_dir(repo_path)
        .args(["absorb", "--dry-run"])
        .assert()
        .success();

    let tips_after = (tip(&repo, "review"), tip(&repo, "perf"), tip(&repo, "docs"));
    assert_eq!(tips_before, tips_after, "dry-run must not move any branch");

    // The staged change must still be staged.
    let staged = std::process::Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(repo_path)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&staged.stdout).trim(), "code.txt");
}

#[test]
fn test_absorb_restores_leftover_changes_after_completion() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = setup_stack(repo_path);

    // An absorbable staged change plus leftovers the engine cannot place: a
    // staged edit to a file whose commit is below the absorb range, an
    // unstaged edit, and an untracked file.
    std::fs::write(repo_path.join("code.txt"), "line1 FIXED\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], repo_path);
    std::fs::write(repo_path.join("a.txt"), "A staged leftover").unwrap();
    run_ok("git", &["add", "a.txt"], repo_path);
    std::fs::write(repo_path.join("extra.txt"), "extra\nunstaged edit\n").unwrap();
    std::fs::write(repo_path.join("untracked.txt"), "untracked").unwrap();

    let mut cmd = kin_cmd();
    cmd.current_dir(repo_path).arg("absorb").assert().success();
    assert_no_rebase_in_progress(repo_path);

    // The absorbable change was folded, the leftovers restored.
    assert_eq!(
        std::fs::read_to_string(repo_path.join("extra.txt")).unwrap(),
        "extra\nunstaged edit\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo_path.join("untracked.txt")).unwrap(),
        "untracked"
    );

    // The staged-but-unabsorbable edit must come back *staged*, so a follow-up
    // `git commit` still includes it.
    let staged = std::process::Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(repo_path)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&staged.stdout).trim(),
        "a.txt",
        "staged leftover must be restored staged"
    );

    // Nothing may be left in the stash list.
    let stashes = std::process::Command::new("git")
        .args(["stash", "list"])
        .current_dir(repo_path)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&stashes.stdout), "");

    let docs_tip = tip(&repo, "docs");
    let perf_tip = tip(&repo, "perf");
    assert_eq!(first_parent(&repo, docs_tip), perf_tip);
    assert_eq!(first_parent(&repo, perf_tip), tip(&repo, "review"));
}

#[test]
fn test_absorb_rejects_base_that_is_not_an_ancestor() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = setup_stack(repo_path);

    // A sibling commit off main that is not in review's history.
    run_ok("git", &["checkout", "-b", "sibling", "main"], repo_path);
    let main_tip = tip(&repo, "main");
    let sibling_oid = make_commit(
        &repo,
        "HEAD",
        "sibling.txt",
        "S",
        "sibling: work",
        &[&repo.find_commit(main_tip).unwrap()],
    );
    run_ok("git", &["checkout", "review"], repo_path);

    std::fs::write(repo_path.join("code.txt"), "line1 FIXED\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], repo_path);

    let tips_before = (tip(&repo, "review"), tip(&repo, "perf"), tip(&repo, "docs"));
    let mut cmd = kin_cmd();
    let output = cmd
        .current_dir(repo_path)
        .args(["absorb", "--base", &sibling_oid.to_string()])
        .output()
        .unwrap();
    assert!(!output.status.success(), "non-ancestor --base must fail");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not an ancestor"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let tips_after = (tip(&repo, "review"), tip(&repo, "perf"), tip(&repo, "docs"));
    assert_eq!(tips_before, tips_after, "no branch may move");
}

#[test]
fn test_absorb_rejects_base_below_the_stack_parent() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = setup_stack(repo_path);

    // From perf, review's commits are below the stack parent; absorbing past
    // them would rewrite review without restacking review's other dependents.
    run_ok("git", &["checkout", "perf"], repo_path);
    std::fs::write(repo_path.join("perf.txt"), "perf FIXED").unwrap();
    run_ok("git", &["add", "perf.txt"], repo_path);

    let main_tip = tip(&repo, "main").to_string();
    let mut cmd = kin_cmd();
    let output = cmd
        .current_dir(repo_path)
        .args(["absorb", "--base", &main_tip])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "--base below the stack parent must fail"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("below the current branch"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_absorb_squash_folds_without_an_editor() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = setup_stack(repo_path);

    std::fs::write(repo_path.join("code.txt"), "line1 FIXED\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], repo_path);

    // A bogus editor proves the fold never opens one for the squash messages.
    let mut cmd = kin_cmd();
    let output = cmd
        .current_dir(repo_path)
        .env("GIT_EDITOR", "false")
        .env("EDITOR", "false")
        .args(["absorb", "--squash", "--message", "squash body"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "squash absorb failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("squash commit"),
        "completion message must say squash, not fixup\nstdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_no_rebase_in_progress(repo_path);

    // Folded into the target with no squash! commit left behind, stack linear.
    let review_tip = tip(&repo, "review");
    let code_commit = first_parent(&repo, review_tip);
    assert_eq!(commit_summary(&repo, code_commit), "review: add code");
    assert_eq!(
        file_in_commit(&repo, code_commit, "code.txt"),
        "line1 FIXED\nline2\nline3\n"
    );
    assert_eq!(first_parent(&repo, tip(&repo, "perf")), review_tip);
}

#[test]
fn test_absorb_rolls_back_when_the_fold_fails_before_starting() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = setup_stack(repo_path);

    // A rejecting pre-rebase hook makes the fold fail without leaving a rebase
    // in progress.
    let hook_dir = repo_path.join(".git/hooks");
    std::fs::create_dir_all(&hook_dir).unwrap();
    let hook = hook_dir.join("pre-rebase");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    std::fs::write(repo_path.join("code.txt"), "line1 FIXED\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], repo_path);
    std::fs::write(repo_path.join("untracked.txt"), "untracked").unwrap();

    let tips_before = (tip(&repo, "review"), tip(&repo, "perf"), tip(&repo, "docs"));
    let mut cmd = kin_cmd();
    let output = cmd.current_dir(repo_path).arg("absorb").output().unwrap();
    assert!(!output.status.success(), "rejected fold must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("rolled back"),
        "error must say the absorb was rolled back, not invite kin continue\nstderr:\n{}",
        stderr
    );
    assert!(
        !stderr.contains("kin continue"),
        "a fold that never started must not invite kin continue\nstderr:\n{}",
        stderr
    );

    // Every branch is back where it was, the fixup commits are gone, the
    // absorbed content is back in the index, the untracked leftover is back,
    // and no resumable state was left behind.
    let tips_after = (tip(&repo, "review"), tip(&repo, "perf"), tip(&repo, "docs"));
    assert_eq!(tips_before, tips_after, "rollback must restore every tip");
    let staged = std::process::Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(repo_path)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&staged.stdout).trim(), "code.txt");
    assert_eq!(
        std::fs::read_to_string(repo_path.join("untracked.txt")).unwrap(),
        "untracked"
    );
    assert!(
        !repo_path.join(".git/kindra_rebase_state.json").exists(),
        "no resumable state may remain after a rollback"
    );
}

#[test]
fn test_absorb_refuses_when_in_range_branch_is_checked_out_elsewhere() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = setup_stack(repo_path);

    // A sibling branch sharing review's tip commit, checked out in another
    // worktree. It is not a descendant (same tip, so it is not in the
    // restacked sub-stack), but the fold's --update-refs would move it, so
    // absorb must refuse up front instead of silently skipping it.
    let review_tip = tip(&repo, "review");
    run_ok(
        "git",
        &["branch", "shared-head", &review_tip.to_string()],
        repo_path,
    );
    let wt_path = temp.path().join("../absorb-wt-shared");
    run_ok(
        "git",
        &["worktree", "add", wt_path.to_str().unwrap(), "shared-head"],
        repo_path,
    );

    std::fs::write(repo_path.join("code.txt"), "line1 FIXED\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], repo_path);

    let mut cmd = kin_cmd();
    let output = cmd.current_dir(repo_path).arg("absorb").output().unwrap();
    let _ = std::process::Command::new("git")
        .args(["worktree", "remove", "--force", wt_path.to_str().unwrap()])
        .current_dir(repo_path)
        .output();
    assert!(
        !output.status.success(),
        "absorb must refuse while an in-range branch is checked out in another worktree\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        tip(&repo, "review"),
        review_tip,
        "no branch may move when the worktree check refuses"
    );
}

#[test]
fn test_absorb_conflicting_dependent_completes_via_continue() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = setup_stack(repo_path);

    // Give perf a commit that edits the same line the fixup will change, so
    // restacking perf conflicts.
    run_ok("git", &["checkout", "perf"], repo_path);
    let perf_old_tip = tip(&repo, "perf");
    make_commit(
        &repo,
        "HEAD",
        "code.txt",
        "line1 PERF\nline2\nline3\n",
        "perf: edit line1",
        &[&repo.find_commit(perf_old_tip).unwrap()],
    );
    run_ok("git", &["checkout", "review"], repo_path);

    std::fs::write(repo_path.join("code.txt"), "line1 REVIEW\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], repo_path);
    std::fs::write(repo_path.join("untracked.txt"), "untracked").unwrap();

    let mut cmd = kin_cmd();
    cmd.current_dir(repo_path).arg("absorb").assert().failure();

    // Resolve the conflict on perf's commit and continue.
    std::fs::write(repo_path.join("code.txt"), "line1 RESOLVED\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], repo_path);
    let mut cmd = kin_cmd();
    cmd.current_dir(repo_path)
        .env("GIT_EDITOR", "cat")
        .arg("continue")
        .assert()
        .success();
    // Only check for an in-progress rebase; `git rebase --continue` leaves a
    // stale REBASE_HEAD marker behind when the conflicted branch was the last
    // one rebased, exactly as it does in plain-git conflict flows.
    assert!(!repo_path.join(".git/rebase-merge").exists());
    assert!(!repo_path.join(".git/rebase-apply").exists());

    // The whole stack must be linear again and the untracked leftover restored.
    let review_tip = tip(&repo, "review");
    let perf_tip = tip(&repo, "perf");
    assert_eq!(commit_summary(&repo, perf_tip), "perf: edit line1");
    assert_eq!(
        first_parent(&repo, first_parent(&repo, perf_tip)),
        review_tip,
        "perf must sit on the rewritten review"
    );
    assert_eq!(
        std::fs::read_to_string(repo_path.join("untracked.txt")).unwrap(),
        "untracked"
    );
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "review");
}

#[test]
fn test_absorb_undo_restores_pre_absorb_tips() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = setup_stack(repo_path);

    let tips_before = (tip(&repo, "review"), tip(&repo, "perf"), tip(&repo, "docs"));

    std::fs::write(repo_path.join("code.txt"), "line1 FIXED\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], repo_path);

    let mut cmd = kin_cmd();
    cmd.current_dir(repo_path).arg("absorb").assert().success();
    assert_ne!(
        tip(&repo, "review"),
        tips_before.0,
        "absorb must move review"
    );

    let mut cmd = kin_cmd();
    cmd.current_dir(repo_path).arg("undo").assert().success();

    let tips_after = (tip(&repo, "review"), tip(&repo, "perf"), tip(&repo, "docs"));
    assert_eq!(
        tips_before, tips_after,
        "undo must restore every pre-absorb branch tip"
    );
}
