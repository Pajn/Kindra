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

    // An absorbable staged change, a staged edit the engine cannot place (a.txt
    // belongs to a commit below the absorb range), and an untracked file.
    std::fs::write(repo_path.join("code.txt"), "line1 FIXED\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], repo_path);
    std::fs::write(repo_path.join("a.txt"), "A staged leftover").unwrap();
    run_ok("git", &["add", "a.txt"], repo_path);
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

    // Every branch is back where it was, the fixup commits are gone, both
    // staged changes are back *staged*, the untracked leftover is back, and no
    // resumable state was left behind.
    let tips_after = (tip(&repo, "review"), tip(&repo, "perf"), tip(&repo, "docs"));
    assert_eq!(tips_before, tips_after, "rollback must restore every tip");
    let staged = std::process::Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(repo_path)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&staged.stdout).trim(),
        "a.txt\ncode.txt",
        "both the absorbed hunk and the set-aside staged hunk must come back staged"
    );
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
fn test_absorb_abort_restores_tips_and_absorbed_changes() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = setup_stack(repo_path);

    // Give perf a commit that conflicts with the fixup so the dependent
    // restack stops after the fold has already completed.
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

    // An absorbable staged change, a staged edit the engine cannot place, and
    // an untracked file.
    std::fs::write(repo_path.join("code.txt"), "line1 REVIEW\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], repo_path);
    std::fs::write(repo_path.join("a.txt"), "A staged leftover").unwrap();
    run_ok("git", &["add", "a.txt"], repo_path);
    std::fs::write(repo_path.join("untracked.txt"), "untracked").unwrap();

    let tips_before = (tip(&repo, "review"), tip(&repo, "perf"), tip(&repo, "docs"));

    let mut cmd = kin_cmd();
    cmd.current_dir(repo_path).arg("absorb").assert().failure();

    let mut cmd = kin_cmd();
    cmd.current_dir(repo_path).arg("abort").assert().success();

    // Every branch is back at its pre-absorb tip.
    let tips_after = (tip(&repo, "review"), tip(&repo, "perf"), tip(&repo, "docs"));
    assert_eq!(tips_before, tips_after, "abort must restore every tip");
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "review");

    // The absorbed change was already folded into now-discarded history; abort
    // must bring it back as a staged change, alongside the set-aside staged
    // leftover, instead of losing it.
    let staged = std::process::Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(repo_path)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&staged.stdout).trim(),
        "a.txt\ncode.txt",
        "abort must restage the absorbed change and the set-aside staged hunk"
    );
    assert_eq!(
        std::fs::read_to_string(repo_path.join("code.txt")).unwrap(),
        "line1 REVIEW\nline2\nline3\n",
        "the absorbed content must be back in the working tree"
    );
    assert_eq!(
        std::fs::read_to_string(repo_path.join("untracked.txt")).unwrap(),
        "untracked"
    );

    // Bookkeeping: no resumable state, no leftover stash entry.
    assert!(!repo_path.join(".git/kindra_rebase_state.json").exists());
    let stashes = std::process::Command::new("git")
        .args(["stash", "list"])
        .current_dir(repo_path)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&stashes.stdout), "");
}

#[test]
fn test_absorb_restacks_branch_forking_from_inside_the_range() {
    check_absorb_fork(None);
}

#[test]
fn test_absorb_fork_conflict_continue() {
    check_absorb_fork(Some("continue"));
}

#[test]
fn test_absorb_fork_conflict_abort() {
    check_absorb_fork(Some("abort"));
}

#[test]
fn test_absorb_fork_dry_run() {
    check_absorb_fork(Some("dry-run"));
}

#[test]
fn test_absorb_multiple_fork_points_with_abbreviated_todo() {
    check_absorb_fork(Some("multiple"));
}

#[test]
fn test_absorb_fork_undo() {
    check_absorb_fork(Some("undo"));
}

fn check_absorb_fork(mode: Option<&str>) {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = setup_stack(repo_path);

    // A branch forked from review's first commit (which no branch points at)
    // with its own work on top. The fold would rewrite the fork point, and
    // absorb must carry the branch onto the rewritten fork point.
    let review_tip = tip(&repo, "review");
    let code_commit = first_parent(&repo, review_tip);
    run_ok(
        "git",
        &["checkout", "-b", "loose", &code_commit.to_string()],
        repo_path,
    );
    std::fs::write(repo_path.join("loose.txt"), "L").unwrap();
    run_ok("git", &["add", "loose.txt"], repo_path);
    run_ok("git", &["commit", "-m", "loose: work"], repo_path);
    if matches!(mode, Some("continue" | "abort")) {
        std::fs::write(repo_path.join("code.txt"), "line1 SIDE\nline2\nline3\n").unwrap();
        run_ok("git", &["add", "code.txt"], repo_path);
        run_ok("git", &["commit", "-m", "side edit"], repo_path);
    }
    run_ok("git", &["checkout", "-b", "loose-child"], repo_path);
    std::fs::write(repo_path.join("child.txt"), "child").unwrap();
    run_ok("git", &["add", "child.txt"], repo_path);
    run_ok("git", &["commit", "-m", "child"], repo_path);
    run_ok("git", &["checkout", "review"], repo_path);

    if mode == Some("multiple") {
        // perf now forks at review's second commit, while loose forks at its
        // first: neither fork point has a branch ref on the rewritten path.
        std::fs::write(repo_path.join("tail.txt"), "tail").unwrap();
        run_ok("git", &["add", "tail.txt"], repo_path);
        run_ok("git", &["commit", "-m", "review tail"], repo_path);
        std::fs::write(repo_path.join("extra.txt"), "extra fixed").unwrap();
        run_ok("git", &["add", "extra.txt"], repo_path);
        run_ok(
            "git",
            &["config", "rebase.abbreviateCommands", "true"],
            repo_path,
        );
    }
    std::fs::write(repo_path.join("code.txt"), "line1 FIXED\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], repo_path);

    let names = ["review", "perf", "docs", "loose", "loose-child"];
    let before: Vec<_> = names.iter().map(|name| tip(&repo, name)).collect();
    let mut command = kin_cmd();
    command.current_dir(repo_path).arg("absorb");
    if mode == Some("multiple") {
        command.arg("--force-author");
    }
    if mode == Some("dry-run") {
        command.arg("--dry-run");
    }
    let output = command.output().unwrap();
    if matches!(mode, Some("continue" | "abort")) {
        assert!(!output.status.success());
        assert!(repo_path.join(".git/rebase-merge").exists());
        if mode == Some("continue") {
            std::fs::write(repo_path.join("code.txt"), "line1 FIXED\nline2\nline3\n").unwrap();
            run_ok("git", &["add", "code.txt"], repo_path);
        }
        let output = kin_cmd()
            .current_dir(repo_path)
            .env("GIT_EDITOR", "true")
            .arg(mode.unwrap())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    } else {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if matches!(mode, Some("abort" | "dry-run")) {
        assert_eq!(
            before,
            names
                .iter()
                .map(|name| tip(&repo, name))
                .collect::<Vec<_>>()
        );
        let staged = std::process::Command::new("git")
            .current_dir(repo_path)
            .args(["diff", "--cached", "--name-only"])
            .output()
            .unwrap();
        assert!(staged.status.success());
        assert_eq!(String::from_utf8_lossy(&staged.stdout).trim(), "code.txt");
    } else {
        let rewritten_fork = if mode == Some("multiple") {
            let second = first_parent(&repo, tip(&repo, "review"));
            assert_eq!(first_parent(&repo, tip(&repo, "perf")), second);
            assert_eq!(
                file_in_commit(&repo, tip(&repo, "perf"), "extra.txt"),
                "extra fixed"
            );
            first_parent(&repo, second)
        } else {
            first_parent(&repo, tip(&repo, "review"))
        };
        assert_ne!(rewritten_fork, code_commit);
        assert!(
            repo.graph_descendant_of(tip(&repo, "loose"), rewritten_fork)
                .unwrap()
        );
        assert!(
            !repo
                .graph_descendant_of(tip(&repo, "loose"), tip(&repo, "review"))
                .unwrap()
        );
        assert_eq!(
            first_parent(&repo, tip(&repo, "loose-child")),
            tip(&repo, "loose")
        );
        for name in names {
            assert_eq!(
                file_in_commit(&repo, tip(&repo, name), "code.txt"),
                "line1 FIXED\nline2\nline3\n"
            );
        }
    }
    if mode == Some("undo") {
        let output = kin_cmd()
            .current_dir(repo_path)
            .arg("undo")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            before,
            names
                .iter()
                .map(|name| tip(&repo, name))
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(common::current_branch(repo_path), "review");
    assert_no_rebase_in_progress(repo_path);
    assert!(!repo_path.join(".git/kindra_rebase_state.json").exists());
    assert_eq!(
        repo.references_glob("refs/kindra/absorb/*")
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn test_absorb_moves_shared_head_sibling_and_undo_restores_it() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = setup_stack(repo_path);

    // A sibling branch sharing review's tip commit: not a descendant (same
    // tip), so only the fold's --update-refs moves it.
    let review_tip_before = tip(&repo, "review");
    run_ok(
        "git",
        &["branch", "sibling", &review_tip_before.to_string()],
        repo_path,
    );

    std::fs::write(repo_path.join("code.txt"), "line1 FIXED\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], repo_path);

    let mut cmd = kin_cmd();
    cmd.current_dir(repo_path).arg("absorb").assert().success();

    // The sibling must follow the fold onto the rewritten tip.
    let review_tip = tip(&repo, "review");
    assert_ne!(review_tip, review_tip_before);
    assert_eq!(
        tip(&repo, "sibling"),
        review_tip,
        "shared-head sibling must be moved with the fold"
    );

    // And undo must restore it along with the rest of the stack.
    let mut cmd = kin_cmd();
    cmd.current_dir(repo_path).arg("undo").assert().success();
    assert_eq!(tip(&repo, "review"), review_tip_before);
    assert_eq!(
        tip(&repo, "sibling"),
        review_tip_before,
        "undo must restore the sibling's pre-fold tip"
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
    // A uniquely-scoped sibling directory: a fixed shared path would leak
    // across failed runs and make the next `git worktree add` refuse.
    let wt_temp = TempDir::new().unwrap();
    let wt_path = wt_temp.path().join("shared-head-wt");
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

    // Resolve the conflict on perf's commit and continue. A bogus editor
    // proves the resume honors the state's suppress_editor flag: git rebase
    // --continue after a conflicted pick opens the commit-message editor, so
    // without the pin this fails.
    std::fs::write(repo_path.join("code.txt"), "line1 RESOLVED\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], repo_path);
    let mut cmd = kin_cmd();
    cmd.current_dir(repo_path)
        .env("GIT_EDITOR", "false")
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

#[test]
fn test_absorb_fork_in_another_worktree_is_rejected_before_changes() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = setup_stack(repo_path);
    let fork = first_parent(&repo, tip(&repo, "review"));
    run_ok(
        "git",
        &["checkout", "-b", "side", &fork.to_string()],
        repo_path,
    );
    std::fs::write(repo_path.join("side.txt"), "side").unwrap();
    run_ok("git", &["add", "side.txt"], repo_path);
    run_ok("git", &["commit", "-m", "side"], repo_path);
    run_ok("git", &["checkout", "review"], repo_path);
    let other = TempDir::new().unwrap();
    run_ok(
        "git",
        &["worktree", "add", other.path().to_str().unwrap(), "side"],
        repo_path,
    );
    std::fs::write(repo_path.join("code.txt"), "line1 FIXED\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], repo_path);
    let before = tip(&repo, "review");
    let output = kin_cmd()
        .current_dir(repo_path)
        .arg("absorb")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("checked out"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(tip(&repo, "review"), before);
    assert!(!repo_path.join(".git/kindra_rebase_state.json").exists());
    assert_eq!(
        repo.references_glob("refs/kindra/absorb/*")
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn test_absorb_fork_from_linked_worktree() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = setup_stack(repo_path);
    let fork = first_parent(&repo, tip(&repo, "review"));
    run_ok(
        "git",
        &["checkout", "-b", "side", &fork.to_string()],
        repo_path,
    );
    std::fs::write(repo_path.join("side.txt"), "side").unwrap();
    run_ok("git", &["add", "side.txt"], repo_path);
    run_ok("git", &["commit", "-m", "side"], repo_path);
    run_ok("git", &["checkout", "main"], repo_path);
    let other = TempDir::new().unwrap();
    run_ok(
        "git",
        &["worktree", "add", other.path().to_str().unwrap(), "review"],
        repo_path,
    );
    // An anchor owned by another operation must be left intact.
    let anchor = format!("refs/kindra/absorb/other-operation/{fork}");
    run_ok(
        "git",
        &["update-ref", &anchor, &fork.to_string()],
        repo_path,
    );
    std::fs::write(other.path().join("code.txt"), "line1 FIXED\nline2\nline3\n").unwrap();
    run_ok("git", &["add", "code.txt"], other.path());
    let output = kin_cmd()
        .current_dir(other.path())
        .arg("absorb")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        first_parent(&repo, tip(&repo, "side")),
        first_parent(&repo, tip(&repo, "review"))
    );
    let linked = Repository::open(other.path()).unwrap();
    assert_eq!(
        linked
            .references_glob("refs/kindra/absorb/*")
            .unwrap()
            .count(),
        1
    );
    assert_eq!(repo.find_reference(&anchor).unwrap().target(), Some(fork));
    assert!(!linked.path().join("kindra_rebase_state.json").exists());
}
