use git2::Repository;
use tempfile::TempDir;

mod common;
use common::{gits_cmd, make_commit, run_ok};

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
fn test_restack_ignores_stale_branch_pointing_at_old_base() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = Repository::init(repo_path).unwrap();

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    let old_main_oid = make_commit(&repo, "HEAD", "a.txt", "A", "feat: A", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);

    run_ok("git", &["branch", "stale-base"], repo_path);

    run_ok("git", &["checkout", "main"], repo_path);
    std::fs::write(repo_path.join("a.txt"), "A amended").unwrap();
    run_ok("git", &["add", "a.txt"], repo_path);
    run_ok("git", &["commit", "--amend", "-m", "feat: A"], repo_path);

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.current_dir(repo_path).arg("restack").assert().success();

    let stale_oid = repo
        .find_branch("stale-base", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    assert_eq!(
        stale_oid, old_main_oid,
        "Branch pointing at the rewritten base commit should not be restacked"
    );
}

#[test]
fn test_restack_continues_past_tip_tree_match() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = Repository::init(repo_path).unwrap();

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    let _old_main_oid = make_commit(&repo, "HEAD", "a.txt", "A", "feat: A", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);

    run_ok("git", &["checkout", "-b", "feat"], repo_path);
    run_ok(
        "git",
        &["commit", "--allow-empty", "-m", "empty child"],
        repo_path,
    );

    run_ok("git", &["checkout", "main"], repo_path);
    run_ok(
        "git",
        &["commit", "--amend", "-m", "feat: A rewritten"],
        repo_path,
    );
    let new_main_oid = repo.head().unwrap().target().unwrap();

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.current_dir(repo_path).arg("restack").assert().success();

    run_ok("git", &["checkout", "feat"], repo_path);
    let feat_head = repo.head().unwrap().target().unwrap();
    let feat_parent = repo.find_commit(feat_head).unwrap().parent_id(0).unwrap();
    assert_eq!(
        feat_parent, new_main_oid,
        "restack should continue scanning when the branch tip shares the target tree"
    );
}

#[test]
fn test_restack_ignores_metadata_match_on_other_lineage() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = Repository::init(repo_path).unwrap();

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    let root_oid = make_commit(&repo, "HEAD", "root.txt", "root", "root", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);

    let main_base_oid = make_commit(
        &repo,
        "HEAD",
        "a.txt",
        "A",
        "shared summary",
        &[&repo.find_commit(root_oid).unwrap()],
    );

    run_ok("git", &["checkout", "-b", "feat"], repo_path);
    make_commit(
        &repo,
        "HEAD",
        "feat.txt",
        "feat",
        "feat child",
        &[&repo.find_commit(main_base_oid).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "noise", &root_oid.to_string()],
        repo_path,
    );
    let noise_oid = make_commit(
        &repo,
        "HEAD",
        "noise.txt",
        "noise",
        "shared summary",
        &[&repo.find_commit(root_oid).unwrap()],
    );

    run_ok("git", &["checkout", "main"], repo_path);
    std::fs::write(repo_path.join("a.txt"), "A amended").unwrap();
    run_ok("git", &["add", "a.txt"], repo_path);
    run_ok(
        "git",
        &["commit", "--amend", "-m", "shared summary"],
        repo_path,
    );
    let new_main_oid = repo.head().unwrap().target().unwrap();

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.current_dir(repo_path).arg("restack").assert().success();

    run_ok("git", &["checkout", "feat"], repo_path);
    let feat_head = repo.head().unwrap().target().unwrap();
    let feat_parent = repo.find_commit(feat_head).unwrap().parent_id(0).unwrap();
    assert_eq!(feat_parent, new_main_oid);

    run_ok("git", &["checkout", "noise"], repo_path);
    let noise_head = repo.head().unwrap().target().unwrap();
    assert_eq!(
        noise_head, noise_oid,
        "metadata-only matches on a different first-parent lineage must not restack"
    );
}

#[test]
fn test_restack_ignores_metadata_only_match_on_sibling_commit() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = Repository::init(repo_path).unwrap();

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    let root_oid = make_commit(&repo, "HEAD", "root.txt", "root", "root", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);

    let shared_parent_oid = make_commit(
        &repo,
        "HEAD",
        "shared.txt",
        "shared",
        "shared",
        &[&repo.find_commit(root_oid).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "old-base", &shared_parent_oid.to_string()],
        repo_path,
    );
    let old_base_oid = make_commit(
        &repo,
        "HEAD",
        "base.txt",
        "branch one",
        "fixup",
        &[&repo.find_commit(shared_parent_oid).unwrap()],
    );
    run_ok("git", &["reset", "--hard", "HEAD"], repo_path);

    run_ok(
        "git",
        &["checkout", "-b", "feat", &old_base_oid.to_string()],
        repo_path,
    );
    let old_feat_oid = make_commit(
        &repo,
        "HEAD",
        "feat.txt",
        "feat child",
        "feat child",
        &[&repo.find_commit(old_base_oid).unwrap()],
    );
    run_ok("git", &["reset", "--hard", "HEAD"], repo_path);

    run_ok("git", &["checkout", "main"], repo_path);
    let rewritten_main_oid = make_commit(
        &repo,
        "HEAD",
        "base.txt",
        "branch two",
        "fixup",
        &[&repo.find_commit(shared_parent_oid).unwrap()],
    );

    let mut cmd = gits_cmd();
    cmd.current_dir(repo_path).arg("restack").assert().success();

    let feat_oid = repo
        .find_reference("refs/heads/feat")
        .unwrap()
        .target()
        .unwrap();
    assert_eq!(
        feat_oid, old_feat_oid,
        "metadata-only sibling matches must not restack the branch"
    );

    let old_base_head = repo
        .find_reference("refs/heads/old-base")
        .unwrap()
        .target()
        .unwrap();
    assert_eq!(old_base_head, old_base_oid);
    assert_eq!(repo.head().unwrap().target().unwrap(), rewritten_main_oid);
}

#[test]
fn test_restack_matches_earlier_rewritten_commit_in_target_history() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = Repository::init(repo_path).unwrap();

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    let root_oid = make_commit(&repo, "HEAD", "root.txt", "root", "root", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);

    let a_oid = make_commit(
        &repo,
        "HEAD",
        "a.txt",
        "A",
        "commit A",
        &[&repo.find_commit(root_oid).unwrap()],
    );
    let b_oid = make_commit(
        &repo,
        "HEAD",
        "b.txt",
        "B",
        "commit B",
        &[&repo.find_commit(a_oid).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "feat", &a_oid.to_string()],
        repo_path,
    );
    let old_feat_oid = make_commit(
        &repo,
        "HEAD",
        "feat.txt",
        "C",
        "commit C",
        &[&repo.find_commit(a_oid).unwrap()],
    );

    run_ok("git", &["checkout", "main"], repo_path);
    run_ok(
        "git",
        &["reset", "--hard", &root_oid.to_string()],
        repo_path,
    );
    run_ok("git", &["cherry-pick", &a_oid.to_string()], repo_path);
    run_ok("git", &["cherry-pick", &b_oid.to_string()], repo_path);
    let rewritten_main_oid = repo.head().unwrap().target().unwrap();

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.current_dir(repo_path).arg("restack").assert().success();

    run_ok("git", &["checkout", "feat"], repo_path);
    let new_feat_oid = repo.head().unwrap().target().unwrap();
    let new_feat_parent = repo
        .find_commit(new_feat_oid)
        .unwrap()
        .parent_id(0)
        .unwrap();

    assert_ne!(
        new_feat_oid, old_feat_oid,
        "restack should rewrite a child branch based on an earlier rewritten commit"
    );
    assert_eq!(
        new_feat_parent, rewritten_main_oid,
        "restack should recognize rewritten commits earlier in the target history"
    );
}

fn setup_deep_rewritten_base_scenario() -> (TempDir, Repository, git2::Oid, git2::Oid) {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = Repository::init(repo_path).unwrap();

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    let root_oid = make_commit(&repo, "HEAD", "root.txt", "root", "root", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);

    let old_base_oid = make_commit(
        &repo,
        "HEAD",
        "base.txt",
        "base",
        "base",
        &[&repo.find_commit(root_oid).unwrap()],
    );

    let mut parent_oid = old_base_oid;
    for i in 0..104 {
        parent_oid = make_commit(
            &repo,
            "HEAD",
            &format!("main-{i}.txt"),
            &format!("main {i}"),
            &format!("main {i}"),
            &[&repo.find_commit(parent_oid).unwrap()],
        );
    }
    let old_main_tip = repo.head().unwrap().target().unwrap();

    run_ok(
        "git",
        &["checkout", "-b", "feat", &old_base_oid.to_string()],
        repo_path,
    );
    let old_feat_oid = make_commit(
        &repo,
        "HEAD",
        "feat.txt",
        "feat child",
        "feat child",
        &[&repo.find_commit(old_base_oid).unwrap()],
    );

    run_ok("git", &["checkout", "main"], repo_path);
    run_ok(
        "git",
        &["reset", "--hard", &root_oid.to_string()],
        repo_path,
    );
    let range = format!("{}^..{}", old_base_oid, old_main_tip);
    run_ok("git", &["cherry-pick", &range], repo_path);
    let rewritten_main_oid = repo.head().unwrap().target().unwrap();

    (temp, repo, old_feat_oid, rewritten_main_oid)
}

fn test_global_config_dir(root: &std::path::Path) -> std::path::PathBuf {
    if cfg!(target_os = "macos") {
        return root
            .join("Library")
            .join("Application Support")
            .join("gits");
    }
    if cfg!(target_os = "windows") {
        return root.join("AppData").join("Roaming").join("gits");
    }

    root.join(".config").join("gits")
}

fn apply_global_config_env(cmd: &mut assert_cmd::Command, root: &std::path::Path) {
    cmd.env("HOME", root);

    if cfg!(target_os = "linux") || cfg!(target_os = "freebsd") || cfg!(target_os = "openbsd") {
        cmd.env("XDG_CONFIG_HOME", root.join(".config"));
    }

    if cfg!(target_os = "windows") {
        cmd.env("APPDATA", root.join("AppData").join("Roaming"));
        cmd.env("LOCALAPPDATA", root.join("AppData").join("Local"));
    }
}

#[test]
fn test_restack_default_history_limit_skips_deep_rewritten_base() {
    let (temp, repo, old_feat_oid, _rewritten_main_oid) = setup_deep_rewritten_base_scenario();
    let repo_path = temp.path();

    let mut cmd = gits_cmd();
    cmd.current_dir(repo_path).arg("restack").assert().success();

    run_ok("git", &["checkout", "feat"], repo_path);
    let feat_oid = repo.head().unwrap().target().unwrap();
    assert_eq!(
        feat_oid, old_feat_oid,
        "default history limit should avoid scanning beyond one hundred commits"
    );
}

#[test]
fn test_restack_matches_base_beyond_one_hundred_rewritten_commits_with_cli_override() {
    let (temp, repo, old_feat_oid, rewritten_main_oid) = setup_deep_rewritten_base_scenario();
    let repo_path = temp.path();

    let mut cmd = gits_cmd();
    cmd.current_dir(repo_path)
        .arg("restack")
        .arg("--history-limit")
        .arg("300")
        .assert()
        .success();

    run_ok("git", &["checkout", "feat"], repo_path);
    let new_feat_oid = repo.head().unwrap().target().unwrap();
    let new_feat_parent = repo
        .find_commit(new_feat_oid)
        .unwrap()
        .parent_id(0)
        .unwrap();

    assert_ne!(
        new_feat_oid, old_feat_oid,
        "restack should still find a rewritten base deeper than one hundred commits"
    );
    assert_eq!(new_feat_parent, rewritten_main_oid);
}

#[test]
fn test_restack_repo_config_overrides_default_history_limit() {
    let (temp, repo, old_feat_oid, rewritten_main_oid) = setup_deep_rewritten_base_scenario();
    let repo_path = temp.path();

    std::fs::write(
        repo.path().join("gits.toml"),
        "[restack]\nhistory_limit = 300\n",
    )
    .unwrap();

    let mut cmd = gits_cmd();
    cmd.current_dir(repo_path).arg("restack").assert().success();

    run_ok("git", &["checkout", "feat"], repo_path);
    let new_feat_oid = repo.head().unwrap().target().unwrap();
    let new_feat_parent = repo
        .find_commit(new_feat_oid)
        .unwrap()
        .parent_id(0)
        .unwrap();

    assert_ne!(new_feat_oid, old_feat_oid);
    assert_eq!(new_feat_parent, rewritten_main_oid);
}

#[test]
fn test_restack_global_config_overrides_default_history_limit() {
    let (temp, repo, old_feat_oid, rewritten_main_oid) = setup_deep_rewritten_base_scenario();
    let repo_path = temp.path();
    let global_config_root = TempDir::new().unwrap();
    let global_config_dir = test_global_config_dir(global_config_root.path());
    std::fs::create_dir_all(&global_config_dir).unwrap();
    std::fs::write(
        global_config_dir.join("config.toml"),
        "[restack]\nhistory_limit = 300\n",
    )
    .unwrap();

    let mut cmd = gits_cmd();
    cmd.current_dir(repo_path);
    apply_global_config_env(&mut cmd, global_config_root.path());
    cmd.arg("restack").assert().success();

    run_ok("git", &["checkout", "feat"], repo_path);
    let new_feat_oid = repo.head().unwrap().target().unwrap();
    let new_feat_parent = repo
        .find_commit(new_feat_oid)
        .unwrap()
        .parent_id(0)
        .unwrap();

    assert_ne!(new_feat_oid, old_feat_oid);
    assert_eq!(new_feat_parent, rewritten_main_oid);
}

#[test]
fn test_restack_cli_override_takes_precedence_over_repo_and_global_config() {
    let (temp, repo, old_feat_oid, rewritten_main_oid) = setup_deep_rewritten_base_scenario();
    let repo_path = temp.path();
    let global_config_root = TempDir::new().unwrap();
    let global_config_dir = test_global_config_dir(global_config_root.path());
    std::fs::create_dir_all(&global_config_dir).unwrap();
    std::fs::write(
        global_config_dir.join("config.toml"),
        "[restack]\nhistory_limit = 50\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join("gits.toml"),
        "[restack]\nhistory_limit = 75\n",
    )
    .unwrap();

    let mut cmd = gits_cmd();
    cmd.current_dir(repo_path);
    apply_global_config_env(&mut cmd, global_config_root.path());
    cmd.arg("restack")
        .arg("--history-limit")
        .arg("300")
        .assert()
        .success();

    run_ok("git", &["checkout", "feat"], repo_path);
    let new_feat_oid = repo.head().unwrap().target().unwrap();
    let new_feat_parent = repo
        .find_commit(new_feat_oid)
        .unwrap()
        .parent_id(0)
        .unwrap();

    assert_ne!(new_feat_oid, old_feat_oid);
    assert_eq!(new_feat_parent, rewritten_main_oid);
}

#[test]
fn test_restack_continues_past_tip_patch_id_match() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = Repository::init(repo_path).unwrap();

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    let root_oid = make_commit(&repo, "HEAD", "root.txt", "root", "root", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);

    let a_oid = make_commit(
        &repo,
        "HEAD",
        "a.txt",
        "A",
        "commit A",
        &[&repo.find_commit(root_oid).unwrap()],
    );
    let d_oid = make_commit(
        &repo,
        "HEAD",
        "d.txt",
        "D",
        "commit D",
        &[&repo.find_commit(a_oid).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "feat", &a_oid.to_string()],
        repo_path,
    );
    let _unique_oid = make_commit(
        &repo,
        "HEAD",
        "u.txt",
        "U",
        "unique commit",
        &[&repo.find_commit(a_oid).unwrap()],
    );
    let old_feat_oid = make_commit(
        &repo,
        "HEAD",
        "d.txt",
        "D",
        "commit D",
        &[&repo
            .find_commit(repo.head().unwrap().target().unwrap())
            .unwrap()],
    );

    run_ok("git", &["checkout", "main"], repo_path);
    run_ok(
        "git",
        &["reset", "--hard", &root_oid.to_string()],
        repo_path,
    );
    run_ok("git", &["cherry-pick", &a_oid.to_string()], repo_path);
    run_ok("git", &["cherry-pick", &d_oid.to_string()], repo_path);
    let rewritten_main_oid = repo.head().unwrap().target().unwrap();

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("gits");
    cmd.current_dir(repo_path).arg("restack").assert().success();

    run_ok("git", &["checkout", "feat"], repo_path);
    let new_feat_oid = repo.head().unwrap().target().unwrap();

    assert_ne!(
        new_feat_oid, old_feat_oid,
        "restack should not treat a tip-level patch-id match as proof the branch is integrated"
    );
    assert!(
        repo.graph_descendant_of(new_feat_oid, rewritten_main_oid)
            .unwrap(),
        "restack should still move the branch onto the rewritten target history"
    );
}

#[test]
fn test_restack_patch_id_matching_ignores_colored_git_show_output() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = Repository::init(repo_path).unwrap();

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );
    run_ok("git", &["config", "color.ui", "always"], repo_path);

    let root_oid = make_commit(&repo, "HEAD", "root.txt", "root", "root", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);

    let a_oid = make_commit(
        &repo,
        "HEAD",
        "a.txt",
        "A",
        "commit A",
        &[&repo.find_commit(root_oid).unwrap()],
    );
    let d_oid = make_commit(
        &repo,
        "HEAD",
        "d.txt",
        "D",
        "commit D",
        &[&repo.find_commit(a_oid).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "feat", &a_oid.to_string()],
        repo_path,
    );
    make_commit(
        &repo,
        "HEAD",
        "u.txt",
        "U",
        "unique commit",
        &[&repo.find_commit(a_oid).unwrap()],
    );
    let old_feat_oid = make_commit(
        &repo,
        "HEAD",
        "d.txt",
        "D",
        "commit D",
        &[&repo
            .find_commit(repo.head().unwrap().target().unwrap())
            .unwrap()],
    );

    run_ok("git", &["checkout", "main"], repo_path);
    run_ok(
        "git",
        &["reset", "--hard", &root_oid.to_string()],
        repo_path,
    );
    run_ok("git", &["cherry-pick", &a_oid.to_string()], repo_path);
    run_ok("git", &["cherry-pick", &d_oid.to_string()], repo_path);
    let rewritten_main_oid = repo.head().unwrap().target().unwrap();

    let mut cmd = gits_cmd();
    cmd.current_dir(repo_path).arg("restack").assert().success();

    run_ok("git", &["checkout", "feat"], repo_path);
    let new_feat_oid = repo.head().unwrap().target().unwrap();

    assert_ne!(
        new_feat_oid, old_feat_oid,
        "colored git show output must not disable patch-id-based restack detection"
    );
    assert!(
        repo.graph_descendant_of(new_feat_oid, rewritten_main_oid)
            .unwrap(),
        "patch-id matching should still move the branch onto rewritten history"
    );
}

#[test]
fn test_restack_restricts_patch_id_matches_to_target_private_lineage() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = Repository::init(repo_path).unwrap();

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    let root_oid = make_commit(&repo, "HEAD", "root.txt", "root", "root", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);

    let main_base_oid = make_commit(
        &repo,
        "HEAD",
        "base.txt",
        "base",
        "main base",
        &[&repo.find_commit(root_oid).unwrap()],
    );
    let upstream_patch_oid = make_commit(
        &repo,
        "HEAD",
        "shared.txt",
        "shared",
        "shared upstream",
        &[&repo.find_commit(main_base_oid).unwrap()],
    );

    run_ok("git", &["checkout", "-b", "mobile"], repo_path);
    let old_mobile_oid = make_commit(
        &repo,
        "HEAD",
        "mobile.txt",
        "mobile private v1",
        "mobile private",
        &[&repo.find_commit(upstream_patch_oid).unwrap()],
    );

    run_ok("git", &["checkout", "-b", "mobile-tests"], repo_path);
    let old_mobile_tests_oid = make_commit(
        &repo,
        "HEAD",
        "tests.txt",
        "tests",
        "mobile tests",
        &[&repo.find_commit(old_mobile_oid).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "noise", &main_base_oid.to_string()],
        repo_path,
    );
    let noise_unique_oid = make_commit(
        &repo,
        "HEAD",
        "noise.txt",
        "noise",
        "noise unique",
        &[&repo.find_commit(main_base_oid).unwrap()],
    );
    let noise_patch_oid = make_commit(
        &repo,
        "HEAD",
        "shared.txt",
        "shared",
        "shared upstream",
        &[&repo.find_commit(noise_unique_oid).unwrap()],
    );

    run_ok("git", &["checkout", "mobile"], repo_path);
    std::fs::write(repo_path.join("mobile.txt"), "mobile private v2").unwrap();
    run_ok("git", &["add", "mobile.txt"], repo_path);
    run_ok(
        "git",
        &["commit", "--amend", "-m", "mobile private"],
        repo_path,
    );
    let new_mobile_oid = repo
        .find_reference("refs/heads/mobile")
        .unwrap()
        .target()
        .unwrap();

    let mut cmd = gits_cmd();
    cmd.current_dir(repo_path).arg("restack").assert().success();

    let new_mobile_tests_oid = repo
        .find_reference("refs/heads/mobile-tests")
        .unwrap()
        .target()
        .unwrap();
    let new_mobile_tests_parent = repo
        .find_commit(new_mobile_tests_oid)
        .unwrap()
        .parent_id(0)
        .unwrap();
    assert_ne!(
        new_mobile_tests_oid, old_mobile_tests_oid,
        "restack should still move real children of the rewritten branch"
    );
    assert_eq!(
        new_mobile_tests_parent, new_mobile_oid,
        "floating descendants should rebase onto the rewritten branch tip"
    );

    let noise_head = repo
        .find_reference("refs/heads/noise")
        .unwrap()
        .target()
        .unwrap();
    assert_eq!(
        noise_head, noise_patch_oid,
        "patch-id matches against upstream commits must not restack unrelated branches"
    );
}

#[test]
fn test_restack_matches_rewritten_private_commit_by_patch_id_on_non_root_branch() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = Repository::init(repo_path).unwrap();

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    std::fs::write(repo_path.join("root.txt"), "root").unwrap();
    run_ok("git", &["add", "root.txt"], repo_path);
    run_ok("git", &["commit", "-m", "root"], repo_path);
    run_ok("git", &["branch", "-M", "main"], repo_path);

    std::fs::write(repo_path.join("base.txt"), "base").unwrap();
    run_ok("git", &["add", "base.txt"], repo_path);
    run_ok("git", &["commit", "-m", "main base"], repo_path);

    run_ok("git", &["checkout", "-b", "mobile"], repo_path);
    std::fs::write(repo_path.join("mobile.txt"), "mobile private").unwrap();
    run_ok("git", &["add", "mobile.txt"], repo_path);
    run_ok("git", &["commit", "-m", "mobile private"], repo_path);
    let old_mobile_oid = repo.head().unwrap().target().unwrap();

    run_ok("git", &["checkout", "-b", "mobile-tests"], repo_path);
    std::fs::write(repo_path.join("tests.txt"), "tests").unwrap();
    run_ok("git", &["add", "tests.txt"], repo_path);
    run_ok("git", &["commit", "-m", "mobile tests"], repo_path);
    let old_mobile_tests_oid = repo.head().unwrap().target().unwrap();

    run_ok("git", &["checkout", "main"], repo_path);
    std::fs::write(repo_path.join("extra.txt"), "extra").unwrap();
    run_ok("git", &["add", "extra.txt"], repo_path);
    run_ok("git", &["commit", "-m", "main changed"], repo_path);
    let new_main_oid = repo.head().unwrap().target().unwrap();

    run_ok("git", &["checkout", "mobile"], repo_path);
    run_ok(
        "git",
        &["reset", "--hard", &new_main_oid.to_string()],
        repo_path,
    );
    std::fs::write(repo_path.join("mobile.txt"), "mobile private").unwrap();
    run_ok("git", &["add", "mobile.txt"], repo_path);
    run_ok("git", &["commit", "-m", "mobile private"], repo_path);
    let rewritten_mobile_oid = repo
        .find_reference("refs/heads/mobile")
        .unwrap()
        .target()
        .unwrap();

    let mut cmd = gits_cmd();
    cmd.current_dir(repo_path).arg("restack").assert().success();

    let new_mobile_tests_oid = repo
        .find_reference("refs/heads/mobile-tests")
        .unwrap()
        .target()
        .unwrap();
    let new_mobile_tests_parent = repo
        .find_commit(new_mobile_tests_oid)
        .unwrap()
        .parent_id(0)
        .unwrap();

    assert_ne!(
        repo.find_commit(rewritten_mobile_oid)
            .unwrap()
            .parent_id(0)
            .unwrap(),
        repo.find_commit(old_mobile_oid)
            .unwrap()
            .parent_id(0)
            .unwrap(),
        "the rewritten target commit should only match the old base by patch-id"
    );
    assert_ne!(
        new_mobile_tests_oid, old_mobile_tests_oid,
        "restack should rewrite a child branch when its old base only matches by patch-id"
    );
    assert_eq!(
        new_mobile_tests_parent, rewritten_mobile_oid,
        "patch-id matching should still recognize rewritten private commits on non-root branches"
    );
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
