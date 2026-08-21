use assert_cmd::Command;
use git2::{Repository, Signature};
use std::fs;
use std::path::{Path, PathBuf};

#[allow(dead_code)]
pub fn kin_cmd() -> Command {
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kin");
    cmd.env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        // Never let a subprocess (git commit, git rebase --continue, kin's own
        // file editor) fall through to an interactive editor: on CI there is no
        // $EDITOR and no core.editor, so it would resolve to `vi` and hang
        // forever waiting on a TTY. Tests that script edits set their own
        // GIT_EDITOR/GIT_SEQUENCE_EDITOR after this, which overrides these.
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true");
    cmd
}

#[allow(dead_code)]
pub fn run_ok(program: &str, args: &[&str], cwd: &std::path::Path) {
    let output = std::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Run Ok User")
        .env("GIT_AUTHOR_EMAIL", "run-ok@example.com")
        .env("GIT_COMMITTER_NAME", "Run Ok User")
        .env("GIT_COMMITTER_EMAIL", "run-ok@example.com")
        .output()
        .expect("failed to execute command");
    assert!(
        output.status.success(),
        "Command failed: {} {:?}\nstdout:\n{}\nstderr:\n{}",
        program,
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[allow(dead_code)]
pub fn git_command(cwd: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Run Ok User")
        .env("GIT_AUTHOR_EMAIL", "run-ok@example.com")
        .env("GIT_COMMITTER_NAME", "Run Ok User")
        .env("GIT_COMMITTER_EMAIL", "run-ok@example.com");
    command
}

#[allow(dead_code)]
pub fn make_commit_at(
    repo: &Repository,
    refname: &str,
    filename: &str,
    content: &str,
    message: &str,
    parents: &[&git2::Commit<'_>],
    time: i64,
) -> git2::Oid {
    let sig = Signature::new("Test User", "test@example.com", &git2::Time::new(time, 0)).unwrap();
    let mut index = repo.index().unwrap();
    fs::write(repo.workdir().unwrap().join(filename), content).unwrap();
    index.add_path(std::path::Path::new(filename)).unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    repo.commit(Some(refname), &sig, &sig, message, &tree, parents)
        .unwrap()
}

#[allow(dead_code)]
pub fn make_commit(
    repo: &Repository,
    refname: &str,
    filename: &str,
    content: &str,
    message: &str,
    parents: &[&git2::Commit<'_>],
) -> git2::Oid {
    let sig = Signature::now("Test User", "test@example.com").unwrap();
    let mut index = repo.index().unwrap();
    fs::write(repo.workdir().unwrap().join(filename), content).unwrap();
    index.add_path(std::path::Path::new(filename)).unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    repo.commit(Some(refname), &sig, &sig, message, &tree, parents)
        .unwrap()
}

#[allow(dead_code)]
pub fn repo_init(path: &Path) -> Repository {
    std::fs::create_dir_all(path).unwrap();
    run_ok("git", &["init", "--initial-branch=main"], path);
    // Pin hooks to this repo's own hooks directory — git's default — so a
    // developer's global `core.hooksPath` cannot influence test outcomes. Kindra's
    // own guards are what these tests assert on, and a personal pre-push hook
    // protecting `main`/`master` would otherwise mask a missing guard locally while
    // CI (which has no such hook) fails. Tests that install `.git/hooks/*` still
    // work, since this is where git would look anyway.
    run_ok(
        "git",
        &[
            "config",
            "core.hooksPath",
            path.join(".git").join("hooks").to_str().unwrap(),
        ],
        path,
    );
    Repository::open(path).unwrap()
}

#[allow(dead_code)]
/// Creates a repo with `main`, `feature-a`, and `feature-b`, leaving `HEAD` on `feature-b`.
pub fn setup_repo() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    let repo = repo_init(dir.path());
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Test User").unwrap();
    config.set_str("user.email", "test@example.com").unwrap();

    fs::write(dir.path().join("file.txt"), "main").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok("git", &["commit", "-m", "initial"], dir.path());

    run_ok("git", &["checkout", "-b", "feature-a"], dir.path());
    fs::write(dir.path().join("feature.txt"), "feature-a").unwrap();
    run_ok("git", &["add", "feature.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature-a"], dir.path());

    run_ok("git", &["checkout", "-b", "feature-b"], dir.path());
    fs::write(dir.path().join("feature-b.txt"), "feature-b").unwrap();
    run_ok("git", &["add", "feature-b.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature-b"], dir.path());

    dir
}

#[allow(dead_code)]
/// Creates a repo with `main` and `feature-a`, leaving `HEAD` on `main`.
pub fn setup_worktree_repo() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    let repo = repo_init(dir.path());
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "Test User").unwrap();
    config.set_str("user.email", "test@example.com").unwrap();

    fs::write(dir.path().join("file.txt"), "main").unwrap();
    run_ok("git", &["add", "file.txt"], dir.path());
    run_ok("git", &["commit", "-m", "initial"], dir.path());

    run_ok("git", &["checkout", "-b", "feature-a"], dir.path());
    fs::write(dir.path().join("feature.txt"), "feature").unwrap();
    run_ok("git", &["add", "feature.txt"], dir.path());
    run_ok("git", &["commit", "-m", "feature"], dir.path());
    run_ok("git", &["checkout", "main"], dir.path());

    dir
}

#[allow(dead_code)]
pub fn write_repo_config(repo_root: &Path, contents: &str) {
    fs::write(repo_root.join(".git").join("kindra.toml"), contents).unwrap();
}

#[allow(dead_code)]
pub fn current_branch(cwd: &Path) -> String {
    let output = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git branch --show-current failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[allow(dead_code)]
pub fn branch_exists(repo_root: &Path, branch: &str) -> bool {
    git_command(repo_root)
        .args(["rev-parse", "--verify", "--quiet", branch])
        .output()
        .expect("git rev-parse failed")
        .status
        .success()
}

#[allow(dead_code)]
pub fn managed_worktree_path(repo_root: &Path, relative: &str) -> PathBuf {
    repo_root.join(".git/kindra-worktrees").join(relative)
}

#[allow(dead_code)]
pub fn canonical_output_path(output: &[u8], cwd: &Path) -> PathBuf {
    let rendered = String::from_utf8_lossy(output);
    let path = Path::new(rendered.trim());
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    fs::canonicalize(absolute).unwrap()
}

#[allow(dead_code)]
pub fn assert_no_rebase_in_progress(repo_path: &Path) {
    let git_dir = repo_path.join(".git");
    let rebase_merge = git_dir.join("rebase-merge");
    let rebase_apply = git_dir.join("rebase-apply");
    let rebase_head = git_dir.join("REBASE_HEAD");

    assert!(
        !rebase_merge.exists(),
        "Rebase merge in progress at {:?}",
        rebase_merge
    );
    assert!(
        !rebase_apply.exists(),
        "Rebase apply in progress at {:?}",
        rebase_apply
    );
    assert!(
        !rebase_head.exists(),
        "Rebase head exists at {:?}",
        rebase_head
    );
}

/// Build the shape behind the trunk force-push incident: a bare remote, a local
/// clone-like repo with `main` pushed, and `branch` created off `origin/main` so
/// that `branch.<name>.merge` is `refs/heads/main` (git's `autoSetupMerge=true`
/// default for `git switch -c <name> origin/main`).
///
/// Returns the working repo dir and the remote dir.
#[allow(dead_code)]
pub fn setup_trunk_tracking_branch(
    branch: &str,
) -> (tempfile::TempDir, tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo_init(dir.path());

    make_commit(
        &repo,
        "refs/heads/main",
        "main.txt",
        "initial",
        "initial commit",
        &[],
    );

    let remote_dir = tempfile::tempdir().unwrap();
    // `--initial-branch` explicitly: a bare repo's HEAD otherwise follows the
    // ambient `init.defaultBranch`, which is `main` on many dev machines but unset
    // (so `master`) on CI. That leaves the remote's HEAD dangling at a branch that
    // is never created, which breaks cloning it below.
    run_ok(
        "git",
        &["init", "--bare", "--initial-branch=main"],
        remote_dir.path(),
    );
    run_ok(
        "git",
        &[
            "remote",
            "add",
            "origin",
            remote_dir.path().to_str().unwrap(),
        ],
        dir.path(),
    );
    run_ok("git", &["push", "-u", "origin", "main"], dir.path());

    // The footgun, built exactly as git's own default config builds it: with
    // `branch.autoSetupMerge=true`, branching off `origin/main` sets
    // `branch.<name>.merge = refs/heads/main`. Forced on the command line so this
    // reproduces a default machine even when the developer running the suite has
    // set `branch.autoSetupMerge=simple` — that setting avoids the footgun
    // locally, but it must not be what makes Kindra safe.
    run_ok(
        "git",
        &[
            "-c",
            "branch.autoSetupMerge=true",
            "checkout",
            "-b",
            branch,
            "origin/main",
        ],
        dir.path(),
    );
    let merge_config = std::process::Command::new("git")
        .args(["config", "--get", &format!("branch.{branch}.merge")])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&merge_config.stdout).trim(),
        "refs/heads/main",
        "fixture must produce a trunk-tracking branch, or it is not testing the bug",
    );

    {
        let branch_base = repo.head().unwrap().peel_to_commit().unwrap();
        make_commit(
            &repo,
            &format!("refs/heads/{branch}"),
            "work.txt",
            "work",
            "feat: branch work",
            &[&branch_base],
        );
    }
    repo.set_head(&format!("refs/heads/{branch}")).unwrap();
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();

    (dir, remote_dir, repo)
}

/// Advance `main` on the remote from a second clone, then fetch it into `dir`,
/// so `origin/main` is fresh (the implicit `--force-with-lease` is satisfied)
/// while the local stack branch does not contain those commits.
#[allow(dead_code)]
pub fn advance_remote_main(dir: &std::path::Path, remote_dir: &std::path::Path, commits: usize) {
    let other = tempfile::tempdir().unwrap();
    run_ok(
        "git",
        &[
            "clone",
            "--branch",
            "main",
            remote_dir.to_str().unwrap(),
            other.path().to_str().unwrap(),
        ],
        dir,
    );

    for i in 0..commits {
        fs::write(other.path().join(format!("trunk-{i}.txt")), format!("{i}")).unwrap();
        run_ok("git", &["add", "."], other.path());
        run_ok(
            "git",
            &["commit", "-m", &format!("trunk commit {i}")],
            other.path(),
        );
    }
    run_ok("git", &["push", "origin", "main"], other.path());
    run_ok("git", &["fetch", "origin"], dir);
}

#[allow(dead_code)]
pub fn remote_tip(remote_dir: &std::path::Path, refname: &str) -> git2::Oid {
    Repository::open(remote_dir)
        .unwrap()
        .find_reference(refname)
        .unwrap()
        .target()
        .unwrap()
}
