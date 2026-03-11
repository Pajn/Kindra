use anyhow::{Result, anyhow};
use git2::{Commit, Oid, Repository};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::process::Stdio;

#[derive(Clone, Debug)]
pub struct StackBranch {
    pub name: String,
    pub id: Oid,
}

#[derive(Clone, Debug)]
pub struct SyncBoundary {
    pub old_base: Option<Oid>,
    pub merged_branches: Vec<String>,
}

pub fn find_sync_boundary(
    repo: &Repository,
    top_branch: &str,
    upstream_name: &str,
) -> Result<SyncBoundary> {
    let top_id = repo.revparse_single(top_branch)?.id();
    let upstream_id = repo.revparse_single(upstream_name)?.id();
    let merge_base = repo.merge_base(top_id, upstream_id)?;

    let first_parent_chain = collect_first_parent_chain(repo, merge_base, top_id)?;
    let cherry_equivalent = cherry_equivalent_commits(repo, upstream_name, top_branch)?;

    let mut prefix_end: isize = -1;

    for (idx, &commit_id) in first_parent_chain.iter().enumerate() {
        let merged_by_graph =
            repo.graph_descendant_of(upstream_id, commit_id)? || upstream_id == commit_id;
        let merged_by_patch = cherry_equivalent.contains(&commit_id);
        if merged_by_graph || merged_by_patch {
            prefix_end = idx as isize;
        }
    }

    // Identify all local branches that are merged.
    let mut merged_branches = Vec::new();
    let local_branches = repo.branches(Some(git2::BranchType::Local))?;

    let upstream_ref_name = repo
        .resolve_reference_from_short_name(upstream_name)
        .ok()
        .and_then(|r| r.name().map(|s| s.to_string()));

    for res in local_branches {
        let (branch, _) = res?;
        let name = match branch.name()? {
            Some(n) => n.to_string(),
            None => continue,
        };
        if name == upstream_name {
            continue;
        }
        let id = match branch.get().target() {
            Some(id) => id,
            None => continue,
        };

        if let Some(ref ref_name) = upstream_ref_name {
            if branch.get().name() == Some(ref_name) {
                continue;
            }
            if let Ok(upstream) = branch.upstream()
                && upstream.get().name() == Some(ref_name)
            {
                continue;
            }
        }

        // Is this branch part of the stack?
        // We define it as an ancestor of top_branch and descendant of merge_base.
        let is_in_stack_lineage = (repo.graph_descendant_of(top_id, id)? || top_id == id)
            && (repo.graph_descendant_of(id, merge_base)? || id == merge_base);

        if !is_in_stack_lineage {
            continue;
        }

        // Is it merged?
        let merged_by_graph = repo.graph_descendant_of(upstream_id, id)? || upstream_id == id;
        let merged_by_patch = cherry_equivalent.contains(&id);

        if merged_by_graph || merged_by_patch {
            merged_branches.push(name);
        }
    }

    let first_unmerged_idx = (prefix_end + 1) as usize;
    if first_unmerged_idx >= first_parent_chain.len() {
        return Ok(SyncBoundary {
            old_base: None,
            merged_branches,
        });
    }

    let first_commit = first_parent_chain[first_unmerged_idx];
    let first = repo.find_commit(first_commit)?;
    if first.parent_count() == 0 {
        return Err(anyhow!(
            "Cannot sync from root commit {} without a parent base.",
            first_commit
        ));
    }

    Ok(SyncBoundary {
        old_base: Some(first.parent_id(0)?),
        merged_branches,
    })
}

fn collect_first_parent_chain(
    repo: &Repository,
    ancestor_exclusive: Oid,
    tip: Oid,
) -> Result<Vec<Oid>> {
    let mut chain = Vec::new();
    let mut current = tip;

    while current != ancestor_exclusive {
        chain.push(current);
        let commit = repo.find_commit(current)?;
        if commit.parent_count() == 0 {
            return Err(anyhow!(
                "Failed to walk first-parent history from {} to merge-base {}.",
                tip,
                ancestor_exclusive
            ));
        }
        current = commit.parent_id(0)?;
    }

    chain.reverse();
    Ok(chain)
}

pub fn cherry_equivalent_commits(
    repo: &Repository,
    upstream_name: &str,
    top_branch: &str,
) -> Result<HashSet<Oid>> {
    let output = Command::new("git")
        .arg("cherry")
        .arg(upstream_name)
        .arg(top_branch)
        .current_dir(repo_root(repo)?)
        .output()?;

    if !output.status.success() {
        return Err(anyhow!(
            "git cherry failed while computing sync boundary for '{}'.",
            top_branch
        ));
    }

    let mut result = HashSet::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        let marker = parts.next().unwrap_or_default();
        if marker != "-" {
            continue;
        }

        if let Some(oid_text) = parts.next()
            && let Ok(oid) = Oid::from_str(oid_text)
        {
            result.insert(oid);
        }
    }

    Ok(result)
}

pub struct FloatingTargetContext {
    candidates: Vec<FloatingTargetCandidate>,
    candidate_ids: HashSet<Oid>,
    patch_ids: HashSet<String>,
    reflog_ids: HashSet<Oid>,
}

#[derive(Clone)]
struct FloatingTargetCandidate {
    id: Oid,
    tree_id: Oid,
    summary: String,
    email: String,
    parent_id: Option<Oid>,
}

pub fn build_floating_target_context(
    repo: &Repository,
    target_commit: &Commit,
    target_branch: &str,
    history_limit: usize,
    patch_id_cache: &mut HashMap<Oid, Option<String>>,
) -> Result<FloatingTargetContext> {
    let mut candidates = Vec::new();
    let mut commit_ids = Vec::new();
    let mut current = Some(target_commit.id());
    let mut remaining = history_limit;

    while let Some(commit_id) = current {
        if history_limit != 0 && remaining == 0 {
            break;
        }
        let commit = repo.find_commit(commit_id)?;
        candidates.push(FloatingTargetCandidate {
            id: commit_id,
            tree_id: commit.tree_id(),
            summary: commit.summary().unwrap_or("").trim().to_string(),
            email: commit.author().email().unwrap_or("").to_string(),
            parent_id: if commit.parent_count() > 0 {
                Some(commit.parent_id(0)?)
            } else {
                None
            },
        });
        commit_ids.push(commit_id);
        current = if commit.parent_count() > 0 {
            Some(commit.parent_id(0)?)
        } else {
            None
        };
        if history_limit != 0 {
            remaining -= 1;
        }
    }

    ensure_patch_ids(repo, &commit_ids, patch_id_cache)?;
    let patch_ids = commit_ids
        .iter()
        .filter_map(|oid| patch_id_cache.get(oid).and_then(|v| v.as_ref()).cloned())
        .collect();
    let reflog_ids = read_branch_reflog_ids(repo, target_branch);

    Ok(FloatingTargetContext {
        candidate_ids: commit_ids.into_iter().collect(),
        candidates,
        patch_ids,
        reflog_ids,
    })
}

pub fn find_floating_base(
    repo: &Repository,
    branch_tip: Oid,
    target: &FloatingTargetContext,
    history_limit: usize,
    patch_id_cache: &mut HashMap<Oid, Option<String>>,
) -> Result<Option<Oid>> {
    let target_id = target
        .candidates
        .first()
        .map(|candidate| candidate.id)
        .ok_or_else(|| {
            anyhow!("Expected at least one target commit for floating-base detection.")
        })?;

    // If the branch is already on top of, already part of, or already integrated
    // into the target branch, it is not a floating child that needs restacking.
    if repo.graph_descendant_of(branch_tip, target_id)? || branch_tip == target_id {
        return Ok(None);
    }
    if repo.graph_descendant_of(target_id, branch_tip)? {
        return Ok(None);
    }

    let mut patch_candidates = Vec::new();
    let mut current = Some(branch_tip);
    let mut remaining = history_limit;

    while let Some(oid) = current {
        if history_limit != 0 && remaining == 0 {
            break;
        }

        // Optimization: If we hit a commit that is reachable from the target, we stop.
        // Because any match found *after* this point would be a common ancestor, not a floating base.
        if repo.graph_descendant_of(target_id, oid)? {
            break;
        }

        let commit = repo.find_commit(oid)?;
        current = if commit.parent_count() > 0 {
            Some(commit.parent_id(0)?)
        } else {
            None
        };
        if history_limit != 0 {
            remaining -= 1;
        }

        if target.candidate_ids.contains(&oid) {
            if oid != branch_tip {
                return Ok(Some(oid));
            }
            continue;
        }

        // Match by tree-hash against rewritten target commits.
        if target
            .candidates
            .iter()
            .any(|candidate| candidate.tree_id == commit.tree_id())
        {
            if oid != branch_tip {
                return Ok(Some(oid));
            }
            continue;
        }

        // Metadata matches narrow the patch-id fallback, but are not sufficient on their own.
        if metadata_matches_target_candidate(&commit, oid, target)? {
            if oid != branch_tip {
                return Ok(Some(oid));
            }
            continue;
        }

        patch_candidates.push(oid);
    }

    ensure_patch_ids(repo, &patch_candidates, patch_id_cache)?;
    for oid in patch_candidates {
        if oid == branch_tip {
            continue;
        }
        if let Some(patch_id) = patch_id_cache.get(&oid).and_then(|v| v.as_ref())
            && target.patch_ids.contains(patch_id)
        {
            return Ok(Some(oid));
        }
    }

    Ok(None)
}

fn metadata_matches_target_candidate(
    commit: &Commit,
    oid: Oid,
    target: &FloatingTargetContext,
) -> Result<bool> {
    if !target.reflog_ids.contains(&oid) {
        return Ok(false);
    }

    let summary = commit.summary().unwrap_or("").trim();
    let author = commit.author();
    let email = author.email().unwrap_or("");
    let parent_id = if commit.parent_count() > 0 {
        Some(commit.parent_id(0)?)
    } else {
        None
    };

    Ok(target.candidates.iter().any(|candidate| {
        candidate.summary == summary && candidate.email == email && candidate.parent_id == parent_id
    }))
}

fn read_branch_reflog_ids(repo: &Repository, branch_name: &str) -> HashSet<Oid> {
    let reflog_name = if branch_name.starts_with("refs/") {
        branch_name.to_string()
    } else {
        format!("refs/heads/{branch_name}")
    };

    let Ok(reflog) = repo.reflog(&reflog_name) else {
        return HashSet::new();
    };

    let mut ids = HashSet::new();
    for index in 0..reflog.len() {
        if let Some(entry) = reflog.get(index) {
            let oid = entry.id_new();
            if !oid.is_zero() {
                ids.insert(oid);
            }
            let oid = entry.id_old();
            if !oid.is_zero() {
                ids.insert(oid);
            }
        }
    }

    ids
}

fn ensure_patch_ids(
    repo: &Repository,
    commit_ids: &[Oid],
    patch_id_cache: &mut HashMap<Oid, Option<String>>,
) -> Result<()> {
    let missing: Vec<Oid> = commit_ids
        .iter()
        .copied()
        .filter(|oid| !patch_id_cache.contains_key(oid))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    let computed = compute_patch_ids(repo, &missing)?;
    for oid in missing {
        patch_id_cache.insert(oid, computed.get(&oid).cloned());
    }

    Ok(())
}

fn compute_patch_ids(repo: &Repository, commit_ids: &[Oid]) -> Result<HashMap<Oid, String>> {
    if commit_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut result = HashMap::new();
    const PATCH_ID_BATCH_SIZE: usize = 512;

    for chunk in commit_ids.chunks(PATCH_ID_BATCH_SIZE) {
        let mut show = Command::new("git");
        show.arg("show").arg("--no-ext-diff").arg("--no-color");
        for oid in chunk {
            show.arg(oid.to_string());
        }

        let mut show_child = show
            .current_dir(repo_root(repo)?)
            .stdout(Stdio::piped())
            .spawn()?;

        let show_stdout = show_child.stdout.take().ok_or_else(|| {
            anyhow!("Failed to capture git show output for patch-id calculation.")
        })?;

        let patch_output = Command::new("git")
            .arg("patch-id")
            .arg("--stable")
            .current_dir(repo_root(repo)?)
            .stdin(Stdio::from(show_stdout))
            .output()?;

        let show_status = show_child.wait()?;
        if !show_status.success() {
            return Err(anyhow!("git show failed while computing patch ids."));
        }
        if !patch_output.status.success() {
            return Err(anyhow!("git patch-id failed while computing patch ids."));
        }

        let stdout = String::from_utf8_lossy(&patch_output.stdout);
        for line in stdout.lines() {
            let mut parts = line.split_whitespace();
            let patch_id = parts.next().unwrap_or_default();
            let commit_id = parts.next().unwrap_or_default();
            if patch_id.is_empty() || commit_id.is_empty() {
                continue;
            }
            if let Ok(oid) = Oid::from_str(commit_id) {
                result.insert(oid, patch_id.to_string());
            }
        }
    }

    Ok(result)
}

fn repo_root(repo: &Repository) -> Result<&Path> {
    if let Some(workdir) = repo.workdir() {
        return Ok(workdir);
    }

    repo.path()
        .parent()
        .ok_or_else(|| anyhow!("Failed to resolve repository root path."))
}

pub fn get_stack_branches(
    repo: &Repository,
    head_id: Oid,
    upstream_id: Oid,
    upstream_name: &str,
) -> Result<Vec<StackBranch>> {
    let mut branches = Vec::new();
    let local_branches = repo.branches(Some(git2::BranchType::Local))?;

    // Find the merge base of HEAD and upstream.
    // Any branch that is a descendant of this merge base and NOT on upstream is part of the stack.
    let merge_base = repo.merge_base(head_id, upstream_id)?;

    for res in local_branches {
        let (branch, _) = res?;
        let name = branch
            .name()?
            .ok_or_else(|| anyhow!("Invalid branch name"))?;
        let id = branch
            .get()
            .target()
            .ok_or_else(|| anyhow!("Branch target not found"))?;

        if name == upstream_name {
            continue;
        }

        if is_stack_member(repo, id, merge_base, upstream_id, head_id)? {
            branches.push(StackBranch {
                name: name.to_string(),
                id,
            });
        }
    }

    Ok(branches)
}

pub fn get_stack_branches_from_merge_base(
    repo: &Repository,
    merge_base: Oid,
    head_id: Oid,
    upstream_id: Oid,
    upstream_name: &str,
) -> Result<Vec<StackBranch>> {
    // Walk from HEAD backward, stopping at upstream. This builds a set of all commits
    // reachable from HEAD but NOT from upstream — the entire "private stack" range.
    // Cost is O(stack_depth), not O(full repo history), making this fast even in huge repos.
    // TOPOLOGICAL sort avoids the timestamp-ordering pitfall where libgit2 would otherwise
    // eagerly process upstream's recent commits before the (potentially older) stack commits.
    let mut ancestor_set = HashSet::new();
    {
        let mut walk = repo.revwalk()?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL)?;
        walk.push(head_id)?;
        walk.hide(upstream_id)?;
        for id_res in walk {
            ancestor_set.insert(id_res?);
        }
    }

    let local_branches = repo.branches(Some(git2::BranchType::Local))?;
    let mut branches = Vec::new();
    let mut candidates_above = Vec::new();

    for res in local_branches {
        let (branch, _) = res?;
        let name = match branch.name()? {
            Some(n) => n.to_string(),
            None => continue,
        };
        if name == upstream_name {
            continue;
        }
        let id = match branch.get().target() {
            Some(id) => id,
            None => continue,
        };

        if ancestor_set.contains(&id) {
            // Tip is in the private stack range (ancestor of HEAD, not merged into upstream).
            branches.push(StackBranch { name, id });
        } else {
            // Could be above HEAD in the stack, or completely unrelated.
            candidates_above.push((name, id));
        }
    }

    // ancestor_set is empty when HEAD is ON upstream (head_id == upstream_id or HEAD is
    // already merged). This is a rare case (e.g., committing directly on main). Fall back
    // to the original per-branch check which is correct for small test repos.
    let head_is_on_upstream = ancestor_set.is_empty();

    // Pre-compute HEAD's commit timestamp for the candidates_above pre-filter below.
    // A branch can only be "above HEAD" (i.e., HEAD reachable from branch_tip) if the
    // branch tip was committed at the same time as or after HEAD. This O(1) check
    // eliminates the expensive per-branch revwalk for the vast majority of noise branches
    // (old feature branches whose tips predate HEAD). Only computed when needed.
    let head_time = if head_is_on_upstream {
        0 // unused in the fallback path
    } else {
        repo.find_commit(head_id)?.time().seconds()
    };

    for (name, id) in candidates_above {
        let in_stack = if head_is_on_upstream {
            is_stack_member(repo, id, merge_base, upstream_id, head_id)?
        } else {
            // Fast pre-filter: a branch committed strictly before HEAD cannot be above it.
            // Loading one commit object is O(1) — far cheaper than creating a revwalk in
            // repos with many pack files (e.g. 825 packs × 25 ms/walk = 2.4 s for 96 noise
            // branches; with this filter, old branches are skipped in ~30 µs each).
            //
            // Fallback: Git timestamps are not strictly monotonic (e.g., clock skew,
            // rebase). If tip_time < head_time, we perform a definitive O(1) graph
            // check via graph_descendant_of to avoid false negatives.
            // We also must ensure the branch is not already merged into upstream.
            let tip_time = repo.find_commit(id)?.time().seconds();
            if tip_time < head_time {
                repo.graph_descendant_of(id, head_id)?
                    && !(repo.graph_descendant_of(upstream_id, id)? || upstream_id == id)
            } else {
                // Walk from this candidate backward (bounded by upstream) and check if
                // head_id appears in its ancestry. If so, the candidate is above HEAD in
                // the stack. TOPOLOGICAL sort ensures we traverse only the candidate's own
                // commits without being side-tracked by upstream's recent history.
                let mut walk = repo.revwalk()?;
                walk.set_sorting(git2::Sort::TOPOLOGICAL)?;
                walk.push(id)?;
                walk.hide(upstream_id)?;
                let mut found = false;
                for commit_res in walk {
                    if commit_res? == head_id {
                        found = true;
                        break;
                    }
                }
                found
            }
        };

        if in_stack {
            branches.push(StackBranch { name, id });
        }
    }

    Ok(branches)
}

fn is_stack_member(
    repo: &Repository,
    id: Oid,
    merge_base: Oid,
    upstream_id: Oid,
    head_id: Oid,
) -> Result<bool> {
    // Is it reachable from the merge base?
    let is_descendant_of_merge_base = repo.graph_descendant_of(id, merge_base)? || id == merge_base;
    if !is_descendant_of_merge_base {
        return Ok(false);
    }

    // AND it must NOT be reachable from upstream (i.e. not yet merged/on main).
    let is_on_upstream = repo.graph_descendant_of(upstream_id, id)? || upstream_id == id;
    if is_on_upstream {
        return Ok(false);
    }

    // AND it must be on the same lineage as HEAD (ancestor or descendant)
    let is_on_head_lineage = repo.graph_descendant_of(id, head_id)?
        || repo.graph_descendant_of(head_id, id)?
        || id == head_id;

    Ok(is_on_head_lineage)
}

pub fn get_immediate_successors(
    repo: &Repository,
    current_id: Oid,
    stack_branches: &[StackBranch],
) -> Result<Vec<String>> {
    let mut successors = Vec::new();

    let mut candidates = Vec::new();
    for b in stack_branches {
        if b.id != current_id
            && (current_id.is_zero() || repo.graph_descendant_of(b.id, current_id)?)
        {
            candidates.push(b);
        }
    }

    for candidate in &candidates {
        let mut is_immediate = true;
        for other in &candidates {
            if other.id != candidate.id && repo.graph_descendant_of(candidate.id, other.id)? {
                is_immediate = false;
                break;
            }
        }

        if is_immediate && !successors.contains(&candidate.name) {
            successors.push(candidate.name.clone());
        }
    }

    Ok(successors)
}

pub fn get_stack_tips(repo: &Repository, stack_branches: &[StackBranch]) -> Result<Vec<String>> {
    let mut tips = Vec::new();

    for branch in stack_branches {
        let mut has_descendant = false;
        for other in stack_branches {
            if other.id != branch.id && repo.graph_descendant_of(other.id, branch.id)? {
                has_descendant = true;
                break;
            }
        }

        if !has_descendant && !tips.contains(&branch.name) {
            tips.push(branch.name.clone());
        }
    }

    Ok(tips)
}

pub fn collect_descendants(
    repo: &Repository,
    root_name: &str,
    all_branches: &[StackBranch],
    result: &mut Vec<StackBranch>,
) -> Result<()> {
    let root = all_branches
        .iter()
        .find(|b| b.name == root_name)
        .ok_or_else(|| {
            anyhow!(
                "Branch '{}' not found in stack. Cannot move the upstream branch itself.",
                root_name
            )
        })?;

    result.push(root.clone());
    collect_descendants_of_id(repo, root.id, all_branches, result)
}

pub fn collect_descendants_of_id(
    repo: &Repository,
    root_id: Oid,
    all_branches: &[StackBranch],
    result: &mut Vec<StackBranch>,
) -> Result<()> {
    for b in all_branches {
        if b.id != root_id
            && repo.graph_descendant_of(b.id, root_id)?
            && !result.iter().any(|existing| existing.name == b.name)
        {
            result.push(b.clone());
        }
    }
    Ok(())
}

pub fn find_parent_in_stack(
    repo: &Repository,
    branch_name: &str,
    all_branches: &[StackBranch],
    merge_base: Oid,
) -> Result<Oid> {
    let branch = all_branches
        .iter()
        .find(|b| b.name == branch_name)
        .ok_or_else(|| anyhow!("Branch '{}' not found in stack.", branch_name))?;

    let mut best_parent = merge_base;
    for b in all_branches {
        if b.name != branch_name
            && (repo.graph_descendant_of(branch.id, b.id)? || branch.id == b.id)
        {
            if b.id == branch.id {
                continue;
            }
            if best_parent == merge_base || repo.graph_descendant_of(b.id, best_parent)? {
                best_parent = b.id;
            }
        }
    }
    Ok(best_parent)
}

fn is_descendant(repo: &Repository, a: Oid, b: Oid) -> Result<bool> {
    repo.graph_descendant_of(a, b).map_err(|e| e.into())
}

pub fn sort_branches_topologically(repo: &Repository, branches: &mut [StackBranch]) -> Result<()> {
    let mut sort_error = None;
    branches.sort_by(|a, b| {
        use std::cmp::Ordering;
        if a.id == b.id {
            return Ordering::Equal;
        }
        let a_desc_b = match is_descendant(repo, a.id, b.id) {
            Ok(v) => v,
            Err(e) => {
                sort_error = Some(e);
                return Ordering::Equal;
            }
        };
        let b_desc_a = match is_descendant(repo, b.id, a.id) {
            Ok(v) => v,
            Err(e) => {
                sort_error = Some(e);
                return Ordering::Equal;
            }
        };
        match (a_desc_b, b_desc_a) {
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            _ => a.name.cmp(&b.name),
        }
    });

    if let Some(e) = sort_error {
        return Err(e);
    }
    Ok(())
}

/// For each branch build a map branch_name → base_branch_name.
/// The base is the closest ancestor stack branch that is NOT merged into upstream,
/// or the repo upstream if all ancestors are merged.
pub fn compute_base_map(
    repo: &Repository,
    branches: &[(StackBranch, String)],
    upstream_name: &str,
) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();

    for (sb, _) in branches {
        let branch_id = sb.id;
        let mut best: Option<&StackBranch> = None;

        for (candidate, _) in branches {
            if candidate.name == sb.name {
                continue;
            }

            // The candidate must be an ancestor of the branch.
            if repo.graph_descendant_of(branch_id, candidate.id)? {
                // We want the "closest" ancestor, i.e., the one that is NOT an ancestor of any other candidate ancestor.
                if let Some(current_best) = best {
                    if repo.graph_descendant_of(candidate.id, current_best.id)? {
                        best = Some(candidate);
                    }
                } else {
                    best = Some(candidate);
                }
            }
        }

        let base = best
            .map(|b| b.name.clone())
            .unwrap_or_else(|| upstream_name.to_string());
        map.insert(sb.name.clone(), base);
    }

    Ok(map)
}

pub fn build_parent_maps(
    repo: &Repository,
    sub_stack: &[StackBranch],
    all_branches_in_stack: &[StackBranch],
    merge_base: Oid,
    head_id: Oid,
    current_branch_name: &str,
) -> Result<(HashMap<String, String>, HashMap<String, String>)> {
    let mut parent_id_map = HashMap::new();
    let mut parent_name_map = HashMap::new();

    for sb in sub_stack {
        let parent_id = find_parent_in_stack(repo, &sb.name, all_branches_in_stack, merge_base)?;
        parent_id_map.insert(sb.name.clone(), parent_id.to_string());

        // Resolve parent_name_map by finding a parent branch in sub_stack with matching id (and different name)
        if let Some(parent_branch) = sub_stack
            .iter()
            .find(|p| p.id == parent_id && p.name != sb.name)
        {
            parent_name_map.insert(sb.name.clone(), parent_branch.name.clone());
        } else if parent_id == head_id {
            // or, if parent_id == head_id, map to current_branch_name
            parent_name_map.insert(sb.name.clone(), current_branch_name.to_string());
        }
    }

    Ok((parent_id_map, parent_name_map))
}

#[derive(Clone)]
pub struct VisualBranch {
    pub name: String,
    pub display_name: String,
}

pub fn collect_path_branches(
    repo: &Repository,
    target_tip_id: Oid,
    merge_base: Oid,
    stack_branches: &[StackBranch],
) -> Result<Vec<StackBranch>> {
    let mut path_branches = Vec::new();
    for b in stack_branches {
        let is_on_path = (repo.graph_descendant_of(target_tip_id, b.id)? || target_tip_id == b.id)
            && (repo.graph_descendant_of(b.id, merge_base)? || b.id == merge_base);
        if is_on_path {
            path_branches.push(b.clone());
        }
    }
    Ok(path_branches)
}

pub fn visualize_stack(
    repo: &Repository,
    all_branches: &[StackBranch],
    current_branch_name: Option<&str>,
) -> Result<Vec<VisualBranch>> {
    let mut result = Vec::new();

    let mut stack_branches = all_branches.to_vec();
    sort_branches_topologically(repo, &mut stack_branches)?;

    for b in stack_branches {
        let is_current = current_branch_name == Some(&b.name);
        let prefix = if is_current { "* " } else { "  " };
        result.push(VisualBranch {
            name: b.name.clone(),
            display_name: format!("{}{}", prefix, b.name),
        });
    }

    Ok(result)
}
