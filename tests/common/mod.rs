use assert_cmd::Command;
use git2::{Repository, Signature};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[allow(dead_code)]
pub fn kin_cmd() -> Command {
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kin");
    cmd.env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com");
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
pub fn write_repo_config(repo_root: &Path, contents: &str) {
    fs::write(repo_root.join(".git").join("kindra.toml"), contents).unwrap();
}

#[allow(dead_code)]
pub fn read_worktree_metadata(repo_root: &Path) -> Value {
    let raw = fs::read_to_string(repo_root.join(".git").join("kindra_worktrees.json")).unwrap();
    serde_json::from_str(&raw).unwrap()
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
