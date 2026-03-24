use anyhow::{Result, anyhow};
use git2::{Commit, Oid, Repository};
use std::collections::{HashMap, HashSet, VecDeque};
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

#[derive(Clone, Debug)]
pub struct ReorderPlan {
    pub ordered_sub_stack: Vec<StackBranch>,
    pub remaining_branches: Vec<String>,
    pub new_base_map: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct GraphReorderPlan {
    pub remaining_branches: Vec<String>,
    pub parent_id_map: HashMap<String, String>,
    pub new_base_map: HashMap<String, String>,
}

pub fn find_sync_boundary(
    repo: &Repository,
    top_branch: &str,
    upstream_name: &str,
    stack_branches: &[StackBranch],
) -> Result<SyncBoundary> {
    let top_id = repo.revparse_single(top_branch)?.id();
    let upstream_id = repo.revparse_single(upstream_name)?.id();
    let merge_base = resolve_merge_base(repo, top_id, upstream_id)?;
    let lineage = ordered_stack_lineage(repo, top_id, stack_branches)?;

    let mut merged_branches = HashSet::new();
    let mut branch_cutoff = merge_base;
    for branch in lineage.iter().take(lineage.len().saturating_sub(1)) {
        if !branch_segment_integrated(repo, branch_cutoff, branch.id, upstream_id)? {
            break;
        }

        merged_branches.insert(branch.name.clone());
        branch_cutoff = branch.id;
    }

    let first_parent_chain = collect_first_parent_chain(repo, branch_cutoff, top_id)?;
    let mut prefix_end: isize = -1;

    for (idx, &commit_id) in first_parent_chain.iter().enumerate() {
        let merged_by_graph =
            repo.graph_descendant_of(upstream_id, commit_id)? || upstream_id == commit_id;
        let merged_by_content =
            range_changes_present_in_target(repo, branch_cutoff, commit_id, upstream_id)?;
        if merged_by_graph || merged_by_content {
            prefix_end = idx as isize;
        }
    }

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

        let merged_by_content = merged_branches.contains(&name);
        let merged_by_graph = repo.graph_descendant_of(upstream_id, id)? || upstream_id == id;

        if merged_by_graph || merged_by_content {
            merged_branches.insert(name);
        }
    }

    let first_unmerged_idx = (prefix_end + 1) as usize;
    if first_unmerged_idx >= first_parent_chain.len() {
        let mut merged_branches = merged_branches.into_iter().collect::<Vec<_>>();
        merged_branches.sort();
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

    let mut merged_branches = merged_branches.into_iter().collect::<Vec<_>>();
    merged_branches.sort();

    Ok(SyncBoundary {
        old_base: Some(first.parent_id(0)?),
        merged_branches,
    })
}

pub fn plan_descendant_reorder(
    repo: &Repository,
    current_branch_name: &str,
    target_branch_name: &str,
    all_branches_in_stack: &[StackBranch],
    merge_base: Oid,
    upstream_name: &str,
) -> Result<Option<ReorderPlan>> {
    if current_branch_name == target_branch_name {
        return Ok(None);
    }

    let mut sub_stack = Vec::new();
    collect_descendants(
        repo,
        current_branch_name,
        all_branches_in_stack,
        &mut sub_stack,
    )?;

    if !sub_stack
        .iter()
        .any(|branch| branch.name == target_branch_name)
    {
        return Ok(None);
    }

    let tips = get_stack_tips(repo, &sub_stack)?;
    if tips.len() > 1 {
        return Err(anyhow!(
            "Cannot reorder '{}' onto '{}' because the affected subtree is forked. Same-stack reordering only supports a single linear path.",
            current_branch_name,
            target_branch_name
        ));
    }

    if !branches_are_linearly_ordered(repo, &sub_stack)? {
        return Err(anyhow!(
            "Cannot reorder '{}' onto '{}' because the affected subtree is forked. Same-stack reordering only supports a single linear path.",
            current_branch_name,
            target_branch_name
        ));
    }

    sort_branches_topologically(repo, &mut sub_stack)?;
    let target_index = sub_stack
        .iter()
        .position(|branch| branch.name == target_branch_name)
        .ok_or_else(|| {
            anyhow!(
                "Target branch '{}' not found in subtree.",
                target_branch_name
            )
        })?;

    let reordered = sub_stack[target_index..]
        .iter()
        .chain(sub_stack[..target_index].iter())
        .cloned()
        .collect::<Vec<_>>();

    let current_parent_id =
        find_parent_in_stack(repo, current_branch_name, all_branches_in_stack, merge_base)?;
    let current_parent = if current_parent_id == merge_base {
        upstream_name.to_string()
    } else {
        parent_base_spec(
            current_parent_id,
            current_branch_name,
            all_branches_in_stack,
        )
    };

    let mut remaining_branches = Vec::with_capacity(reordered.len());
    let mut new_base_map = HashMap::new();
    for (idx, branch) in reordered.iter().enumerate() {
        remaining_branches.push(branch.name.clone());
        let new_base = if idx == 0 {
            current_parent.clone()
        } else {
            reordered[idx - 1].name.clone()
        };
        new_base_map.insert(branch.name.clone(), new_base);
    }

    Ok(Some(ReorderPlan {
        ordered_sub_stack: sub_stack,
        remaining_branches,
        new_base_map,
    }))
}

fn branches_are_linearly_ordered(repo: &Repository, branches: &[StackBranch]) -> Result<bool> {
    for (idx, branch) in branches.iter().enumerate() {
        for other in branches.iter().skip(idx + 1) {
            let comparable = repo.graph_descendant_of(branch.id, other.id)?
                || repo.graph_descendant_of(other.id, branch.id)?
                || branch.id == other.id;
            if !comparable {
                return Ok(false);
            }
        }
    }

    Ok(true)
}
pub fn collect_stack_component(
    repo: &Repository,
    current_branch_name: &str,
    merge_base: Oid,
    upstream_id: Oid,
    upstream_name: &str,
) -> Result<Vec<StackBranch>> {
    let local_branches = repo.branches(Some(git2::BranchType::Local))?;
    let mut candidates = Vec::new();

    for branch_result in local_branches {
        let (branch, _) = branch_result?;
        let Some(name) = branch.name()? else {
            continue;
        };
        if name == upstream_name {
            continue;
        }

        let Some(id) = branch.get().target() else {
            continue;
        };

        let is_descendant_of_merge_base =
            repo.graph_descendant_of(id, merge_base)? || id == merge_base;
        let is_on_upstream = repo.graph_descendant_of(upstream_id, id)? || upstream_id == id;
        if is_descendant_of_merge_base && !is_on_upstream {
            candidates.push(StackBranch {
                name: name.to_string(),
                id,
            });
        }
    }

    if !candidates
        .iter()
        .any(|branch| branch.name == current_branch_name)
    {
        return Err(anyhow!(
            "Branch '{}' not found in the current stack component.",
            current_branch_name
        ));
    }

    let mut adjacency: HashMap<String, Vec<String>> = candidates
        .iter()
        .map(|branch| (branch.name.clone(), Vec::new()))
        .collect();

    for branch in &candidates {
        let parent_id = find_parent_in_stack(repo, &branch.name, &candidates, merge_base)?;
        if let Some(parent_branch) = candidates
            .iter()
            .find(|candidate| candidate.id == parent_id && candidate.name != branch.name)
        {
            adjacency
                .get_mut(&branch.name)
                .expect("branch adjacency entry must exist")
                .push(parent_branch.name.clone());
            adjacency
                .get_mut(&parent_branch.name)
                .expect("parent adjacency entry must exist")
                .push(branch.name.clone());
        }
    }

    let mut queue = VecDeque::from([current_branch_name.to_string()]);
    let mut visited = HashSet::new();
    while let Some(branch_name) = queue.pop_front() {
        if !visited.insert(branch_name.clone()) {
            continue;
        }
        if let Some(neighbors) = adjacency.get(&branch_name) {
            for neighbor in neighbors {
                if !visited.contains(neighbor) {
                    queue.push_back(neighbor.clone());
                }
            }
        }
    }

    let mut component = candidates
        .into_iter()
        .filter(|branch| visited.contains(&branch.name))
        .collect::<Vec<_>>();
    sort_branches_topologically(repo, &mut component)?;
    Ok(component)
}

pub fn current_parent_name_map(
    repo: &Repository,
    branches: &[StackBranch],
    merge_base: Oid,
    upstream_name: &str,
) -> Result<HashMap<String, String>> {
    let mut parent_map = HashMap::new();

    for branch in branches {
        let parent_id = find_parent_in_stack(repo, &branch.name, branches, merge_base)?;
        let parent_name = if parent_id == merge_base {
            upstream_name.to_string()
        } else {
            branches
                .iter()
                .find(|candidate| candidate.id == parent_id && candidate.name != branch.name)
                .map(|candidate| candidate.name.clone())
                .ok_or_else(|| anyhow!("Failed to resolve parent branch for '{}'.", branch.name))?
        };
        parent_map.insert(branch.name.clone(), parent_name);
    }

    Ok(parent_map)
}

pub fn plan_graph_reorder(
    repo: &Repository,
    branches: &[StackBranch],
    merge_base: Oid,
    upstream_name: &str,
    edited_parent_map: &HashMap<String, String>,
) -> Result<GraphReorderPlan> {
    let expected_names = branches
        .iter()
        .map(|branch| branch.name.clone())
        .collect::<HashSet<_>>();

    if edited_parent_map.len() != branches.len() {
        return Err(anyhow!(
            "Edited branch graph is incomplete. Every branch must appear exactly once."
        ));
    }

    for branch in branches {
        let Some(parent_name) = edited_parent_map.get(&branch.name) else {
            return Err(anyhow!(
                "Branch '{}' is missing from the edited graph.",
                branch.name
            ));
        };

        if parent_name == &branch.name {
            return Err(anyhow!(
                "Branch '{}' cannot list itself as its parent.",
                branch.name
            ));
        }

        if parent_name != upstream_name && !expected_names.contains(parent_name) {
            return Err(anyhow!(
                "Branch '{}' has unknown parent '{}'.",
                branch.name,
                parent_name
            ));
        }
    }

    let order_hint = branches
        .iter()
        .enumerate()
        .map(|(idx, branch)| (branch.name.clone(), idx))
        .collect::<HashMap<_, _>>();
    let remaining_branches =
        topologically_sort_edited_graph(edited_parent_map, upstream_name, &order_hint)?;

    let mut parent_id_map = HashMap::new();
    let mut new_base_map = HashMap::new();
    for branch in branches {
        let parent_id = find_parent_in_stack(repo, &branch.name, branches, merge_base)?;
        parent_id_map.insert(branch.name.clone(), parent_id.to_string());
        new_base_map.insert(
            branch.name.clone(),
            edited_parent_map
                .get(&branch.name)
                .expect("edited parent map already validated")
                .clone(),
        );
    }

    Ok(GraphReorderPlan {
        remaining_branches,
        parent_id_map,
        new_base_map,
    })
}

fn topologically_sort_edited_graph(
    edited_parent_map: &HashMap<String, String>,
    upstream_name: &str,
    order_hint: &HashMap<String, usize>,
) -> Result<Vec<String>> {
    let mut indegree = edited_parent_map
        .keys()
        .map(|branch| (branch.clone(), 0usize))
        .collect::<HashMap<_, _>>();
    let mut children = edited_parent_map
        .keys()
        .map(|branch| (branch.clone(), Vec::new()))
        .collect::<HashMap<_, Vec<String>>>();

    for (branch, parent) in edited_parent_map {
        if parent == upstream_name {
            continue;
        }
        *indegree
            .get_mut(branch)
            .expect("indegree entry must exist for branch") += 1;
        children
            .get_mut(parent)
            .expect("child list must exist for parent branch")
            .push(branch.clone());
    }

    let mut ready = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(branch, _)| branch.clone())
        .collect::<Vec<_>>();
    ready.sort_by_key(|branch| order_hint.get(branch).copied().unwrap_or(usize::MAX));
    let mut ready = VecDeque::from(ready);

    let mut sorted = Vec::with_capacity(edited_parent_map.len());
    while let Some(branch) = ready.pop_front() {
        sorted.push(branch.clone());

        if let Some(child_names) = children.get(&branch) {
            for child in child_names {
                let degree = indegree
                    .get_mut(child)
                    .expect("indegree entry must exist for child branch");
                *degree -= 1;
                if *degree == 0 {
                    ready.push_back(child.clone());
                }
            }
            let mut ready_sorted = ready.into_iter().collect::<Vec<_>>();
            ready_sorted
                .sort_by_key(|candidate| order_hint.get(candidate).copied().unwrap_or(usize::MAX));
            ready = VecDeque::from(ready_sorted);
        }
    }

    if sorted.len() != edited_parent_map.len() {
        return Err(anyhow!(
            "Edited branch graph contains a cycle. Every branch must eventually trace back to '{}'.",
            upstream_name
        ));
    }

    Ok(sorted)
}

pub fn collect_first_parent_chain(
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

pub fn collect_merged_local_branches(
    repo: &Repository,
    target_ref_name: &str,
    protected_branches: &[&str],
) -> Result<Vec<String>> {
    let target_id = repo.revparse_single(target_ref_name)?.id();
    let full_target_ref_name = repo
        .resolve_reference_from_short_name(target_ref_name)
        .ok()
        .and_then(|reference| reference.name().map(|name| name.to_string()));
    let protected_branches = protected_branches
        .iter()
        .map(|name| name.to_string())
        .collect::<HashSet<_>>();

    let mut branches = Vec::new();
    for branch_result in repo.branches(Some(git2::BranchType::Local))? {
        let (branch, _) = branch_result?;
        let Some(name) = branch.name()? else {
            continue;
        };
        if protected_branches.contains(name) {
            continue;
        }

        if let Some(ref full_target_ref_name) = full_target_ref_name {
            if branch.get().name() == Some(full_target_ref_name.as_str()) {
                continue;
            }

            if let Ok(upstream_branch) = branch.upstream()
                && upstream_branch.get().name() == Some(full_target_ref_name.as_str())
            {
                continue;
            }
        }

        let Some(branch_id) = branch.get().target() else {
            continue;
        };
        branches.push((name.to_string(), branch_id));
    }

    let mut merged_branches = Vec::new();
    for (name, branch_id) in branches {
        let merged_by_graph =
            target_id == branch_id || repo.graph_descendant_of(target_id, branch_id)?;
        let merged_by_content = if merged_by_graph {
            false
        } else if let Ok(merge_base) = repo.merge_base(branch_id, target_id) {
            range_changes_present_in_target(repo, merge_base, branch_id, target_id)?
        } else {
            false
        };

        if merged_by_graph || merged_by_content {
            merged_branches.push(name);
        }
    }

    merged_branches.sort();
    Ok(merged_branches)
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

fn floating_patch_id_boundary(
    repo: &Repository,
    target_id: Oid,
    target_branch: &str,
) -> Result<Option<Oid>> {
    let upstream_name = match crate::commands::find_upstream(repo)? {
        Some(name) => name,
        None => return Ok(None),
    };

    if target_branch == upstream_name {
        return Ok(None);
    }

    let upstream_id = repo.revparse_single(&upstream_name)?.id();
    let merge_base = repo.merge_base(target_id, upstream_id)?;
    let stack_branches = get_stack_branches(repo, target_id, upstream_id, &upstream_name)?;

    if !stack_branches
        .iter()
        .any(|branch| branch.name == target_branch)
    {
        return Ok(Some(merge_base));
    }

    Ok(Some(find_parent_in_stack(
        repo,
        target_branch,
        &stack_branches,
        merge_base,
    )?))
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

    let patch_id_boundary = floating_patch_id_boundary(repo, target_commit.id(), target_branch)?;
    let patch_commit_ids: Vec<Oid> = match patch_id_boundary {
        Some(boundary) => commit_ids
            .iter()
            .copied()
            .take_while(|oid| *oid != boundary)
            .collect(),
        None => commit_ids.clone(),
    };

    // Patch-id fallback should only compare against the target branch's private lineage.
    // Matching against upstream commits causes unrelated branches with cherry-picked
    // equivalents to look like floating children.
    ensure_patch_ids(repo, &patch_commit_ids, patch_id_cache)?;
    let patch_ids = patch_commit_ids
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
        let parent_id = first_parent_id(&commit)?;
        current = parent_id;
        if history_limit != 0 {
            remaining -= 1;
        }

        if target.candidate_ids.contains(&oid) {
            if oid != branch_tip {
                return Ok(Some(oid));
            }
            continue;
        }

        // Match by tree-hash against rewritten target commits, but only if the
        // surrounding parent commit also lines up with the rewritten lineage.
        let mut has_tree_match = false;
        for candidate in &target.candidates {
            if candidate.tree_id != commit.tree_id() {
                continue;
            }
            if floating_parent_matches_candidate(
                repo,
                parent_id,
                candidate.parent_id,
                patch_id_cache,
            )? {
                has_tree_match = true;
                break;
            }
        }
        if has_tree_match && oid != branch_tip {
            return Ok(Some(oid));
        }

        // Metadata matches narrow the patch-id fallback, but are not sufficient on their own.
        if metadata_matches_target_candidate(&commit, oid, target)? {
            if oid != branch_tip {
                return Ok(Some(oid));
            }
            continue;
        }

        // Check if metadata matches a target commit but trees differ.
        // This indicates the target was modified (e.g., fixup during rebase)
        // and this commit is the original version - fork point found.
        // This handles the case where:
        // - cli-tree has: main -> old_pty -> old_cli
        // - pty-alive was rebased to: main -> new_pty (modified) -> ...
        // - old_pty and new_pty have same metadata but different trees
        // - old_pty is NOT in candidate_ids (it's the old version)
        //
        // BUT we must verify there's an actual rebase relationship.
        // In the embers case, old_pty was rebased to create new_pty, so:
        //   - old_pty.parent IS in pty-alive's history
        //   - new_pty.parent (which is old_pty) is NOT in pty-alive's history
        //
        // In the sibling case (old_base and rewritten_main):
        //   - Both share the same parent (shared_parent) which IS in main's history
        //   - Neither is a rebased version of the other
        //
        // So we check: if the corresponding target commit's parent is in target history,
        // they are siblings, not a rebase pair.
        if oid != branch_tip {
            let summary = commit.summary().unwrap_or("").trim().to_string();
            let author = commit.author();
            let email = author.email().unwrap_or("").to_string();

            // Look for a target commit with matching summary, author, email but DIFFERENT tree.
            // We don't require parent_id to match because after a rebase, the parent changes.
            // We check parent ancestry to distinguish rebase from sibling:
            // - Rebase: OLD.parent is NOT an ancestor of ANY candidate in the target chain
            //   (because OLD.parent was rebased and is now "orphaned")
            // - Sibling: OLD.parent IS an ancestor of some candidate in target chain
            //   (because OLD.parent is on the main lineage which target is built on)
            // Tree-mismatch matching is only reliable against the current target tip.
            // Matching lower-history commits by summary/email is too ambiguous and can
            // falsely classify unrelated side branches as floating.
            let corresponding_target = target.candidates.iter().find(|c| {
                c.id == target_id
                    && c.summary == summary
                    && c.email == email
                    && c.tree_id != commit.tree_id()
            });

            if let Some(_target_candidate) = corresponding_target {
                // Check if OLD.parent is an ancestor of ANY candidate in target chain.
                // In rebase: old_pty_1 is not an ancestor of any new pty candidate (orphaned).
                // In sibling: A is an ancestor of main' (through main), so it IS an ancestor of candidates.
                let old_parent_ancestor_of_any_candidate = parent_id
                    .map(|pid| {
                        target
                            .candidate_ids
                            .iter()
                            .any(|cid| repo.graph_descendant_of(*cid, pid).unwrap_or(false))
                    })
                    .unwrap_or(false);

                if !old_parent_ancestor_of_any_candidate {
                    // OLD.parent is NOT an ancestor of any candidate - it's orphaned, rebase fork
                    return Ok(Some(oid));
                }
            }
        }

        patch_candidates.push(oid);
    }

    ensure_patch_ids(repo, &patch_candidates, patch_id_cache)?;
    for oid in patch_candidates {
        if oid == branch_tip {
            continue;
        }
        let Some(patch_id) = patch_id_cache.get(&oid).and_then(|v| v.as_ref()).cloned() else {
            continue;
        };
        if !target.patch_ids.contains(&patch_id) {
            continue;
        }

        let commit = repo.find_commit(oid)?;
        let parent_id = first_parent_id(&commit)?;
        let mut has_patch_match = false;
        for candidate in &target.candidates {
            let Some(candidate_patch_id) =
                patch_id_cache.get(&candidate.id).and_then(|v| v.as_ref())
            else {
                continue;
            };
            if candidate_patch_id != &patch_id {
                continue;
            }
            if floating_parent_matches_candidate(
                repo,
                parent_id,
                candidate.parent_id,
                patch_id_cache,
            )? || is_tip_patch_rewrite(&commit, candidate, target_id)
            {
                has_patch_match = true;
                break;
            }
        }
        if has_patch_match {
            return Ok(Some(oid));
        }
    }

    Ok(None)
}

/// Returns the first parent's OID if the commit has at least one parent.
///
/// Returns `Ok(None)` for root commits (commits with no parents).
/// Propagates errors from `parent_id()` via the `?` operator.
fn first_parent_id(commit: &Commit) -> Result<Option<Oid>> {
    if commit.parent_count() > 0 {
        Ok(Some(commit.parent_id(0)?))
    } else {
        Ok(None)
    }
}

/// Checks whether the floating branch's parent commit aligns with a target
/// candidate's parent.
///
/// Returns `true` if parents match by OID, tree-id, or patch-id—indicating
/// the commits share the same logical lineage despite potential rewrites.
///
/// Comparison strategy:
/// 1. OID equality (fastest)
/// 2. Tree-ID equality (fallback when commit was amended but patch is intact)
/// 3. Patch-ID equality via `patch_id_cache` (handles content-preserving rewrites)
///
/// When both `branch_parent_id` and `candidate_parent_id` are `None` (both
/// root commits), returns `true`.
///
/// Calls `ensure_patch_ids` to populate the cache before patch comparison.
/// Returns `Result<bool>` due to repository lookup errors (`find_commit`).
fn floating_parent_matches_candidate(
    repo: &Repository,
    branch_parent_id: Option<Oid>,
    candidate_parent_id: Option<Oid>,
    patch_id_cache: &mut HashMap<Oid, Option<String>>,
) -> Result<bool> {
    let (Some(branch_parent_id), Some(candidate_parent_id)) =
        (branch_parent_id, candidate_parent_id)
    else {
        return Ok(branch_parent_id.is_none() && candidate_parent_id.is_none());
    };

    if branch_parent_id == candidate_parent_id {
        return Ok(true);
    }

    if repo.find_commit(branch_parent_id)?.tree_id()
        == repo.find_commit(candidate_parent_id)?.tree_id()
    {
        return Ok(true);
    }

    ensure_patch_ids(
        repo,
        &[branch_parent_id, candidate_parent_id],
        patch_id_cache,
    )?;
    let branch_patch_id = patch_id_cache
        .get(&branch_parent_id)
        .and_then(|value| value.as_ref());
    let candidate_patch_id = patch_id_cache
        .get(&candidate_parent_id)
        .and_then(|value| value.as_ref());

    Ok(branch_patch_id.is_some() && branch_patch_id == candidate_patch_id)
}

/// Detects the special case where a floating commit's patch-id matches the
/// target tip but the trees differ—indicating the target tip was modified
/// (e.g., via fixup/squash) and the floating commit is the original version.
///
/// Requires matching commit summary and author email for safety to avoid
/// false positives when different commits happen to have the same patch-id.
///
/// Returns `true` when all conditions hold:
/// - `candidate.id == target_id` (same patch-id as target)
/// - `candidate.tree_id != commit.tree_id()` (trees differ)
/// - `candidate.summary == commit.summary().unwrap_or("").trim()` (summary matches)
/// - `candidate.email == commit.author().email().unwrap_or("")` (email matches)
///
/// Returns a simple `bool` (no error propagation needed).
fn is_tip_patch_rewrite(
    commit: &Commit,
    candidate: &FloatingTargetCandidate,
    target_id: Oid,
) -> bool {
    candidate.id == target_id
        && candidate.tree_id != commit.tree_id()
        && candidate.summary == commit.summary().unwrap_or("").trim()
        && candidate.email == commit.author().email().unwrap_or("")
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

fn range_changes_present_in_target(
    repo: &Repository,
    base_id: Oid,
    branch_tip: Oid,
    target_tip: Oid,
) -> Result<bool> {
    if branch_tip == base_id {
        return Ok(true);
    }

    let output = Command::new("git")
        .arg("diff")
        .arg("--name-only")
        .arg("--no-renames")
        .arg("--no-ext-diff")
        .arg(base_id.to_string())
        .arg(branch_tip.to_string())
        .current_dir(repo_root(repo)?)
        .output()?;

    if !output.status.success() {
        return Err(anyhow!(
            "git diff failed while checking whether branch changes are present in target."
        ));
    }

    let touched_paths = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.to_string())
        .collect::<Vec<_>>();

    if touched_paths.is_empty() {
        return Ok(true);
    }

    let repo_root = repo_root(repo)?;
    let branch_patch_id = compute_range_patch_id(
        repo_root,
        &format!("{base_id}..{branch_tip}"),
        &touched_paths,
    )?
    .ok_or_else(|| anyhow!("Missing branch patch id while checking target containment."))?;
    let target_patch_id = compute_range_patch_id(
        repo_root,
        &format!("{base_id}..{target_tip}"),
        &touched_paths,
    )?;

    if target_patch_id.as_deref() == Some(branch_patch_id.as_str()) {
        return Ok(true);
    }

    let Some(_) = target_patch_id else {
        return Ok(false);
    };

    let mut log_child = Command::new("git")
        .arg("log")
        .arg("-p")
        .arg("--format=%H")
        .arg(target_tip.to_string())
        .arg("--")
        .args(&touched_paths)
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .spawn()?;

    let log_stdout = log_child.stdout.take().ok_or_else(|| {
        anyhow!("Failed to capture git log output while checking target patch ids.")
    })?;

    let target_patch_output = Command::new("git")
        .arg("patch-id")
        .arg("--stable")
        .current_dir(repo_root)
        .stdin(Stdio::from(log_stdout))
        .output()?;

    let log_status = log_child.wait()?;
    if !log_status.success() {
        return Err(anyhow!(
            "git log failed while checking whether branch changes are present in target."
        ));
    }
    if !target_patch_output.status.success() {
        return Err(anyhow!(
            "git patch-id failed while checking whether branch changes are present in target."
        ));
    }

    Ok(String::from_utf8_lossy(&target_patch_output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .any(|patch_id| patch_id == branch_patch_id))
}

fn compute_range_patch_id(
    repo_root: &Path,
    range_spec: &str,
    touched_paths: &[String],
) -> Result<Option<String>> {
    let mut diff_child = Command::new("git")
        .arg("diff")
        .arg("-U0")
        .arg("--no-ext-diff")
        .arg(range_spec)
        .arg("--")
        .args(touched_paths)
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .spawn()?;

    let diff_stdout = diff_child.stdout.take().ok_or_else(|| {
        anyhow!("Failed to capture git diff output while computing range patch id.")
    })?;

    let patch_output = Command::new("git")
        .arg("patch-id")
        .arg("--stable")
        .current_dir(repo_root)
        .stdin(Stdio::from(diff_stdout))
        .output()?;

    let diff_status = diff_child.wait()?;
    if !diff_status.success() {
        return Err(anyhow!(
            "git diff failed while computing range patch id for target containment."
        ));
    }
    if !patch_output.status.success() {
        return Err(anyhow!(
            "git patch-id failed while computing range patch id for target containment."
        ));
    }

    Ok(String::from_utf8_lossy(&patch_output.stdout)
        .lines()
        .find_map(|line| {
            line.split_whitespace()
                .next()
                .map(|patch_id| patch_id.to_string())
        }))
}

fn ordered_stack_lineage(
    repo: &Repository,
    top_id: Oid,
    stack_branches: &[StackBranch],
) -> Result<Vec<StackBranch>> {
    let mut lineage = Vec::new();
    for branch in stack_branches {
        if branch.id == top_id || repo.graph_descendant_of(top_id, branch.id)? {
            lineage.push(branch.clone());
        }
    }

    sort_branches_topologically(repo, &mut lineage)?;
    Ok(lineage)
}

fn branch_segment_integrated(
    repo: &Repository,
    old_base: Oid,
    branch_tip: Oid,
    upstream_id: Oid,
) -> Result<bool> {
    if repo.graph_descendant_of(upstream_id, branch_tip)? {
        return Ok(true);
    }

    range_changes_present_in_target(repo, old_base, branch_tip, upstream_id)
}

fn repo_root(repo: &Repository) -> Result<&Path> {
    if let Some(workdir) = repo.workdir() {
        return Ok(workdir);
    }

    repo.path()
        .parent()
        .ok_or_else(|| anyhow!("Failed to resolve repository root path."))
}

pub fn resolve_merge_base(repo: &Repository, a: Oid, b: Oid) -> Result<Oid> {
    match repo.merge_base(a, b) {
        Ok(merge_base) => Ok(merge_base),
        Err(err) if err.code() == git2::ErrorCode::NotFound => git_merge_base(repo, a, b),
        Err(err) => Err(err.into()),
    }
}

fn git_merge_base(repo: &Repository, a: Oid, b: Oid) -> Result<Oid> {
    let output = Command::new("git")
        .arg("merge-base")
        .arg(a.to_string())
        .arg(b.to_string())
        .current_dir(repo_root(repo)?)
        .output()?;

    if !output.status.success() {
        return Err(anyhow!("no merge base found between {} and {}.", a, b));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let merge_base = stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| anyhow!("git merge-base produced no output."))?;
    Oid::from_str(merge_base.trim()).map_err(|err| err.into())
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

fn parent_base_spec(parent_id: Oid, branch_name: &str, all_branches: &[StackBranch]) -> String {
    all_branches
        .iter()
        .find(|branch| branch.id == parent_id && branch.name != branch_name)
        .map(|branch| branch.name.clone())
        .unwrap_or_else(|| parent_id.to_string())
}

fn is_descendant(repo: &Repository, a: Oid, b: Oid) -> Result<bool> {
    repo.graph_descendant_of(a, b).map_err(|e| e.into())
}

pub fn sort_branches_topologically(repo: &Repository, branches: &mut [StackBranch]) -> Result<()> {
    let original = branches.to_vec();
    let mut outgoing = vec![Vec::new(); original.len()];
    let mut indegree = vec![0usize; original.len()];

    for (idx, branch) in original.iter().enumerate() {
        for (other_idx, other) in original.iter().enumerate() {
            if idx == other_idx || branch.id == other.id {
                continue;
            }

            if is_descendant(repo, branch.id, other.id)? {
                outgoing[other_idx].push(idx);
                indegree[idx] += 1;
            }
        }
    }

    let mut ready = (0..original.len())
        .filter(|&idx| indegree[idx] == 0)
        .collect::<Vec<_>>();
    ready.sort_by(|&a, &b| original[a].name.cmp(&original[b].name));
    let mut ready = VecDeque::from(ready);

    let mut ordered = Vec::with_capacity(original.len());
    while let Some(idx) = ready.pop_front() {
        ordered.push(idx);

        for &child_idx in &outgoing[idx] {
            indegree[child_idx] -= 1;
            if indegree[child_idx] == 0 {
                ready.push_back(child_idx);
            }
        }

        let mut ready_sorted = ready.into_iter().collect::<Vec<_>>();
        ready_sorted.sort_by(|&a, &b| original[a].name.cmp(&original[b].name));
        ready = VecDeque::from(ready_sorted);
    }

    if ordered.len() != original.len() {
        return Err(anyhow!("Failed to topologically sort stack branches."));
    }

    for (slot, idx) in ordered.into_iter().enumerate() {
        branches[slot] = original[idx].clone();
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
