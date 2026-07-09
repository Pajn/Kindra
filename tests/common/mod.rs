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
