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

    // An absorbable staged change plus leftovers the engine cannot place: an
    // unstaged edit and an untracked file.
    std::fs::write(repo_path.join("code.txt"), "line1 FIXED\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], repo_path);
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
