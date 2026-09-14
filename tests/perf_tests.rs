//! Performance regression tests for stack discovery in large repositories.
//!
//! These tests verify that `get_stack_branches_from_merge_base` is O(stack_depth),
//! not O(repo_history). A 2-branch stack must be discoverable in under 500ms even
//! when the base repo has thousands of commits and many unrelated local branches.
//!
//! The scenario is deliberately realistic:
//!   - A long main history (many commits)
//!   - The stack branches off an older point on main (not the tip)
//!   - Additional commits land on main after the stack diverges
//!   - Several unrelated feature branches diverge from various points on main
//!   - Objects are packed (as they would be in any real repo after `git gc`)
//!
//! With the old O(N·k) algorithm (graph_descendant_of per branch), this takes several
//! seconds because each unrelated branch triggers an exhaustive commit-graph walk.
//! With the O(stack_depth) algorithm, it completes in <20ms.

mod common;
use common::repo_init;
use git2::{Repository, Signature};
use kindra::stack::{
    StackBranch, get_full_stack_branches_for_head, get_stack_branches_from_merge_base,
    plan_tree_sync, resolve_merge_base,
};
use std::time::{Duration, Instant};
use tempfile::tempdir;

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

/// Append `n` empty commits to `refname`, return the final commit OID.
fn append_commits(repo: &Repository, refname: &str, n: u32) -> git2::Oid {
    let sig = Signature::now("perf", "perf@test.com").unwrap();

    let mut parent_oid: Option<git2::Oid> = repo.refname_to_id(refname).ok();

    let mut last = parent_oid.unwrap_or_else(git2::Oid::zero);

    for i in 0..n {
        let tree_id = if let Some(p) = parent_oid {
            repo.find_commit(p).unwrap().tree_id()
        } else {
            repo.treebuilder(None).unwrap().write().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        let parents: Vec<git2::Commit> = parent_oid
            .map(|p| vec![repo.find_commit(p).unwrap()])
            .unwrap_or_default();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();

        last = repo
            .commit(
                Some(refname),
                &sig,
                &sig,
                &format!("commit {i}"),
                &tree,
                &parent_refs,
            )
            .unwrap();
        parent_oid = Some(last);
    }
    last
}

/// Create a branch at `oid` with `extra` commits on top of it; returns the tip OID.
fn branch_with_commits(
    repo: &Repository,
    name: &str,
    base_oid: git2::Oid,
    extra: u32,
) -> git2::Oid {
    let refname = format!("refs/heads/{name}");

    if extra == 0 {
        repo.reference(
            &refname,
            base_oid,
            true,
            &format!("branch_with_commits: create {name} at {base_oid}"),
        )
        .unwrap();
        return base_oid;
    }

    let sig = Signature::now("perf", "perf@test.com").unwrap();
    let base = repo.find_commit(base_oid).unwrap();
    let tree = repo.find_tree(base.tree_id()).unwrap();

    let c1 = repo
        .commit(Some(&refname), &sig, &sig, "branch c1", &tree, &[&base])
        .unwrap();

    let mut tip = c1;
    for i in 1..extra {
        let parent = repo.find_commit(tip).unwrap();
        let tree = repo.find_tree(parent.tree_id()).unwrap();
        tip = repo
            .commit(
                Some(&refname),
                &sig,
                &sig,
                &format!("branch c{}", i + 1),
                &tree,
                &[&parent],
            )
            .unwrap();
    }
    tip
}

/// Create `name` at `base_oid` with `count` commits that each change a file, so
/// the branch has a real diff. The shared `branch_with_commits` reuses its
/// parent's tree, which makes a branch look like it carries no changes at all.
fn branch_with_content_commits(
    repo: &Repository,
    name: &str,
    base_oid: git2::Oid,
    count: u32,
) -> git2::Oid {
    let refname = format!("refs/heads/{name}");
    let sig = Signature::now("perf", "perf@test.com").unwrap();
    let mut tip = base_oid;

    for i in 0..count {
        let parent = repo.find_commit(tip).unwrap();
        let mut builder = repo.treebuilder(Some(&parent.tree().unwrap())).unwrap();
        let blob = repo
            .blob(format!("{name} commit {i}\ncontent line\n").as_bytes())
            .unwrap();
        builder
            .insert(format!("{name}-{i}.txt"), blob, 0o100644)
            .unwrap();
        let tree_id = builder.write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        tip = repo
            .commit(
                Some(&refname),
                &sig,
                &sig,
                &format!("{name}: commit {i}"),
                &tree,
                &[&parent],
            )
            .unwrap();
    }
    tip
}

/// Pack all loose objects (mirrors what `git gc` does for real repos).
fn pack_objects(dir: &std::path::Path) {
    let status = std::process::Command::new("git")
        .args(["repack", "-a", "-d", "--quiet"])
        .current_dir(dir)
        .status()
        .expect("Failed to execute git repack");
    assert!(status.success(), "git repack failed");
}

// ────────────────────────────────────────────────────────────────────────────
// The test
// ────────────────────────────────────────────────────────────────────────────

/// Realistic large-repo scenario:
///
///   main (1000 commits)
///     │
///     └ at commit 900: feature-a (3 commits)
///                        └ feature-b (3 commits)  ← HEAD
///     │
///     └ commits 901–1000 land on main after the stack was created
///     └ 20 "noise" branches diverged from various points in main[900..1000]
///
/// The noise branches are above the stack's merge_base (commit 900), so the old
/// algorithm's first `graph_descendant_of(noise, merge_base)` check would return
/// true and proceed to the expensive O(history) checks.
///
/// With the O(stack_depth) algorithm + TOPOLOGICAL revwalk, only the 6 stack
/// commits are walked for the ancestor_set, and each noise-branch revwalk is
/// bounded by that branch's own few commits (not main's full history).
#[test]
fn stack_discovery_is_proportional_to_stack_size_not_history() {
    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    // ── 1. Build main history (900 commits before stack, 100 after) ──────────
    let _pre_stack_tip = append_commits(&repo, "refs/heads/main", 900);

    // Grab the commit at position 900 — this will be the stack's merge_base.
    let merge_base_oid = repo.refname_to_id("refs/heads/main").unwrap();

    // ── 2. Build the small stack (2 branches, 3 commits each) ─────────────────
    let fa_tip = branch_with_commits(&repo, "feature-a", merge_base_oid, 3);
    let head_id = branch_with_commits(&repo, "feature-b", fa_tip, 3);

    // ── 3. Advance main by 100 more commits (simulates ongoing development) ───
    let upstream_tip = append_commits(&repo, "refs/heads/main", 100);

    // ── 4. 20 noise branches diverged from main[901..1000] ───────────────────
    // Collect those 100 post-stack commits so we can branch from them.
    let post_stack_commits: Vec<git2::Oid> = {
        let mut walk = repo.revwalk().unwrap();
        walk.push(upstream_tip).unwrap();
        walk.hide(merge_base_oid).unwrap();
        walk.collect::<Result<Vec<_>, _>>().unwrap()
    };

    let step = (post_stack_commits.len() / 21).max(1);
    for (i, &base_oid) in post_stack_commits.iter().step_by(step).take(20).enumerate() {
        branch_with_commits(&repo, &format!("noise-{i}"), base_oid, 2);
    }

    // ── 5. Pack objects (mirrors real-world repos) ─────────────────────────────
    pack_objects(dir.path());

    let upstream_id = upstream_tip;
    let merge_base = repo.merge_base(head_id, upstream_id).unwrap();

    // ── 6. Warm-up run (load lazy state: packfile indexes, ODB caches, etc.) ──
    let _ = get_stack_branches_from_merge_base(&repo, merge_base, head_id, upstream_id, "main")
        .unwrap();

    // ── 7. Timed runs ──────────────────────────────────────────────────────────
    const RUNS: u32 = 5;
    let mut total = Duration::ZERO;
    for _ in 0..RUNS {
        let t = Instant::now();
        let _ = get_stack_branches_from_merge_base(&repo, merge_base, head_id, upstream_id, "main")
            .unwrap();
        total += t.elapsed();
    }
    let avg = total / RUNS;

    // ── 8. Correctness assertion ───────────────────────────────────────────────
    let stack = get_stack_branches_from_merge_base(&repo, merge_base, head_id, upstream_id, "main")
        .unwrap();
    let mut names: Vec<&str> = stack.iter().map(|b| b.name.as_str()).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["feature-a", "feature-b"],
        "Only the two stack branches should be discovered; noise branches must be excluded"
    );

    // ── 9. Performance assertion ───────────────────────────────────────────────
    // 500ms is very generous for a 2-branch stack — the algorithm should finish in
    // <20ms on modern hardware. If this regresses to the O(repo_history) algorithm,
    // it will take several seconds and the assertion will catch it.
    assert!(
        avg < Duration::from_millis(500),
        "Stack discovery averaged {avg:?} over {RUNS} runs — expected <500ms.\n\
         This suggests a regression to O(repo_history) instead of O(stack_depth).\n\
         Scenario: 2-branch stack on a 1000-commit main with 20 post-merge-base noise branches."
    );

    eprintln!(
        "✓ Stack discovery: {avg:?} avg over {RUNS} runs \
         (2-branch stack, 1000-commit main, 20 noise branches above merge-base)"
    );
}

/// `kin tree` discovers the whole stack around HEAD rather than just HEAD's own
/// ancestry, so it has to consider every local branch. Doing that one branch at
/// a time costs a revwalk per branch, each of which re-marks upstream's history
/// as uninteresting — on a repo with a few hundred branches that dominates the
/// command. Discovery must instead stay proportional to the private history the
/// branches actually span.
///
///   main (3100 commits)
///     │
///     └ at commit 3000: feature-a (3 commits)
///                         └ feature-b (3 commits)  ← HEAD
///     │
///     └ 400 unrelated branches diverged from points across main
#[test]
fn full_stack_discovery_is_proportional_to_stack_size_not_branch_count() {
    const NOISE_BRANCHES: usize = 400;
    const MAIN_COMMITS: u32 = 3000;

    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let _pre_stack_tip = append_commits(&repo, "refs/heads/main", MAIN_COMMITS);
    let merge_base_oid = repo.refname_to_id("refs/heads/main").unwrap();

    let fa_tip = branch_with_commits(&repo, "feature-a", merge_base_oid, 3);
    let head_id = branch_with_commits(&repo, "feature-b", fa_tip, 3);

    let upstream_tip = append_commits(&repo, "refs/heads/main", 100);

    // Spread the noise branches across main so they fork from many different
    // points rather than sharing one cheap boundary. Skip the stack's own fork
    // point: a noise branch there would build commits identical to feature-a's
    // (same parent, tree and message) and so land on the same OIDs.
    let main_commits: Vec<git2::Oid> = {
        let mut walk = repo.revwalk().unwrap();
        walk.push(upstream_tip).unwrap();
        walk.collect::<Result<Vec<_>, _>>()
            .unwrap()
            .into_iter()
            .filter(|&oid| oid != merge_base_oid)
            .collect()
    };
    let step = (main_commits.len() / (NOISE_BRANCHES + 1)).max(1);
    for (i, &base_oid) in main_commits
        .iter()
        .step_by(step)
        .take(NOISE_BRANCHES)
        .enumerate()
    {
        branch_with_commits(&repo, &format!("noise-{i}"), base_oid, 2);
    }

    pack_objects(dir.path());

    let upstream_id = upstream_tip;

    // Warm-up run (load lazy state: packfile indexes, ODB caches, etc.).
    let _ = get_full_stack_branches_for_head(&repo, head_id, upstream_id, "main").unwrap();

    const RUNS: u32 = 5;
    let mut total = Duration::ZERO;
    for _ in 0..RUNS {
        let t = Instant::now();
        let _ = get_full_stack_branches_for_head(&repo, head_id, upstream_id, "main").unwrap();
        total += t.elapsed();
    }
    let avg = total / RUNS;

    let stack = get_full_stack_branches_for_head(&repo, head_id, upstream_id, "main").unwrap();
    let mut names: Vec<&str> = stack.iter().map(|b| b.name.as_str()).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["feature-a", "feature-b"],
        "Only the two stack branches should be discovered; noise branches must be excluded"
    );

    // A single bounded walk finishes in a few milliseconds here. One walk per
    // branch takes well over a second, which this catches with room to spare for
    // slower machines.
    assert!(
        avg < Duration::from_millis(300),
        "Full stack discovery averaged {avg:?} over {RUNS} runs — expected <300ms.\n\
         This suggests a regression to one history walk per local branch.\n\
         Scenario: 2-branch stack on a {MAIN_COMMITS}-commit main with {NOISE_BRANCHES} unrelated branches."
    );

    eprintln!(
        "✓ Full stack discovery: {avg:?} avg over {RUNS} runs \
         (2-branch stack, {MAIN_COMMITS}-commit main, {NOISE_BRANCHES} unrelated branches)"
    );
}

/// Planning a tree sync asks the same questions about the same local branches
/// once per branch in the stack. Answering them per branch makes the plan cost
/// stack size × local branches graph walks, which on a repo with a few hundred
/// branches dominates `kin sync` before a single rebase starts.
///
///   main (1000 commits)
///     └ a stack of 20 branches
///   plus 400 unrelated local branches
#[test]
fn tree_sync_planning_is_proportional_to_stack_not_local_branch_count() {
    const STACK_BRANCHES: u32 = 20;
    const NOISE_BRANCHES: usize = 400;

    let dir = tempdir().unwrap();
    let repo = repo_init(dir.path());

    let _pre = append_commits(&repo, "refs/heads/main", 1000);
    let fork_point = repo.refname_to_id("refs/heads/main").unwrap();

    // A linear stack, each branch two commits above the one below it.
    let mut tip = fork_point;
    let mut stack = Vec::new();
    for i in 0..STACK_BRANCHES {
        let name = format!("stack-{i}");
        tip = branch_with_content_commits(&repo, &name, tip, 2);
        stack.push(StackBranch { name, id: tip });
    }

    let upstream_tip = append_commits(&repo, "refs/heads/main", 50);

    // Unrelated branches, spread across main so they do not share one boundary.
    let main_commits: Vec<git2::Oid> = {
        let mut walk = repo.revwalk().unwrap();
        walk.push(upstream_tip).unwrap();
        walk.collect::<Result<Vec<_>, _>>()
            .unwrap()
            .into_iter()
            .filter(|&oid| oid != fork_point)
            .collect()
    };
    let step = (main_commits.len() / (NOISE_BRANCHES + 1)).max(1);
    for (i, &base) in main_commits
        .iter()
        .step_by(step)
        .take(NOISE_BRANCHES)
        .enumerate()
    {
        branch_with_commits(&repo, &format!("noise-{i}"), base, 1);
    }

    pack_objects(dir.path());

    let merge_base = resolve_merge_base(&repo, upstream_tip, tip).unwrap();
    let _ = plan_tree_sync(&repo, &stack, "main", merge_base).unwrap();

    const RUNS: u32 = 3;
    let mut total = Duration::ZERO;
    for _ in 0..RUNS {
        let start = Instant::now();
        let _ = plan_tree_sync(&repo, &stack, "main", merge_base).unwrap();
        total += start.elapsed();
    }
    let avg = total / RUNS;

    let plan = plan_tree_sync(&repo, &stack, "main", merge_base).unwrap();
    assert_eq!(
        plan.remaining.len(),
        STACK_BRANCHES as usize,
        "every stack branch should be planned for a rebase, got {:?}",
        plan.remaining
    );

    // Answering the branch-independent questions once leaves this well under a
    // second. Re-asking them per branch takes tens of seconds on a stack and a
    // branch list this size.
    assert!(
        avg < Duration::from_secs(4),
        "Tree sync planning averaged {avg:?} over {RUNS} runs — expected <4s.\n\
         This suggests the per-branch scan over every local branch is back.\n\
         Scenario: {STACK_BRANCHES}-branch stack, 1050-commit main, {NOISE_BRANCHES} unrelated branches."
    );

    eprintln!(
        "✓ Tree sync planning: {avg:?} avg over {RUNS} runs \
         ({STACK_BRANCHES}-branch stack, {NOISE_BRANCHES} unrelated branches)"
    );
}
