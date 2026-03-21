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
use kindra::stack::get_stack_branches_from_merge_base;
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
