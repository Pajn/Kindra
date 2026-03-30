use git2::Repository;
use kindra::stack::{build_floating_target_context, find_floating_base};
use std::collections::HashMap;
use tempfile::TempDir;

mod common;
use common::{assert_no_rebase_in_progress, kin_cmd, make_commit, repo_init, run_ok};

#[test]
fn test_restack_basic() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = repo_init(repo_path);

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
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kin");
    cmd.current_dir(repo_path).arg("restack").assert().success();

    assert_no_rebase_in_progress(repo_path);

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
    let repo = repo_init(repo_path);

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
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kin");
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
    let repo = repo_init(repo_path);

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
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kin");
    cmd.current_dir(repo_path).arg("restack").assert().success();

    assert_no_rebase_in_progress(repo_path);

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
    let repo = repo_init(repo_path);

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

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kin");
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
    let repo = repo_init(repo_path);

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

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kin");
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
    let repo = repo_init(repo_path);

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

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kin");
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
    let repo = repo_init(repo_path);

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

    let mut cmd = kin_cmd();
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
fn test_find_floating_base_ignores_metadata_only_sibling_with_truncated_target_history() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = repo_init(repo_path);

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
    let rewritten_main_oid = make_commit(
        &repo,
        "HEAD",
        "base.txt",
        "branch two",
        "fixup",
        &[&repo.find_commit(shared_parent_oid).unwrap()],
    );

    let repo = git2::Repository::open(repo_path).unwrap();
    let target_commit = repo.find_commit(rewritten_main_oid).unwrap();
    let mut patch_id_cache = HashMap::new();
    let target =
        build_floating_target_context(&repo, &target_commit, "main", 1, &mut patch_id_cache)
            .unwrap();

    let floating_base =
        find_floating_base(&repo, old_feat_oid, &target, 0, &mut patch_id_cache).unwrap();
    assert_eq!(
        floating_base, None,
        "metadata-only sibling matches must stay ignored when the shared parent is outside the truncated target candidates",
    );
}

#[test]
fn test_find_floating_base_ignores_unrelated_tree_mismatch_below_target_tip() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = repo_init(repo_path);

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    let root_oid = make_commit(&repo, "HEAD", "root.txt", "root", "root", &[]);
    run_ok("git", &["branch", "-M", "main"], repo_path);

    let target_base_oid = make_commit(
        &repo,
        "HEAD",
        "main-base.txt",
        "main base",
        "main base",
        &[&repo.find_commit(root_oid).unwrap()],
    );
    let target_fixup_oid = make_commit(
        &repo,
        "HEAD",
        "main-fixup.txt",
        "main fixup",
        "fixup",
        &[&repo.find_commit(target_base_oid).unwrap()],
    );
    let target_tip_oid = make_commit(
        &repo,
        "HEAD",
        "main-tip.txt",
        "main tip",
        "main tip",
        &[&repo.find_commit(target_fixup_oid).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "side", &root_oid.to_string()],
        repo_path,
    );
    let side_base_oid = make_commit(
        &repo,
        "HEAD",
        "side-base.txt",
        "side base",
        "side base",
        &[&repo.find_commit(root_oid).unwrap()],
    );
    let _side_fixup_oid = make_commit(
        &repo,
        "HEAD",
        "side-fixup.txt",
        "side branch fixup",
        "fixup",
        &[&repo.find_commit(side_base_oid).unwrap()],
    );
    let side_child_oid = make_commit(
        &repo,
        "HEAD",
        "side-child.txt",
        "side child",
        "child",
        &[&repo
            .find_commit(repo.head().unwrap().target().unwrap())
            .unwrap()],
    );

    let repo = git2::Repository::open(repo_path).unwrap();
    let target_commit = repo.find_commit(target_tip_oid).unwrap();
    let mut patch_id_cache = HashMap::new();
    let target =
        build_floating_target_context(&repo, &target_commit, "main", 0, &mut patch_id_cache)
            .unwrap();

    let floating_base =
        find_floating_base(&repo, side_child_oid, &target, 0, &mut patch_id_cache).unwrap();
    assert_eq!(
        floating_base, None,
        "a side-branch fixup that only matches an unrelated lower target commit must not be treated as a floating base",
    );
}

#[test]
fn test_restack_matches_earlier_rewritten_commit_in_target_history() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = repo_init(repo_path);

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

    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kin");
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
    let repo = repo_init(repo_path);

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
            .join("kindra");
    }
    if cfg!(target_os = "windows") {
        return root.join("AppData").join("Roaming").join("kindra");
    }

    root.join(".config").join("kindra")
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

    let mut cmd = kin_cmd();
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

    let mut cmd = kin_cmd();
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
        repo.path().join("kindra.toml"),
        "[restack]\nhistory_limit = 300\n",
    )
    .unwrap();

    let mut cmd = kin_cmd();
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

    let mut cmd = kin_cmd();
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
        repo.path().join("kindra.toml"),
        "[restack]\nhistory_limit = 75\n",
    )
    .unwrap();

    let mut cmd = kin_cmd();
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

/// Negative test: kin restack should NOT rebase branches that share no common
/// history with the target, even if their patch-ids match.
///
/// Scenario:
/// - feat branch has commits cherry-picked from main that happen to produce
///   identical patch-ids
/// - But feat never shared history with main's stack
/// - kin should NOT rebase feat onto main
#[test]
fn test_restack_does_not_rebase_unrelated_history_with_patch_id_match() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = repo_init(repo_path);

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

    // Create feat branch from A with unique commits and cherry-picked equivalent of D
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
    let _feat_tip_oid = make_commit(
        &repo,
        "HEAD",
        "d.txt",
        "D",
        "commit D", // Same message and content as D on main
        &[&repo
            .find_commit(repo.head().unwrap().target().unwrap())
            .unwrap()],
    );

    // Main gets reset and rebuilt via cherry-pick (same content, new commit IDs)
    run_ok("git", &["checkout", "main"], repo_path);
    run_ok(
        "git",
        &["reset", "--hard", &root_oid.to_string()],
        repo_path,
    );
    run_ok("git", &["cherry-pick", &a_oid.to_string()], repo_path);
    run_ok("git", &["cherry-pick", &d_oid.to_string()], repo_path);
    let _rewritten_main_oid = repo.head().unwrap().target().unwrap();

    // Record the original feat tip for comparison
    run_ok("git", &["checkout", "feat"], repo_path);
    let feat_head_before = repo.head().unwrap().target().unwrap();

    // Run kin restack - should succeed but NOT rebase feat
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kin");
    cmd.current_dir(repo_path).arg("restack").assert().success();

    // Verify feat was NOT moved
    run_ok("git", &["checkout", "feat"], repo_path);
    let feat_head_after = repo.head().unwrap().target().unwrap();

    assert_eq!(
        feat_head_before, feat_head_after,
        "kin restack should NOT rebase feat since it shares no common history with main"
    );
}

#[test]
fn test_restack_ignores_isolated_patch_and_tree_matches_on_side_branches() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    repo_init(repo_path);

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    let head_oid = || {
        Repository::open(repo_path)
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap()
    };
    let commit_with_git = |filename: &str, content: &str, message: &str| -> git2::Oid {
        std::fs::write(repo_path.join(filename), content).unwrap();
        run_ok("git", &["add", filename], repo_path);
        run_ok("git", &["commit", "-m", message], repo_path);
        head_oid()
    };
    let commit_with_git_env =
        |filename: &str, content: &str, message: &str, timestamp: &str| -> git2::Oid {
            std::fs::write(repo_path.join(filename), content).unwrap();

            let add = std::process::Command::new("git")
                .args(["add", filename])
                .current_dir(repo_path)
                .env("GIT_AUTHOR_NAME", "Test User")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test User")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .unwrap();
            assert!(
                add.status.success(),
                "git add failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&add.stdout),
                String::from_utf8_lossy(&add.stderr),
            );

            let commit = std::process::Command::new("git")
                .args(["commit", "-m", message])
                .current_dir(repo_path)
                .env("GIT_AUTHOR_NAME", "Test User")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test User")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .env("GIT_AUTHOR_DATE", timestamp)
                .env("GIT_COMMITTER_DATE", timestamp)
                .output()
                .unwrap();
            assert!(
                commit.status.success(),
                "git commit failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&commit.stdout),
                String::from_utf8_lossy(&commit.stderr),
            );

            head_oid()
        };

    let _root_oid = commit_with_git("root.txt", "root", "root");
    run_ok("git", &["branch", "-M", "main"], repo_path);
    let main_base_oid = commit_with_git("main.txt", "main base", "main base");

    run_ok("git", &["checkout", "-b", "target"], repo_path);
    let _old_a_oid = commit_with_git("a.txt", "A1", "target A");
    let old_b_oid = commit_with_git("b.txt", "B1", "target B");

    run_ok("git", &["checkout", "-b", "true-child"], repo_path);
    let true_child_tip_before = commit_with_git("child.txt", "true child", "true child");

    run_ok(
        "git",
        &["checkout", "-b", "noise-patch", &main_base_oid.to_string()],
        repo_path,
    );
    commit_with_git("noise.txt", "noise", "noise root");
    commit_with_git("a.txt", "A1", "target A");
    let noise_patch_tip_before = commit_with_git("noise-tip.txt", "noise tip", "noise patch tip");

    run_ok(
        "git",
        &["checkout", "-b", "noise-tree", &main_base_oid.to_string()],
        repo_path,
    );
    commit_with_git("temp.txt", "temp", "tree root");
    std::fs::remove_file(repo_path.join("temp.txt")).unwrap();
    std::fs::write(repo_path.join("a.txt"), "A1").unwrap();
    std::fs::write(repo_path.join("b.txt"), "B1").unwrap();
    run_ok("git", &["rm", "temp.txt"], repo_path);
    run_ok("git", &["add", "a.txt", "b.txt"], repo_path);
    run_ok("git", &["commit", "-m", "tree match"], repo_path);
    let noise_tree_tip_before = commit_with_git("tree-tip.txt", "tree tip", "noise tree tip");

    run_ok("git", &["checkout", "target"], repo_path);
    run_ok(
        "git",
        &["reset", "--hard", &main_base_oid.to_string()],
        repo_path,
    );
    let _rewritten_a_oid = commit_with_git_env("a.txt", "A1", "target A", "@1000000 +0000");
    let rewritten_target_tip = commit_with_git_env("b.txt", "B1", "target B", "@1000001 +0000");
    assert_ne!(
        rewritten_target_tip, old_b_oid,
        "the rewritten target tip must receive a new commit id"
    );

    let output = kin_cmd()
        .current_dir(repo_path)
        .arg("restack")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "restack failed\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr),
    );
    assert_no_rebase_in_progress(repo_path);

    let repo = Repository::open(repo_path).unwrap();

    let true_child_tip_after = repo
        .find_reference("refs/heads/true-child")
        .unwrap()
        .target()
        .unwrap();
    assert_ne!(
        true_child_tip_after, true_child_tip_before,
        "true-child should rebase onto rewritten target branch"
    );

    let noise_patch_tip_after = repo
        .find_reference("refs/heads/noise-patch")
        .unwrap()
        .target()
        .unwrap();
    assert_eq!(
        noise_patch_tip_after, noise_patch_tip_before,
        "an isolated patch-id match on a side branch must not trigger restack\nstdout:\n{}",
        stdout,
    );

    let noise_tree_tip_after = repo
        .find_reference("refs/heads/noise-tree")
        .unwrap()
        .target()
        .unwrap();
    assert_eq!(
        noise_tree_tip_after, noise_tree_tip_before,
        "an isolated tree match on a side branch must not trigger restack\nstdout:\n{}",
        stdout,
    );
    assert!(
        !stdout.contains("noise-patch"),
        "restack should not report the patch-only side branch\nstdout:\n{}",
        stdout,
    );
    assert!(
        !stdout.contains("noise-tree"),
        "restack should not report the tree-only side branch\nstdout:\n{}",
        stdout,
    );
}

#[test]
fn test_restack_patch_id_matching_ignores_colored_git_show_output() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = repo_init(repo_path);

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

    let mut cmd = kin_cmd();
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
    let repo = repo_init(repo_path);

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

    let mut cmd = kin_cmd();
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
    let repo = repo_init(repo_path);

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

    let mut cmd = kin_cmd();
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
    let repo = repo_init(repo_path);

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
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kin");
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
    let repo = repo_init(repo_path);

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
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kin");
    cmd.current_dir(repo_path).arg("restack").assert().failure();

    // Resolve conflict
    std::fs::write(repo_path.join("conflict.txt"), "Resolved").unwrap();
    run_ok("git", &["add", "conflict.txt"], repo_path);

    // Run kin continue
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kin");
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
    let repo = repo_init(repo_path);

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
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kin");
    cmd.current_dir(repo_path).arg("restack").assert().failure();

    // Run kin abort
    let mut cmd = assert_cmd::cargo::cargo_bin_cmd!("kin");
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

#[test]
fn test_restack_detects_floating_branch_after_upstream_rebase() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = repo_init(repo_path);

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    let root_oid = make_commit(&repo, "HEAD", "root.txt", "root", "root", &[]);
    let shared_base_oid = make_commit(
        &repo,
        "HEAD",
        "shared.txt",
        "shared",
        "shared base",
        &[&repo.find_commit(root_oid).unwrap()],
    );
    run_ok("git", &["branch", "-M", "main"], repo_path);

    run_ok("git", &["checkout", "-b", "pty-alive"], repo_path);
    let pty_commit_1 = make_commit(
        &repo,
        "HEAD",
        "pty1.txt",
        "pty1 content",
        "first pty commit",
        &[&repo.find_commit(shared_base_oid).unwrap()],
    );
    let pty_commit_2 = make_commit(
        &repo,
        "HEAD",
        "pty2.txt",
        "pty2 content",
        "second pty commit",
        &[&repo.find_commit(pty_commit_1).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "cli-tree", &pty_commit_2.to_string()],
        repo_path,
    );
    let cli_unique_1 = make_commit(
        &repo,
        "HEAD",
        "cli.txt",
        "CLI content here",
        "cli unique commit 1",
        &[&repo.find_commit(pty_commit_2).unwrap()],
    );
    let _cli_unique_2 = make_commit(
        &repo,
        "HEAD",
        "cli2.txt",
        "CLI content v2",
        "cli unique commit 2",
        &[&repo.find_commit(cli_unique_1).unwrap()],
    );

    run_ok("git", &["checkout", "main"], repo_path);
    std::fs::write(repo_path.join("shared.txt"), "shared modified\n").unwrap();
    run_ok("git", &["add", "shared.txt"], repo_path);
    run_ok("git", &["commit", "-m", "shared base modified"], repo_path);

    run_ok("git", &["checkout", "pty-alive"], repo_path);
    run_ok("git", &["rebase", "main"], repo_path);

    let repo = git2::Repository::open(repo_path).unwrap();
    let old_pty_commit_1 = repo.find_commit(pty_commit_1).unwrap();
    let old_pty_commit_2 = repo.find_commit(pty_commit_2).unwrap();
    let main_tip = repo.revparse_single("main").unwrap().id();

    run_ok(
        "git",
        &["reset", "--hard", &main_tip.to_string()],
        repo_path,
    );

    let repo = git2::Repository::open(repo_path).unwrap();
    let rewritten_pty_commit_1 = make_commit(
        &repo,
        "HEAD",
        "pty1.txt",
        "pty1 content rewritten",
        "first pty commit",
        &[&repo.find_commit(main_tip).unwrap()],
    );
    let _rewritten_pty_commit_2 = make_commit(
        &repo,
        "HEAD",
        "pty2.txt",
        "pty2 content rewritten",
        "second pty commit",
        &[&repo.find_commit(rewritten_pty_commit_1).unwrap()],
    );

    let repo = git2::Repository::open(repo_path).unwrap();
    let new_pty_tip = repo.revparse_single("pty-alive").unwrap().id();
    let old_cli_tree_tip = repo.revparse_single("cli-tree").unwrap().id();

    let new_pty_commit = repo.find_commit(new_pty_tip).unwrap();
    let new_pty_commit_1 = repo
        .find_commit(new_pty_commit.parent_id(0).unwrap())
        .unwrap();
    assert_eq!(new_pty_commit.summary().unwrap(), "second pty commit");
    assert_eq!(new_pty_commit_1.summary().unwrap(), "first pty commit");
    assert_ne!(
        new_pty_commit_1.tree().unwrap().id(),
        old_pty_commit_1.tree().unwrap().id()
    );
    assert_ne!(
        new_pty_commit.tree().unwrap().id(),
        old_pty_commit_2.tree().unwrap().id()
    );

    let mut cmd = kin_cmd();
    let output = cmd.current_dir(repo_path).arg("restack").output().unwrap();
    assert!(output.status.success());

    assert_no_rebase_in_progress(repo_path);

    let repo = git2::Repository::open(repo_path).unwrap();
    let cli_head = repo.revparse_single("cli-tree").unwrap();
    let cli_commit_2 = repo.find_commit(cli_head.id()).unwrap();
    let cli_commit_1 = repo
        .find_commit(cli_commit_2.parent_id(0).unwrap())
        .unwrap();
    let new_pty_tip_after_restack = repo.revparse_single("pty-alive").unwrap();

    assert_eq!(
        cli_commit_1.parent_id(0).unwrap(),
        new_pty_tip_after_restack.id()
    );

    let cli_tree = cli_commit_2.tree().unwrap();
    assert!(cli_tree.get_name("pty1.txt").is_some());
    assert!(cli_tree.get_name("pty2.txt").is_some());
    assert!(cli_tree.get_name("cli.txt").is_some());
    assert!(cli_tree.get_name("cli2.txt").is_some());

    assert!(cli_head.id() != old_cli_tree_tip);
}

#[test]
fn test_restack_chain_rebase_preserves_stack_relationship() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = repo_init(repo_path);

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    let root_oid = make_commit(&repo, "HEAD", "root.txt", "root", "root", &[]);
    let main_oid = make_commit(
        &repo,
        "HEAD",
        "main.txt",
        "main content",
        "main commit",
        &[&repo.find_commit(root_oid).unwrap()],
    );
    run_ok("git", &["branch", "-M", "main"], repo_path);

    run_ok("git", &["checkout", "-b", "feature-A"], repo_path);
    let commit_a = make_commit(
        &repo,
        "HEAD",
        "a.txt",
        "A content",
        "Add feature A",
        &[&repo.find_commit(main_oid).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "feature-B", &commit_a.to_string()],
        repo_path,
    );
    let commit_b = make_commit(
        &repo,
        "HEAD",
        "b.txt",
        "B content",
        "Add feature B",
        &[&repo.find_commit(commit_a).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "feature-C", &commit_b.to_string()],
        repo_path,
    );
    let commit_c = make_commit(
        &repo,
        "HEAD",
        "c.txt",
        "C content",
        "Add feature C",
        &[&repo.find_commit(commit_b).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "feature-D", &commit_c.to_string()],
        repo_path,
    );
    let _commit_d = make_commit(
        &repo,
        "HEAD",
        "d.txt",
        "D content",
        "Add feature D",
        &[&repo.find_commit(commit_c).unwrap()],
    );

    run_ok("git", &["checkout", "main"], repo_path);
    let main_prime = make_commit(
        &repo,
        "HEAD",
        "main.txt",
        "main content v2",
        "main commit v2",
        &[&repo.find_commit(main_oid).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "stack-A", &main_prime.to_string()],
        repo_path,
    );
    let new_a = make_commit(
        &repo,
        "HEAD",
        "a.txt",
        "A content",
        "Add feature A",
        &[&repo.find_commit(main_prime).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "stack-B", &new_a.to_string()],
        repo_path,
    );
    let new_b = make_commit(
        &repo,
        "HEAD",
        "b.txt",
        "B content",
        "Add feature B",
        &[&repo.find_commit(new_a).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "stack-C", &new_b.to_string()],
        repo_path,
    );
    let new_c = make_commit(
        &repo,
        "HEAD",
        "c.txt",
        "C content",
        "Add feature C",
        &[&repo.find_commit(new_b).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "stack-D", &new_c.to_string()],
        repo_path,
    );
    let _new_d = make_commit(
        &repo,
        "HEAD",
        "d.txt",
        "D content",
        "Add feature D",
        &[&repo.find_commit(new_c).unwrap()],
    );

    let refs_before = std::process::Command::new("git")
        .args(["show-ref", "--heads"])
        .current_dir(repo_path)
        .output()
        .unwrap();
    assert!(
        refs_before.status.success(),
        "git show-ref --heads failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&refs_before.stdout),
        String::from_utf8_lossy(&refs_before.stderr),
    );
    let refs_before_stdout = String::from_utf8_lossy(&refs_before.stdout).into_owned();

    let mut cmd = kin_cmd();
    let output = cmd.current_dir(repo_path).arg("restack").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "restack failed\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr),
    );
    assert_no_rebase_in_progress(repo_path);
    assert!(
        !stdout.contains("matches old base"),
        "restack unexpectedly reported floating children\nstdout:\n{}",
        stdout,
    );

    let refs_after = std::process::Command::new("git")
        .args(["show-ref", "--heads"])
        .current_dir(repo_path)
        .output()
        .unwrap();
    assert!(
        refs_after.status.success(),
        "git show-ref --heads failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&refs_after.stdout),
        String::from_utf8_lossy(&refs_after.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&refs_after.stdout),
        refs_before_stdout,
        "restack should not move any branch tips when no floating children exist",
    );
}

#[test]
fn test_restack_pick_reports_no_candidates_for_repaired_top_stack() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = repo_init(repo_path);

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    let root_oid = make_commit(&repo, "HEAD", "root.txt", "root", "root", &[]);
    let main_oid = make_commit(
        &repo,
        "HEAD",
        "main.txt",
        "main content",
        "main commit",
        &[&repo.find_commit(root_oid).unwrap()],
    );
    run_ok("git", &["branch", "-M", "main"], repo_path);

    run_ok("git", &["checkout", "-b", "feature-A"], repo_path);
    let commit_a = make_commit(
        &repo,
        "HEAD",
        "a.txt",
        "A content",
        "Add feature A",
        &[&repo.find_commit(main_oid).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "feature-B", &commit_a.to_string()],
        repo_path,
    );
    let commit_b = make_commit(
        &repo,
        "HEAD",
        "b.txt",
        "B content",
        "Add feature B",
        &[&repo.find_commit(commit_a).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "feature-C", &commit_b.to_string()],
        repo_path,
    );
    let commit_c = make_commit(
        &repo,
        "HEAD",
        "c.txt",
        "C content",
        "Add feature C",
        &[&repo.find_commit(commit_b).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "feature-D", &commit_c.to_string()],
        repo_path,
    );
    let _commit_d = make_commit(
        &repo,
        "HEAD",
        "d.txt",
        "D content",
        "Add feature D",
        &[&repo.find_commit(commit_c).unwrap()],
    );

    run_ok("git", &["checkout", "main"], repo_path);
    let main_prime = make_commit(
        &repo,
        "HEAD",
        "main.txt",
        "main content v2",
        "main commit v2",
        &[&repo.find_commit(main_oid).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "stack-A", &main_prime.to_string()],
        repo_path,
    );
    let new_a = make_commit(
        &repo,
        "HEAD",
        "a.txt",
        "A content",
        "Add feature A",
        &[&repo.find_commit(main_prime).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "stack-B", &new_a.to_string()],
        repo_path,
    );
    let new_b = make_commit(
        &repo,
        "HEAD",
        "b.txt",
        "B content",
        "Add feature B",
        &[&repo.find_commit(new_a).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "stack-C", &new_b.to_string()],
        repo_path,
    );
    let new_c = make_commit(
        &repo,
        "HEAD",
        "c.txt",
        "C content",
        "Add feature C",
        &[&repo.find_commit(new_b).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "stack-D", &new_c.to_string()],
        repo_path,
    );
    let _new_d = make_commit(
        &repo,
        "HEAD",
        "d.txt",
        "D content",
        "Add feature D",
        &[&repo.find_commit(new_c).unwrap()],
    );

    let output = kin_cmd()
        .current_dir(repo_path)
        .arg("restack")
        .arg("--pick")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "restack --pick failed\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        stdout.contains("No floating children found."),
        "restack --pick should short-circuit before the picker when no candidates exist\nstdout:\n{}",
        stdout,
    );
    assert!(!stdout.contains("Select branches to restack"));
}

#[test]
fn test_restack_preserves_alternate_stack_when_anchor_branch_tip_is_descendant_of_old_base() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    repo_init(repo_path);

    run_ok("git", &["config", "user.name", "Test User"], repo_path);
    run_ok(
        "git",
        &["config", "user.email", "test@example.com"],
        repo_path,
    );

    let commit_with_git = |filename: &str, content: &str, message: &str| -> git2::Oid {
        std::fs::write(repo_path.join(filename), content).unwrap();
        run_ok("git", &["add", filename], repo_path);
        run_ok("git", &["commit", "-m", message], repo_path);
        git2::Repository::open(repo_path)
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap()
    };

    let _root_oid = commit_with_git("root.txt", "root", "root");
    let _main_oid = commit_with_git("main.txt", "main content", "main commit");

    run_ok("git", &["checkout", "-b", "preserved-stack"], repo_path);
    let _old_base_oid = commit_with_git("base.txt", "legacy base", "stack base");
    let preserved_tip_oid =
        commit_with_git("preserved.txt", "preserved ancestor", "preserved ancestor");

    run_ok(
        "git",
        &[
            "checkout",
            "-b",
            "descendant-stack",
            &preserved_tip_oid.to_string(),
        ],
        repo_path,
    );
    let _old_descendant_tip_oid =
        commit_with_git("descendant.txt", "descendant child", "descendant child");

    run_ok("git", &["checkout", "main"], repo_path);
    run_ok("git", &["checkout", "-b", "stack-root"], repo_path);
    let rewritten_base_oid = commit_with_git("base.txt", "legacy base", "stack base rewritten");

    let mut cmd = kin_cmd();
    let output = cmd.current_dir(repo_path).arg("restack").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "restack failed\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr),
    );
    assert_no_rebase_in_progress(repo_path);

    let refs_after = std::process::Command::new("git")
        .args(["show-ref", "--heads"])
        .current_dir(repo_path)
        .output()
        .unwrap();
    let refs_after_stdout = String::from_utf8_lossy(&refs_after.stdout).into_owned();

    let repo = git2::Repository::open(repo_path).unwrap();
    let preserved_after = repo.revparse_single("preserved-stack").unwrap().id();
    let descendant_after = repo.revparse_single("descendant-stack").unwrap().id();

    let preserved_commit = repo.find_commit(preserved_after).unwrap();
    assert_eq!(
        preserved_commit.parent_id(0).unwrap(),
        rewritten_base_oid,
        "preserved-stack should rebase onto stack-root\nstdout:\n{}\nrefs:\n{}",
        stdout,
        refs_after_stdout,
    );

    let descendant_commit = repo.find_commit(descendant_after).unwrap();
    assert_eq!(
        descendant_commit.parent_id(0).unwrap(),
        preserved_after,
        "descendant-stack should remain a child of preserved-stack\nstdout:\n{}\nrefs:\n{}",
        stdout,
        refs_after_stdout,
    );
    assert!(
        stdout.contains("matches old base"),
        "expected restack to detect and rebase the alternate stack\nstdout:\n{}",
        stdout,
    );
}

#[test]
fn test_restack_rebases_multiple_commits_after_single_fork_match() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path();
    let repo = repo_init(repo_path);

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
        "base v1",
        "base commit",
        &[&repo.find_commit(root_oid).unwrap()],
    );

    run_ok(
        "git",
        &["checkout", "-b", "feature", &old_base_oid.to_string()],
        repo_path,
    );
    let old_middle_oid = make_commit(
        &repo,
        "HEAD",
        "middle.txt",
        "middle",
        "middle commit",
        &[&repo.find_commit(old_base_oid).unwrap()],
    );
    let old_tip_oid = make_commit(
        &repo,
        "HEAD",
        "tip.txt",
        "tip",
        "tip commit",
        &[&repo.find_commit(old_middle_oid).unwrap()],
    );

    run_ok("git", &["checkout", "main"], repo_path);
    std::fs::write(repo_path.join("base.txt"), "base v2").unwrap();
    run_ok("git", &["add", "base.txt"], repo_path);
    run_ok(
        "git",
        &["commit", "--amend", "-m", "base commit"],
        repo_path,
    );

    let new_main_oid = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap();
    assert_ne!(new_main_oid, old_base_oid);

    let output = kin_cmd()
        .current_dir(repo_path)
        .arg("restack")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "restack failed\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr),
    );
    assert_no_rebase_in_progress(repo_path);
    assert!(
        stdout.contains("feature"),
        "restack should detect the floating child branch\nstdout:\n{}",
        stdout,
    );

    run_ok("git", &["checkout", "feature"], repo_path);
    let new_tip_oid = repo.head().unwrap().target().unwrap();
    assert_ne!(new_tip_oid, old_tip_oid);

    let new_tip = repo.find_commit(new_tip_oid).unwrap();
    assert_eq!(new_tip.summary().unwrap(), "tip commit");
    let new_middle = new_tip.parent(0).unwrap();
    assert_eq!(new_middle.summary().unwrap(), "middle commit");
    let new_base = new_middle.parent(0).unwrap();
    assert_eq!(new_base.id(), new_main_oid);
    assert_eq!(new_base.summary().unwrap(), "base commit");
}
