use git2::Repository;
use tempfile::TempDir;

mod common;
use common::{make_commit, run_ok};

#[test]
fn test_restack_basic() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = Repository::init(repo_path).unwrap();

    // Setup git config
    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    // 1. Commit A on main
    let head_oid = make_commit(&repo, "HEAD", "a.txt", "A", "feat: A", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);

    // 2. Create branch 'feat' off main, commit B
    run_ok("git", &["checkout", "-b", "feat"], repo_path);
    let _feat_oid = make_commit(
        &repo,
        "HEAD",
        "b.txt",
        "B",
        "feat: B",
        &[&repo.find_commit(head_oid).unwrap()],
    );

    // 3. Checkout main, Amend A (changes content, but keeps summary)
    run_ok("git", &["checkout", "main"], repo_path);
    std::fs::write(repo_path.join("a.txt"), "A amended").unwrap();
    run_ok("git", &["add", "a.txt"], repo_path);
    run_ok("git", &["commit", "--amend", "-m", "feat: A"], repo_path);

    let new_head_oid = repo.head().unwrap().target().unwrap();
    assert_ne!(head_oid, new_head_oid);

    // 4. Run restack
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.current_dir(repo_path).arg("restack").assert().success();

    // 5. Verify feat
    run_ok("git", &["checkout", "feat"], repo_path);
    let feat_new_oid = repo.head().unwrap().target().unwrap();
    let feat_commit = repo.find_commit(feat_new_oid).unwrap();
    let parent_oid = feat_commit.parent_id(0).unwrap();

    assert_eq!(
        parent_oid, new_head_oid,
        "feat should be rebased onto new main"
    );
}

#[test]
fn test_restack_unrelated() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = Repository::init(repo_path).unwrap();

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    // Initial commit
    let root_oid = make_commit(&repo, "HEAD", "root.txt", "Root", "Initial", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);

    // Commit A on main
    make_commit(
        &repo,
        "HEAD",
        "a.txt",
        "A",
        "feat: A",
        &[&repo.find_commit(root_oid).unwrap()],
    );

    // Branch other OFF ROOT (unrelated to A)
    // HEAD is currently A. HEAD^ is root.
    run_ok("git", &["checkout", "-b", "other"], repo_path);
    run_ok("git", &["reset", "--hard", "HEAD^"], repo_path);

    // Make commit C on other
    let c_oid = make_commit(
        &repo,
        "HEAD",
        "c.txt",
        "C",
        "feat: C",
        &[&repo.find_commit(root_oid).unwrap()],
    );

    // Checkout main, Amend A
    run_ok("git", &["checkout", "main"], repo_path);
    std::fs::write(repo_path.join("a.txt"), "A amended").unwrap();
    run_ok("git", &["add", "a.txt"], repo_path);
    run_ok("git", &["commit", "--amend", "-m", "feat: A"], repo_path);

    // Run restack
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.current_dir(repo_path).arg("restack").assert().success();

    // Verify other did NOT move
    run_ok("git", &["checkout", "other"], repo_path);
    let other_head = repo.head().unwrap().target().unwrap();
    assert_eq!(other_head, c_oid, "Unrelated branch should not move");
}

#[test]
fn test_restack_multiple_children() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = Repository::init(repo_path).unwrap();

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    // Commit A
    let head_oid = make_commit(&repo, "HEAD", "a.txt", "A", "feat: A", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);

    // Branch feat1, Commit B
    run_ok("git", &["checkout", "-b", "feat1"], repo_path);
    make_commit(
        &repo,
        "HEAD",
        "b.txt",
        "B",
        "feat: B",
        &[&repo.find_commit(head_oid).unwrap()],
    );

    // Branch feat2, Commit C (also off A)
    run_ok("git", &["checkout", "main"], repo_path);
    run_ok("git", &["checkout", "-b", "feat2"], repo_path);
    make_commit(
        &repo,
        "HEAD",
        "c.txt",
        "C",
        "feat: C",
        &[&repo.find_commit(head_oid).unwrap()],
    );

    // Amend A
    run_ok("git", &["checkout", "main"], repo_path);
    std::fs::write(repo_path.join("a.txt"), "A amended").unwrap();
    run_ok("git", &["add", "a.txt"], repo_path);
    run_ok("git", &["commit", "--amend", "-m", "feat: A"], repo_path);
    let new_head_oid = repo.head().unwrap().target().unwrap();

    // Run restack
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.current_dir(repo_path).arg("restack").assert().success();

    // Verify feat1
    run_ok("git", &["checkout", "feat1"], repo_path);
    let feat1_head = repo.head().unwrap().target().unwrap();
    let feat1_parent = repo.find_commit(feat1_head).unwrap().parent_id(0).unwrap();
    assert_eq!(feat1_parent, new_head_oid);

    // Verify feat2
    run_ok("git", &["checkout", "feat2"], repo_path);
    let feat2_head = repo.head().unwrap().target().unwrap();
    let feat2_parent = repo.find_commit(feat2_head).unwrap().parent_id(0).unwrap();
    assert_eq!(feat2_parent, new_head_oid);
}

#[test]
fn test_restack_conflict() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = Repository::init(repo_path).unwrap();

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    // 1. Commit A on main
    let a_oid = make_commit(&repo, "HEAD", "conflict.txt", "A", "feat: A", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);

    // 2. Branch 'feat' off main, commit B (conflicting change)
    run_ok("git", &["checkout", "-b", "feat"], repo_path);
    make_commit(
        &repo,
        "HEAD",
        "conflict.txt",
        "B",
        "feat: B",
        &[&repo.find_commit(a_oid).unwrap()],
    );

    // 3. Checkout main, Amend A (conflicting change)
    run_ok("git", &["checkout", "main"], repo_path);
    std::fs::write(repo_path.join("conflict.txt"), "A amended").unwrap();
    run_ok("git", &["add", "conflict.txt"], repo_path);
    run_ok("git", &["commit", "--amend", "-m", "feat: A"], repo_path);

    // 4. Run restack - should fail due to conflict
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.current_dir(repo_path).arg("restack").assert().failure();

    // 5. Assert we are in a rebase state
    assert!(
        repo_path.join(".git/REBASE_HEAD").exists()
            || repo_path.join(".git/rebase-merge").exists()
            || repo_path.join(".git/rebase-apply").exists()
    );
}

#[test]
fn test_restack_continue() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = Repository::init(repo_path).unwrap();

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    // Setup conflict
    let a_oid = make_commit(&repo, "HEAD", "conflict.txt", "A", "feat: A", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);
    run_ok("git", &["checkout", "-b", "feat"], repo_path);
    make_commit(
        &repo,
        "HEAD",
        "conflict.txt",
        "B",
        "feat: B",
        &[&repo.find_commit(a_oid).unwrap()],
    );
    run_ok("git", &["checkout", "main"], repo_path);
    std::fs::write(repo_path.join("conflict.txt"), "A amended").unwrap();
    run_ok("git", &["add", "conflict.txt"], repo_path);
    run_ok("git", &["commit", "--amend", "-m", "feat: A"], repo_path);

    // Run restack
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.current_dir(repo_path).arg("restack").assert().failure();

    // Resolve conflict
    std::fs::write(repo_path.join("conflict.txt"), "Resolved").unwrap();
    run_ok("git", &["add", "conflict.txt"], repo_path);

    // Run gits continue
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.current_dir(repo_path)
        .env("GIT_EDITOR", "cat")
        .arg("continue")
        .assert()
        .success();

    // Verify feat is rebased
    run_ok("git", &["checkout", "feat"], repo_path);
    let feat_head = repo.head().unwrap().target().unwrap();
    let feat_parent = repo.find_commit(feat_head).unwrap().parent_id(0).unwrap();
    let main_head = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap();
    assert_eq!(feat_parent, main_head);
}

#[test]
fn test_restack_abort() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = Repository::init(repo_path).unwrap();

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    // Setup conflict
    let a_oid = make_commit(&repo, "HEAD", "conflict.txt", "A", "feat: A", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);
    run_ok("git", &["checkout", "-b", "feat"], repo_path);
    let b_oid = make_commit(
        &repo,
        "HEAD",
        "conflict.txt",
        "B",
        "feat: B",
        &[&repo.find_commit(a_oid).unwrap()],
    );
    run_ok("git", &["checkout", "main"], repo_path);
    std::fs::write(repo_path.join("conflict.txt"), "A amended").unwrap();
    run_ok("git", &["add", "conflict.txt"], repo_path);
    run_ok("git", &["commit", "--amend", "-m", "feat: A"], repo_path);

    // Run restack
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.current_dir(repo_path).arg("restack").assert().failure();

    // Run gits abort
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.current_dir(repo_path).arg("abort").assert().success();

    // Verify feat is back to b_oid
    let feat_oid = repo
        .find_reference("refs/heads/feat")
        .unwrap()
        .target()
        .unwrap();
    assert_eq!(feat_oid, b_oid);

    // Verify we are back on main
    let head_name = repo.head().unwrap().shorthand().unwrap().to_string();
    assert_eq!(head_name, "main");
}
