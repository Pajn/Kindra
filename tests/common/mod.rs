use assert_cmd::Command;
use git2::{Repository, Signature};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Environment variables that would let the invoking user's Git setup reach a
/// test: they redirect which config files, repository or work tree Git uses.
const LEAKY_GIT_ENV: &[&str] = &[
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES",
    "GIT_NAMESPACE",
];

/// A home directory shared by every test in this process that contains
/// nothing but a Git identity. Pointing `HOME` (and the platform config
/// variables) here keeps global Git config — which libgit2 reads from `HOME` —
/// and the global Kindra config out of tests.
fn hermetic_home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hermetic-home");
        fs::create_dir_all(&home).unwrap();
        // Test binaries run concurrently and share this directory, so publish
        // the file with an atomic rename rather than writing it in place.
        let mut staged = tempfile::NamedTempFile::new_in(&home).unwrap();
        staged
            .write_all(b"[user]\n\tname = Test User\n\temail = test@example.com\n")
            .unwrap();
        staged.persist(home.join(".gitconfig")).unwrap();
        home
    })
}

/// The environment [`kin_cmd`], [`run_ok`] and [`git_command`] run with:
/// `(variable, value)` pairs to set, plus [`LEAKY_GIT_ENV`] to remove.
/// [`apply_global_config_env`] overrides the home-related entries.
fn hermetic_env() -> Vec<(&'static str, OsString)> {
    hermetic_env_for(hermetic_home())
}

fn hermetic_env_for(home: &Path) -> Vec<(&'static str, OsString)> {
    vec![
        ("HOME", home.into()),
        ("XDG_CONFIG_HOME", home.join(".config").into()),
        ("APPDATA", home.join("AppData").join("Roaming").into()),
        ("LOCALAPPDATA", home.join("AppData").join("Local").into()),
        ("GIT_CONFIG_GLOBAL", home.join(".gitconfig").into()),
        ("GIT_CONFIG_NOSYSTEM", "1".into()),
    ]
}

fn hermetic_std_command(program: &str) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    for name in LEAKY_GIT_ENV {
        command.env_remove(name);
    }
    command.envs(hermetic_env());
    command
}

/// Where the global Kindra config lives for a home directory at `root`, as
/// resolved by `dirs::config_dir` on this platform.
#[allow(dead_code)]
pub fn test_global_config_dir(root: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        return root
            .join("Library")
            .join("Application Support")
            .join("kindra");
    }
    if cfg!(target_os = "windows") {
        return root.join("AppData").join("Roaming").join("kindra");
    }

    root.join(".config").join("kindra")
}

/// Point `cmd` at a home directory rooted at `root`, so a global Kindra config
/// written under [`test_global_config_dir`] is picked up.
#[allow(dead_code)]
pub fn apply_global_config_env(cmd: &mut Command, root: &Path) {
    cmd.envs(hermetic_env_for(root));
}

#[allow(dead_code)]
pub fn kin_cmd() -> Command {
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kin");
    for name in LEAKY_GIT_ENV {
        cmd.env_remove(name);
    }
    cmd.envs(hermetic_env())
        .env("GIT_AUTHOR_NAME", "Test User")
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
    let output = hermetic_std_command(program)
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
    let mut command = hermetic_std_command("git");
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

/// The repository config path as Kindra reports it in messages: inside the
/// canonicalized common Git directory of the repository at `repo_root`.
#[allow(dead_code)]
pub fn repo_config_path(repo_root: &Path) -> PathBuf {
    fs::canonicalize(repo_root.join(".git"))
        .unwrap()
        .join("kindra.toml")
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

/// Commit `file` to `branch` on the bare `remote_dir` from a throwaway clone and
/// push it, fetching into no other repository, so a local clone's
/// remote-tracking ref for `branch` goes stale. The branch starts from the
/// remote's `main` when it does not exist there yet. Returns the new remote tip.
#[allow(dead_code)]
pub fn push_remote_commit(remote_dir: &Path, branch: &str, file: &str) -> git2::Oid {
    let other = tempfile::tempdir().unwrap();
    run_ok(
        "git",
        &[
            "clone",
            remote_dir.to_str().unwrap(),
            other.path().to_str().unwrap(),
        ],
        remote_dir,
    );
    let refname = format!("refs/heads/{branch}");
    let exists = Repository::open(remote_dir)
        .unwrap()
        .find_reference(&refname)
        .is_ok();
    let start = if exists {
        format!("origin/{branch}")
    } else {
        "origin/main".to_string()
    };
    run_ok("git", &["checkout", "-B", branch, &start], other.path());
    fs::write(other.path().join(file), file).unwrap();
    run_ok("git", &["add", file], other.path());
    run_ok(
        "git",
        &["commit", "-m", &format!("add {file}")],
        other.path(),
    );
    run_ok(
        "git",
        &["push", "origin", &format!("HEAD:{refname}")],
        other.path(),
    );
    remote_tip(remote_dir, &refname)
}

/// Add a linked worktree for `branch` in a fresh temp directory outside the
/// repository. Returns the temp dir (keep it alive for the test) and the
/// worktree's path.
#[allow(dead_code)]
pub fn add_linked_worktree(repo_root: &Path, branch: &str) -> (tempfile::TempDir, PathBuf) {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("linked");
    run_ok(
        "git",
        &["worktree", "add", path.to_str().unwrap(), branch],
        repo_root,
    );
    (parent, path)
}

/// A native Git operation a test leaves stopped in a worktree.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeStop {
    Merge,
    CherryPick,
    Revert,
    Am,
}

/// The file every [`stop_native_operation`] conflict is in.
#[allow(dead_code)]
pub const NATIVE_CONFLICT_FILE: &str = "native-conflict.txt";

#[allow(dead_code)]
fn commit_native_conflict_file(cwd: &Path, content: &str) {
    fs::write(cwd.join(NATIVE_CONFLICT_FILE), content).unwrap();
    run_ok("git", &["add", NATIVE_CONFLICT_FILE], cwd);
    run_ok("git", &["commit", "-m", content.trim()], cwd);
}

/// Stop `op` on a conflict in [`NATIVE_CONFLICT_FILE`] in the worktree at
/// `cwd`, after committing "our" side of the conflict to the current branch.
/// With `resolve`, the conflict is resolved and staged, so only the
/// operation's final commit is missing. The side commit the operation applies
/// has no branch, so it never shows up in stack discovery.
#[allow(dead_code)]
pub fn stop_native_operation(cwd: &Path, op: NativeStop, resolve: bool) {
    let output = if op == NativeStop::Revert {
        commit_native_conflict_file(cwd, "zero\n");
        commit_native_conflict_file(cwd, "one\n");
        commit_native_conflict_file(cwd, "two\n");
        git_command(cwd)
            .args(["revert", "--no-edit", "HEAD~1"])
            .output()
            .unwrap()
    } else {
        run_ok("git", &["switch", "-q", "-c", "native-side"], cwd);
        commit_native_conflict_file(cwd, "side\n");
        run_ok("git", &["switch", "-q", "-"], cwd);
        commit_native_conflict_file(cwd, "ours\n");
        let output = match op {
            NativeStop::Merge => git_command(cwd)
                .args(["merge", "--no-edit", "native-side"])
                .output()
                .unwrap(),
            NativeStop::CherryPick => git_command(cwd)
                .args(["cherry-pick", "native-side"])
                .output()
                .unwrap(),
            _ => {
                let patch = git_command(cwd)
                    .args(["format-patch", "-1", "--stdout", "native-side"])
                    .output()
                    .unwrap();
                assert!(patch.status.success());
                let mut child = git_command(cwd)
                    .arg("am")
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                use std::io::Write;
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(&patch.stdout)
                    .unwrap();
                child.wait_with_output().unwrap()
            }
        };
        run_ok("git", &["branch", "-D", "-q", "native-side"], cwd);
        output
    };
    assert!(
        !output.status.success(),
        "{op:?} was expected to stop on a conflict\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_ne!(
        Repository::open(cwd).unwrap().state(),
        git2::RepositoryState::Clean,
        "{op:?} did not leave an operation in progress"
    );
    if resolve {
        assert_ne!(
            op,
            NativeStop::Am,
            "git am has no resolved-but-uncommitted state"
        );
        fs::write(cwd.join(NATIVE_CONFLICT_FILE), "resolved\n").unwrap();
        run_ok("git", &["add", NATIVE_CONFLICT_FILE], cwd);
    }
}

/// Start a native `git bisect` between `bad` and `good`; Git checks out a
/// midpoint commit, detaching HEAD.
#[allow(dead_code)]
pub fn start_native_bisect(cwd: &Path, bad: &str, good: &str) {
    run_ok("git", &["bisect", "start", bad, good], cwd);
    assert_eq!(
        Repository::open(cwd).unwrap().state(),
        git2::RepositoryState::Bisect
    );
}

/// Persist a paused Kindra operation (a reorder of `branch` stopped
/// mid-rebase) that reconciliation keeps in place.
#[allow(dead_code)]
pub fn write_paused_operation(repo: &Repository, branch: &str) {
    fs::write(
        StateFile::Rebase.in_git_dir(repo.path()),
        format!(
            r#"{{"operation":"Reorder","original_branch":"{branch}","target_branch":"main",
            "remaining_branches":[],"in_progress_branch":"{branch}"}}"#
        ),
    )
    .unwrap();
}

/// Assert no Kindra operation state was persisted in the worktree at `cwd`.
#[allow(dead_code)]
pub fn assert_no_kindra_operation(cwd: &Path) {
    let repo = Repository::open(cwd).unwrap();
    for file in StateFile::ALL {
        let name = file.file_name();
        assert!(
            !file.in_git_dir(repo.path()).exists(),
            "{name} was persisted by a refused command"
        );
    }
}

/// Check out `branch` and commit `content` to `file` on it, with the subject
/// `<branch> <content>`.
#[allow(dead_code)]
pub fn commit_on(root: &Path, branch: &str, file: &str, content: &str) {
    run_ok("git", &["checkout", branch], root);
    fs::write(root.join(file), content).unwrap();
    run_ok("git", &["add", file], root);
    run_ok(
        "git",
        &["commit", "-m", &format!("{branch} {content}")],
        root,
    );
}

/// Merge `merged` into `branch` with a merge commit, as `git merge` does when a
/// branch takes its parent's new commits instead of being rebased onto them.
#[allow(dead_code)]
pub fn merge_into(root: &Path, branch: &str, merged: &str) {
    run_ok("git", &["checkout", branch], root);
    run_ok("git", &["merge", "--no-ff", "--no-edit", merged], root);
}

/// Everything a refused command must leave as it was in the worktree at `cwd`:
/// every ref (branches, stashes, Kindra's own), what HEAD names, the working
/// tree and index, and the Kindra files in the Git directory (operation state
/// and the undo log) other than the repository lock.
#[allow(dead_code)]
pub fn repository_snapshot(cwd: &Path) -> String {
    let git = |args: &[&str]| {
        let output = git_command(cwd).args(args).output().unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let repo = Repository::open(cwd).unwrap();
    let mut kindra_files = fs::read_dir(repo.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        // Every command takes the lock, refused or not.
        .filter(|name| name.starts_with("kindra") && name != "kindra.lock")
        .collect::<Vec<_>>();
    kindra_files.sort();
    format!(
        "refs:\n{}HEAD: {}status:\n{}kindra files: {kindra_files:?}",
        git(&["for-each-ref", "--format=%(refname) %(objectname)"]),
        git(&["symbolic-ref", "-q", "HEAD"]),
        git(&["status", "--porcelain=v1", "--untracked-files=all"]),
    )
}

/// A file Kindra persists an operation's progress in, inside a worktree's Git
/// directory. The names are part of the on-disk format, so tests spell them
/// out here once instead of borrowing them from the crate: renaming one in the
/// implementation then fails tests rather than silently orphaning operations
/// paused by an older Kindra.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateFile {
    /// Every operation driven by the rebase loop (move, restack, reorder,
    /// sync, commit, absorb).
    Rebase,
    /// An interrupted `kin run`.
    Run,
    /// Checkout hydration.
    Checkout,
}

#[allow(dead_code)]
impl StateFile {
    pub const ALL: [Self; 3] = [Self::Rebase, Self::Run, Self::Checkout];

    pub fn file_name(self) -> &'static str {
        match self {
            Self::Rebase => "kindra_rebase_state.json",
            Self::Run => "kindra_run_state.json",
            Self::Checkout => "kindra_checkout_state.json",
        }
    }

    /// This state file inside the Git directory `git_dir` (`repo.path()`).
    pub fn in_git_dir(self, git_dir: &Path) -> PathBuf {
        git_dir.join(self.file_name())
    }
}

/// Where the worktree checked out at `worktree` keeps `file`: inside its own
/// Git directory, which for a linked worktree is not `<worktree>/.git`.
#[allow(dead_code)]
pub fn state_file(worktree: &Path, file: StateFile) -> PathBuf {
    let repo = Repository::open(worktree).unwrap();
    file.in_git_dir(repo.path())
}

/// Where the worktree at `worktree` keeps the state of a rebase-based
/// operation.
#[allow(dead_code)]
pub fn rebase_state_file(worktree: &Path) -> PathBuf {
    state_file(worktree, StateFile::Rebase)
}

/// The smallest rebase state Kindra accepts: only the fields without a serde
/// default. Every other field takes the value an older Kindra that never wrote
/// it leaves behind.
#[allow(dead_code)]
pub const MINIMAL_REBASE_STATE_JSON: &str = r#"{
  "operation": "Move",
  "original_branch": "",
  "target_branch": "",
  "remaining_branches": [],
  "in_progress_branch": null
}"#;

/// A `RebaseState` for tests, deserialized from [`MINIMAL_REBASE_STATE_JSON`]
/// so tests keep compiling when the state gains a field. Set the fields a test
/// cares about with struct update syntax:
///
/// ```ignore
/// let state = RebaseState {
///     owned_tip_map: HashMap::from([("main".to_string(), tip.to_string())]),
///     ..rebase_state(Operation::Commit, "main", "main")
/// };
/// ```
#[allow(dead_code)]
pub fn rebase_state(
    operation: kindra::rebase_utils::Operation,
    original_branch: &str,
    target_branch: &str,
) -> kindra::rebase_utils::RebaseState {
    let mut state: kindra::rebase_utils::RebaseState =
        serde_json::from_str(MINIMAL_REBASE_STATE_JSON).unwrap();
    state.operation = operation;
    state.original_branch = original_branch.to_string();
    state.target_branch = target_branch.to_string();
    state
}

/// A shell snippet for a mock (`gh`) or Git hook that pauses the `kin`
/// process running it: it creates `$KIN_TEST_BLOCK_DIR/started`, then waits up
/// to 30 seconds for `$KIN_TEST_BLOCK_DIR/release`. Used with
/// [`run_while_blocked`].
#[allow(dead_code)]
pub const BLOCK_UNTIL_RELEASED: &str = r#"touch "$KIN_TEST_BLOCK_DIR/started"
for _ in $(seq 1 300); do
    [ -e "$KIN_TEST_BLOCK_DIR/release" ] && break
    sleep 0.1
done"#;

/// Run `cmd`, whose mock or hook contains [`BLOCK_UNTIL_RELEASED`], and call
/// `during` while it is paused there; then release it and return its output.
/// `cmd` is released even when `during` panics, and waiting for it to pause
/// is bounded, so a failure cannot hang the suite.
#[allow(dead_code)]
pub fn run_while_blocked(mut cmd: Command, during: impl FnOnce()) -> std::process::Output {
    struct Release(PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, "");
        }
    }

    let block_dir = tempfile::tempdir().unwrap();
    cmd.env("KIN_TEST_BLOCK_DIR", block_dir.path());
    let started = block_dir.path().join("started");
    let release = Release(block_dir.path().join("release"));
    let running = std::thread::spawn(move || cmd.output().unwrap());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !started.exists() {
        if running.is_finished() {
            drop(release);
            panic!(
                "command finished before reaching the blocking point: {:?}",
                running.join().unwrap()
            );
        }
        assert!(
            std::time::Instant::now() < deadline,
            "command did not reach the blocking point"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    during();
    drop(release);
    running.join().unwrap()
}

/// Assert that a command which rewrites branches (`kin restack`) is refused in
/// `cwd` because another `kin` process holds the repository lock.
#[allow(dead_code)]
pub fn assert_repository_locked(cwd: &Path) {
    kin_cmd()
        .arg("restack")
        .current_dir(cwd)
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Another 'kin' process is operating on this repository",
        ));
}

/// Another repository, which a test names in Git's repository variables the
/// way Git does for the hooks it runs and wrappers do for the commands they
/// wrap. Kindra works on the repository it discovers from its working
/// directory, so every Git command it runs must leave this one as it was.
#[allow(dead_code)]
pub struct ForeignRepository {
    dir: tempfile::TempDir,
    before: String,
}

#[allow(dead_code)]
impl ForeignRepository {
    /// A repository with one commit on `main`, whose files and branches are
    /// not those of any test fixture.
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        repo_init(dir.path());
        fs::write(dir.path().join("foreign.txt"), "foreign").unwrap();
        run_ok("git", &["add", "foreign.txt"], dir.path());
        run_ok("git", &["commit", "-m", "foreign"], dir.path());
        let before = repository_snapshot(dir.path());
        Self { dir, before }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Name this repository's Git directory, work tree and index in `cmd`'s
    /// environment.
    pub fn name_in<'a>(&self, cmd: &'a mut Command) -> &'a mut Command {
        let git_dir = self.path().join(".git");
        cmd.env("GIT_DIR", &git_dir)
            .env("GIT_WORK_TREE", self.path())
            .env("GIT_INDEX_FILE", git_dir.join("index"))
    }

    /// A `kin` command whose environment names this repository.
    pub fn kin_cmd(&self) -> Command {
        let mut cmd = kin_cmd();
        self.name_in(&mut cmd);
        cmd
    }

    /// Assert nothing changed here: refs, HEAD, index, work tree and Kindra
    /// files.
    pub fn assert_untouched(&self) {
        assert_eq!(
            repository_snapshot(self.path()),
            self.before,
            "the repository named in the environment changed"
        );
    }
}
