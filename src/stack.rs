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
pub struct StackCommit {
    pub commit_id: Oid,
    pub branch_name: String,
    pub position: (usize, usize),
    pub message: String,
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

#[derive(Default)]
struct TargetPathHistory {
    commits: Vec<TargetPathCommit>,
}

struct TargetPathCommit {
    id: Oid,
    changed_paths: HashSet<String>,
}

impl TargetPathHistory {
    fn load(repo: &Repository, target_tip: Oid, paths: &[String]) -> Result<Self> {
        if paths.is_empty() {
            return Ok(Self::default());
        }

        let output = Command::new("git")
            .arg("log")
            .arg("--format=__KINDRA_COMMIT__%H")
            .arg("--name-only")
            .arg("--no-renames")
            .arg("--no-ext-diff")
            .arg(target_tip.to_string())
            .arg("--")
            .args(paths)
            .current_dir(repo_root(repo)?)
            .output()?;

        if !output.status.success() {
            return Err(anyhow!(
                "git log failed while indexing target history for sync containment checks."
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut commits = Vec::new();
        let mut current: Option<TargetPathCommit> = None;

        for line in stdout.lines() {
            if let Some(raw_oid) = line.strip_prefix("__KINDRA_COMMIT__") {
                if let Some(commit) = current.take() {
                    commits.push(commit);
                }
                current = Some(TargetPathCommit {
                    id: Oid::from_str(raw_oid.trim())?,
                    changed_paths: HashSet::new(),
                });
                continue;
            }

            if line.trim().is_empty() {
                continue;
            }

            if let Some(commit) = current.as_mut() {
                commit.changed_paths.insert(line.to_string());
            }
        }

        if let Some(commit) = current {
            commits.push(commit);
        }

        Ok(Self { commits })
    }

    fn covering_commits(&self, touched_paths: &[String]) -> Vec<Oid> {
        self.commits
            .iter()
            .filter(|commit| {
                commit.changed_paths.len() >= touched_paths.len()
                    && touched_paths
                        .iter()
                        .all(|path| commit.changed_paths.contains(path))
            })
            .map(|commit| commit.id)
            .collect()
    }
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
    let prefix_touched_paths = first_parent_chain
        .iter()
        .map(|&commit_id| range_touched_paths(repo, branch_cutoff, commit_id))
        .collect::<Result<Vec<_>>>()?;
    let mut union_paths = prefix_touched_paths
        .iter()
        .flat_map(|paths| paths.iter().cloned())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    union_paths.sort();
    let target_history = TargetPathHistory::load(repo, upstream_id, &union_paths)?;
    let mut prefix_end: isize = -1;

    for (idx, (&commit_id, touched_paths)) in first_parent_chain
        .iter()
        .zip(prefix_touched_paths.iter())
        .enumerate()
    {
        let merged_by_graph =
            repo.graph_descendant_of(upstream_id, commit_id)? || upstream_id == commit_id;
        if merged_by_graph {
            prefix_end = idx as isize;
            continue;
        }

        if range_changes_present_in_target_with_history(
            repo,
            branch_cutoff,
            commit_id,
            upstream_id,
            touched_paths,
            &target_history,
        )? {
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

    sort_branches_topologically(repo, &mut sub_stack)?;
    let current_parent_id =
        find_parent_in_stack(repo, current_branch_name, all_branches_in_stack, merge_base)?;
    // A tree rotation cannot preserve merge edges using branch-by-branch rebases.
    // The parent and its ancestors are unchanged, so exclude them from validation.
    for branch in &sub_stack {
        for id in collect_first_parent_chain(repo, current_parent_id, branch.id)? {
            if repo.find_commit(id)?.parent_count() > 1 {
                return Err(anyhow!(
                    "Cannot reorder because the affected subtree is forked and merged. Tree reordering requires single-parent commits."
                ));
            }
        }
    }

    let current_parent = if current_parent_id == merge_base {
        upstream_name.to_string()
    } else {
        parent_base_spec(
            current_parent_id,
            current_branch_name,
            all_branches_in_stack,
        )
    };

    let mut new_base_map = HashMap::new();
    for branch in &sub_stack {
        let parent_id =
            find_parent_in_stack(repo, &branch.name, all_branches_in_stack, merge_base)?;
        new_base_map.insert(
            branch.name.clone(),
            parent_base_spec(parent_id, &branch.name, all_branches_in_stack),
        );
    }

    // Preserve the existing linear rotation, stopping at a fork instead of
    // choosing an arbitrary leaf. Only these two edges change; side paths keep
    // their original parents on both sides of the rotation.
    let mut attachment = target_branch_name.to_string();
    loop {
        let children = sub_stack
            .iter()
            .filter(|branch| new_base_map.get(&branch.name) == Some(&attachment))
            .collect::<Vec<_>>();
        if children.len() != 1 {
            break;
        }
        attachment = children[0].name.clone();
    }
    new_base_map.insert(target_branch_name.to_string(), current_parent.clone());
    new_base_map.insert(current_branch_name.to_string(), attachment);
    let order_hint = sub_stack
        .iter()
        .enumerate()
        .map(|(idx, branch)| (branch.name.clone(), idx))
        .collect();
    let remaining_branches =
        topologically_sort_edited_graph(&new_base_map, &current_parent, &order_hint)?;

    Ok(Some(ReorderPlan {
        ordered_sub_stack: sub_stack,
        remaining_branches,
        new_base_map,
    }))
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
    let target_name_for_comparison = full_target_ref_name.as_deref().unwrap_or(target_ref_name);
    let target_short_name = target_name_for_comparison
        .strip_prefix("refs/heads/")
        .or_else(|| target_name_for_comparison.strip_prefix("refs/remotes/"))
        .unwrap_or(target_name_for_comparison);
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

        if branch.get().shorthand() == Some(target_short_name) {
            continue;
        }

        if let Ok(upstream_branch) = branch.upstream()
            && let Some(upstream_name) = upstream_branch.name()?
        {
            // Get the short name of the upstream branch
            let upstream_short = upstream_name
                .strip_prefix("refs/remotes/")
                .unwrap_or(upstream_name);
            if upstream_short == target_short_name {
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
    candidate_positions: HashMap<Oid, usize>,
    patch_ids: HashSet<String>,
    /// Paths touched by each commit behind `patch_ids`, as sets. A branch
    /// commit can only share a patch id with a target commit that touches the
    /// same paths, so this prunes the patch-id fallback to the few candidates
    /// that could match. `None` when a merge commit is in the private lineage;
    /// its combined diff has no single path set to compare against.
    patch_path_sets: Option<HashSet<Vec<String>>>,
    reflog_ids: HashSet<Oid>,
    historical_tip_ids: HashSet<Oid>,
}

#[derive(Clone)]
struct FloatingTargetCandidate {
    id: Oid,
    tree_id: Oid,
    summary: String,
    email: String,
    parent_id: Option<Oid>,
}

#[derive(Clone, Copy)]
struct FloatingBaseMatch {
    branch_id: Oid,
    target_index: usize,
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
    let mut all_candidates = Vec::new();
    let mut commit_ids = Vec::new();
    let mut current = Some(target_commit.id());
    let mut remaining = history_limit;

    while let Some(commit_id) = current {
        if history_limit != 0 && remaining == 0 {
            break;
        }
        let commit = repo.find_commit(commit_id)?;
        all_candidates.push(FloatingTargetCandidate {
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
    let private_len = match patch_id_boundary {
        Some(boundary) => commit_ids
            .iter()
            .position(|oid| *oid == boundary)
            .unwrap_or(commit_ids.len()),
        None => commit_ids.len(),
    };
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
    let patch_path_sets = if patch_commit_ids.is_empty() {
        Some(HashSet::new())
    } else if patch_commit_ids
        .iter()
        .map(|oid| {
            repo.find_commit(*oid)
                .map(|commit| commit.parent_count() > 1)
        })
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .any(|is_merge| is_merge)
    {
        None
    } else {
        Some(
            touched_path_sets(repo, &patch_commit_ids)?
                .into_values()
                .collect(),
        )
    };
    let candidates: Vec<FloatingTargetCandidate> =
        all_candidates.into_iter().take(private_len).collect();
    let candidate_ids = candidates.iter().map(|candidate| candidate.id).collect();
    let candidate_positions = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| (candidate.id, index))
        .collect();
    let reflog_ids = read_branch_reflog_ids(repo, target_branch);

    // Exact shared ancestors need evidence that they belonged to this branch,
    // and a private boundary so shared upstream history cannot adopt siblings.
    let mut historical_tip_ids = HashSet::new();
    if patch_id_boundary.is_some() {
        historical_tip_ids.extend(reflog_ids.iter().copied());
        if let Ok(branch) = repo.find_branch(target_branch, git2::BranchType::Local)
            && let Ok(upstream) = branch.upstream()
            && upstream.get().is_remote()
            && let Some(tip) = upstream.get().target()
        {
            historical_tip_ids.insert(tip);
        }
    }

    Ok(FloatingTargetContext {
        candidates,
        candidate_ids,
        candidate_positions,
        patch_ids,
        patch_path_sets,
        reflog_ids,
        historical_tip_ids,
    })
}

/// For each branch tip, the commit on its history that the target branch was
/// rewritten from (its floating base), or `None` when the branch is not a
/// floating child of the target.
///
/// The tips are resolved together because the patch-id fallback needs a
/// `git show | git patch-id` round trip per batch of commits; sharing it lets
/// every branch's candidates go out in one batch instead of one pair of
/// processes per branch.
pub fn find_floating_bases(
    repo: &Repository,
    branch_tips: &[Oid],
    target: &FloatingTargetContext,
    history_limit: usize,
    patch_id_cache: &mut HashMap<Oid, Option<String>>,
) -> Result<Vec<Option<Oid>>> {
    let mut walks = Vec::with_capacity(branch_tips.len());
    let mut pending = Vec::new();
    for &branch_tip in branch_tips {
        let walk =
            walk_floating_candidates(repo, branch_tip, target, history_limit, patch_id_cache)?;
        if let FloatingWalk::PatchCandidates(candidates) = &walk {
            pending.extend(candidates.iter().copied());
        }
        walks.push(walk);
    }
    let pending = filter_patch_candidates(repo, target, &pending)?;
    ensure_patch_ids(repo, &pending, patch_id_cache)?;
    let mut results = Vec::with_capacity(branch_tips.len());
    for (walk, &branch_tip) in walks.into_iter().zip(branch_tips) {
        let found = match walk {
            FloatingWalk::Resolved(found) => found,
            FloatingWalk::PatchCandidates(candidates) => {
                match_floating_patch_ids(repo, branch_tip, target, &candidates, patch_id_cache)?
            }
        };
        results.push(found.map(|matching| matching.branch_id));
    }
    Ok(results)
}

/// Outcome of walking a branch's first-parent history against the target.
enum FloatingWalk {
    /// The walk settled the answer on its own, by an exact, tree, or metadata match.
    Resolved(Option<FloatingBaseMatch>),
    /// The walk found nothing conclusive; these private commits remain to be
    /// compared by patch id (see [`match_floating_patch_ids`]).
    PatchCandidates(Vec<Oid>),
}

/// The merge bases of a branch tip and the target, or none when the histories
/// are unrelated. Every reachability question the walk asks reduces to these:
/// the target is an ancestor of the tip exactly when it is one of the bases,
/// and a commit on the tip's history is reachable from the target exactly when
/// some base reaches it. Resolving them once per branch keeps each question a
/// short walk near the fork point rather than a paint-down from the target
/// over everything the branch has fallen behind.
fn floating_merge_bases(repo: &Repository, branch_tip: Oid, target_id: Oid) -> Result<Vec<Oid>> {
    match repo.merge_bases(branch_tip, target_id) {
        Ok(bases) => Ok(bases.iter().copied().collect()),
        Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(Vec::new()),
        Err(err) => Err(err.into()),
    }
}

/// Keep only the patch candidates that could share a patch id with a target
/// commit: those touching exactly the paths some target commit touches. Merge
/// commits are kept as they are; they have no single path set to compare.
/// Candidates are deduplicated, preserving first occurrence.
fn filter_patch_candidates(
    repo: &Repository,
    target: &FloatingTargetContext,
    candidates: &[Oid],
) -> Result<Vec<Oid>> {
    let mut seen = HashSet::new();
    let candidates: Vec<Oid> = candidates
        .iter()
        .copied()
        .filter(|oid| seen.insert(*oid))
        .collect();
    let Some(target_path_sets) = &target.patch_path_sets else {
        return Ok(candidates);
    };
    let mut kept = Vec::new();
    let mut comparable = Vec::new();
    for oid in candidates {
        if repo.find_commit(oid)?.parent_count() > 1 {
            kept.push(oid);
        } else {
            comparable.push(oid);
        }
    }
    let path_sets = touched_path_sets(repo, &comparable)?;
    for oid in comparable {
        let paths = path_sets.get(&oid).cloned().unwrap_or_default();
        if target_path_sets.contains(&paths) {
            kept.push(oid);
        }
    }
    Ok(kept)
}

/// The sorted set of paths each commit changes against its first parent (or
/// against the empty tree for a root commit), in one `git diff-tree` run.
///
/// The output is read as NUL-delimited `--name-status` records: a bare commit
/// id opens a commit's block, and every change follows as a status record and
/// then its path record. A path is always consumed as the record after a
/// status, so a file whose name happens to spell a commit id cannot be taken
/// for a header.
fn touched_path_sets(repo: &Repository, commit_ids: &[Oid]) -> Result<HashMap<Oid, Vec<String>>> {
    let mut sets: HashMap<Oid, Vec<String>> = HashMap::new();
    if commit_ids.is_empty() {
        return Ok(sets);
    }
    let mut child = Command::new("git")
        .arg("diff-tree")
        .arg("-r")
        .arg("--root")
        .arg("--name-status")
        .arg("-z")
        .arg("--no-ext-diff")
        .arg("--stdin")
        .current_dir(repo_root(repo)?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("Failed to open git diff-tree stdin for path listing."))?;
    let input: String = commit_ids.iter().map(|oid| format!("{oid}\n")).collect();
    // Feed the ids from another thread: with enough commits the output fills
    // the pipe before the input is fully written, and both ends would block.
    let output = std::thread::scope(|scope| {
        let writer = scope.spawn(move || {
            use std::io::Write;
            stdin.write_all(input.as_bytes())
        });
        let output = child.wait_with_output();
        let written = writer
            .join()
            .map_err(|_| anyhow!("git diff-tree input writer panicked"))?;
        // A closed pipe means diff-tree exited early; its status carries the error.
        if let Err(err) = written
            && err.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(err.into());
        }
        output.map_err(anyhow::Error::from)
    })?;
    if !output.status.success() {
        return Err(anyhow!("git diff-tree failed while listing touched paths."));
    }
    let requested: HashSet<Oid> = commit_ids.iter().copied().collect();
    let mut current: Option<Oid> = None;
    let mut records = output.stdout.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        let record = String::from_utf8_lossy(record);
        if let Ok(oid) = Oid::from_str(&record)
            && requested.contains(&oid)
        {
            sets.entry(oid).or_default();
            current = Some(oid);
            continue;
        }
        // Any other record is a change status; the path is the record after it.
        let Some(path) = records.next() else {
            break;
        };
        if let Some(oid) = current {
            sets.entry(oid)
                .or_default()
                .push(String::from_utf8_lossy(path).into_owned());
        }
    }
    for paths in sets.values_mut() {
        paths.sort();
        paths.dedup();
    }
    Ok(sets)
}

fn reachable_from_merge_bases(repo: &Repository, bases: &[Oid], oid: Oid) -> Result<bool> {
    if bases.contains(&oid) {
        return Ok(true);
    }
    for &base in bases {
        if repo.graph_descendant_of(base, oid)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn walk_floating_candidates(
    repo: &Repository,
    branch_tip: Oid,
    target: &FloatingTargetContext,
    history_limit: usize,
    patch_id_cache: &mut HashMap<Oid, Option<String>>,
) -> Result<FloatingWalk> {
    let Some(target_id) = target.candidates.first().map(|candidate| candidate.id) else {
        return Ok(FloatingWalk::Resolved(None));
    };

    // If the branch is already on top of, already part of, or already integrated
    // into the target branch, it is not a floating child that needs restacking.
    if branch_tip == target_id {
        return Ok(FloatingWalk::Resolved(None));
    }
    let merge_bases = floating_merge_bases(repo, branch_tip, target_id)?;
    if merge_bases.contains(&target_id) || merge_bases.contains(&branch_tip) {
        return Ok(FloatingWalk::Resolved(None));
    }

    let mut patch_candidates = Vec::new();
    let mut current = Some(branch_tip);
    let mut remaining = history_limit;

    while let Some(oid) = current {
        if history_limit != 0 && remaining == 0 {
            break;
        }

        // An unchanged commit in the target's private lineage is an exact fork
        // point when the parent gained commits. Check before stopping at shared
        // history; upstream commits are excluded from candidate_positions.
        if oid != branch_tip
            && target.historical_tip_ids.contains(&oid)
            && let Some(&target_index) = target.candidate_positions.get(&oid)
        {
            return Ok(FloatingWalk::Resolved(Some(FloatingBaseMatch {
                branch_id: oid,
                target_index,
            })));
        }

        // Optimization: If we hit a commit that is reachable from the target, we stop.
        // Because any match found *after* this point would be a common ancestor, not a floating base.
        if reachable_from_merge_bases(repo, &merge_bases, oid)? {
            break;
        }

        let commit = repo.find_commit(oid)?;
        let parent_id = first_parent_id(&commit)?;
        current = parent_id;
        if history_limit != 0 {
            remaining -= 1;
        }

        if let Some(&target_index) = target.candidate_positions.get(&oid) {
            if oid != branch_tip {
                let matching = FloatingBaseMatch {
                    branch_id: oid,
                    target_index,
                };
                if validate_floating_match(repo, branch_tip, matching, target, patch_id_cache)? {
                    return Ok(FloatingWalk::Resolved(Some(matching)));
                }
            }
            continue;
        }

        // Match by tree-hash against rewritten target commits, but only if the
        // surrounding parent commit also lines up with the rewritten lineage.
        for (target_index, candidate) in target.candidates.iter().enumerate() {
            if candidate.tree_id != commit.tree_id() {
                continue;
            }
            if !floating_parent_matches_candidate(
                repo,
                parent_id,
                candidate.parent_id,
                patch_id_cache,
            )? {
                continue;
            }
            if oid != branch_tip {
                let matching = FloatingBaseMatch {
                    branch_id: oid,
                    target_index,
                };
                if validate_floating_match(repo, branch_tip, matching, target, patch_id_cache)? {
                    return Ok(FloatingWalk::Resolved(Some(matching)));
                }
            }
        }

        // Metadata matches narrow the patch-id fallback, but are not sufficient on their own.
        if let Some(target_index) = metadata_matches_target_candidate(&commit, oid, target)?
            && oid != branch_tip
        {
            let matching = FloatingBaseMatch {
                branch_id: oid,
                target_index,
            };
            if validate_floating_match(repo, branch_tip, matching, target, patch_id_cache)? {
                return Ok(FloatingWalk::Resolved(Some(matching)));
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
                    let matching = FloatingBaseMatch {
                        branch_id: oid,
                        target_index: 0,
                    };
                    if validate_floating_match(repo, branch_tip, matching, target, patch_id_cache)?
                    {
                        return Ok(FloatingWalk::Resolved(Some(matching)));
                    }
                }
            }
        }

        patch_candidates.push(oid);
    }

    // No private target commit means no patch id can match; skip the round trip.
    if target.patch_ids.is_empty() {
        return Ok(FloatingWalk::Resolved(None));
    }
    Ok(FloatingWalk::PatchCandidates(patch_candidates))
}

/// Patch-id fallback of the floating walk: match the branch's private commits
/// against rewritten target commits by content. Callers must have populated
/// `patch_id_cache` for `patch_candidates` (see [`ensure_patch_ids`]).
fn match_floating_patch_ids(
    repo: &Repository,
    branch_tip: Oid,
    target: &FloatingTargetContext,
    patch_candidates: &[Oid],
    patch_id_cache: &mut HashMap<Oid, Option<String>>,
) -> Result<Option<FloatingBaseMatch>> {
    let Some(target_id) = target.candidates.first().map(|candidate| candidate.id) else {
        return Ok(None);
    };
    for &oid in patch_candidates {
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
        for (target_index, candidate) in target.candidates.iter().enumerate() {
            let Some(candidate_patch_id) =
                patch_id_cache.get(&candidate.id).and_then(|v| v.as_ref())
            else {
                continue;
            };
            if candidate_patch_id != &patch_id {
                continue;
            }
            let parent_matches = floating_parent_matches_candidate(
                repo,
                parent_id,
                candidate.parent_id,
                patch_id_cache,
            )?;
            if !(parent_matches || is_tip_patch_rewrite(&commit, candidate, target_id)) {
                continue;
            }

            let matching = FloatingBaseMatch {
                branch_id: oid,
                target_index,
            };
            if validate_floating_match(repo, branch_tip, matching, target, patch_id_cache)? {
                return Ok(Some(matching));
            }
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
) -> Result<Option<usize>> {
    if !target.reflog_ids.contains(&oid) {
        return Ok(None);
    }

    let summary = commit.summary().unwrap_or("").trim();
    let author = commit.author();
    let email = author.email().unwrap_or("");
    let parent_id = if commit.parent_count() > 0 {
        Some(commit.parent_id(0)?)
    } else {
        None
    };

    Ok(target.candidates.iter().position(|candidate| {
        candidate.summary == summary && candidate.email == email && candidate.parent_id == parent_id
    }))
}

fn validate_floating_match(
    repo: &Repository,
    branch_tip: Oid,
    matching: FloatingBaseMatch,
    target: &FloatingTargetContext,
    patch_id_cache: &mut HashMap<Oid, Option<String>>,
) -> Result<bool> {
    let Some(target_tip) = target.candidates.first().map(|candidate| candidate.id) else {
        return Ok(false);
    };
    let branch_chain = match repo.merge_base(branch_tip, target_tip) {
        Ok(graph_merge_base) => collect_first_parent_chain(repo, graph_merge_base, branch_tip)?,
        Err(_) => collect_first_parent_chain_to_root(repo, branch_tip)?,
    };
    let Some(match_index) = branch_chain
        .iter()
        .position(|oid| *oid == matching.branch_id)
    else {
        return Ok(false);
    };

    // Floating branches must have at least one commit ahead of the matched old base.
    if match_index + 1 >= branch_chain.len() {
        return Ok(false);
    }

    let mut chain_len = 1usize;
    let mut branch_index = match_index;
    let mut target_index = matching.target_index;

    while branch_index > 0 && target_index + 1 < target.candidates.len() {
        let branch_oid = branch_chain[branch_index - 1];
        let candidate = &target.candidates[target_index + 1];
        if !branch_commit_matches_target_candidate(
            repo,
            branch_oid,
            candidate,
            target,
            patch_id_cache,
        )? {
            break;
        }
        chain_len += 1;
        branch_index -= 1;
        target_index += 1;
    }

    if chain_len >= 2 {
        return Ok(true);
    }

    // Fallback: a single matched point is enough only when it is the branch's immediate
    // fork point above the graph merge-base, meaning all newer commits belong to the suffix
    // that should be replayed.
    Ok(match_index == 0)
}

fn collect_first_parent_chain_to_root(repo: &Repository, tip: Oid) -> Result<Vec<Oid>> {
    let mut chain = Vec::new();
    let mut current = Some(tip);

    while let Some(oid) = current {
        chain.push(oid);
        let commit = repo.find_commit(oid)?;
        current = if commit.parent_count() > 0 {
            Some(commit.parent_id(0)?)
        } else {
            None
        };
    }

    chain.reverse();
    Ok(chain)
}

fn branch_commit_matches_target_candidate(
    repo: &Repository,
    branch_oid: Oid,
    candidate: &FloatingTargetCandidate,
    target: &FloatingTargetContext,
    patch_id_cache: &mut HashMap<Oid, Option<String>>,
) -> Result<bool> {
    if branch_oid == candidate.id {
        return Ok(true);
    }

    let branch_commit = repo.find_commit(branch_oid)?;
    if branch_commit.tree_id() == candidate.tree_id {
        return Ok(true);
    }

    ensure_patch_ids(repo, &[branch_oid, candidate.id], patch_id_cache)?;
    let branch_patch_id = patch_id_cache
        .get(&branch_oid)
        .and_then(|value| value.as_ref());
    let candidate_patch_id = patch_id_cache
        .get(&candidate.id)
        .and_then(|value| value.as_ref());
    if branch_patch_id.is_some() && branch_patch_id == candidate_patch_id {
        return Ok(true);
    }

    if target.reflog_ids.contains(&branch_oid)
        && candidate.summary == branch_commit.summary().unwrap_or("").trim()
        && candidate.email == branch_commit.author().email().unwrap_or("")
    {
        return Ok(true);
    }

    Ok(false)
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
    // Compared only against other commit patch ids, so any context width works
    // as long as it is the same on both sides.
    compute_patch_ids_for_commits(repo_root(repo)?, commit_ids, None, DiffContext::Default)
}

fn compute_commit_patch_ids_for_paths(
    repo_root: &Path,
    commit_ids: &[Oid],
    touched_paths: &[String],
) -> Result<HashMap<Oid, String>> {
    // These are compared against a range patch id from `compute_range_patch_id`,
    // which diffs with zero context. `git patch-id` hashes context lines too, so
    // both sides have to ask for the same width or no pair can ever match.
    compute_patch_ids_for_commits(
        repo_root,
        commit_ids,
        Some(touched_paths),
        DiffContext::Zero,
    )
}

/// How much context a diff carries before it is hashed into a patch id. Patch
/// ids are only comparable between diffs generated with the same width.
#[derive(Clone, Copy)]
enum DiffContext {
    Default,
    Zero,
}

impl DiffContext {
    fn arg(self) -> Option<&'static str> {
        match self {
            DiffContext::Default => None,
            DiffContext::Zero => Some("-U0"),
        }
    }
}

fn compute_patch_ids_for_commits(
    repo_root: &Path,
    commit_ids: &[Oid],
    touched_paths: Option<&[String]>,
    context: DiffContext,
) -> Result<HashMap<Oid, String>> {
    if commit_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut result = HashMap::new();
    const PATCH_ID_BATCH_SIZE: usize = 512;

    for chunk in commit_ids.chunks(PATCH_ID_BATCH_SIZE) {
        let mut show = Command::new("git");
        show.arg("show").arg("--no-ext-diff").arg("--no-color");
        if let Some(context_arg) = context.arg() {
            show.arg(context_arg);
        }
        for oid in chunk {
            show.arg(oid.to_string());
        }
        if let Some(paths) = touched_paths {
            show.arg("--").args(paths);
        }

        let mut show_child = show.current_dir(repo_root).stdout(Stdio::piped()).spawn()?;

        let show_stdout = show_child.stdout.take().ok_or_else(|| {
            anyhow!("Failed to capture git show output for patch-id calculation.")
        })?;

        let patch_output = Command::new("git")
            .arg("patch-id")
            .arg("--stable")
            .current_dir(repo_root)
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
    let touched_paths = range_touched_paths(repo, base_id, branch_tip)?;

    if touched_paths.is_empty() {
        return Ok(true);
    }

    let target_history = TargetPathHistory::load(repo, target_tip, &touched_paths)?;
    range_changes_present_in_target_with_history(
        repo,
        base_id,
        branch_tip,
        target_tip,
        &touched_paths,
        &target_history,
    )
}

fn range_touched_paths(repo: &Repository, base_id: Oid, branch_tip: Oid) -> Result<Vec<String>> {
    if branch_tip == base_id {
        return Ok(Vec::new());
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

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.to_string())
        .collect())
}

fn range_changes_present_in_target_with_history(
    repo: &Repository,
    base_id: Oid,
    branch_tip: Oid,
    target_tip: Oid,
    touched_paths: &[String],
    target_history: &TargetPathHistory,
) -> Result<bool> {
    if touched_paths.is_empty() {
        return Ok(true);
    }

    if commits_match_on_paths(repo, branch_tip, target_tip, touched_paths)? {
        return Ok(true);
    }

    if commits_match_on_paths(repo, base_id, target_tip, touched_paths)? {
        return Ok(false);
    }

    let candidate_commits = target_history.covering_commits(touched_paths);
    if candidate_commits.is_empty() {
        return Ok(false);
    }

    let repo_root = repo_root(repo)?;
    let branch_patch_id = compute_range_patch_id(
        repo_root,
        &format!("{base_id}..{branch_tip}"),
        touched_paths,
    )?
    .ok_or_else(|| anyhow!("Missing branch patch id while checking target containment."))?;
    let target_patch_ids =
        compute_commit_patch_ids_for_paths(repo_root, &candidate_commits, touched_paths)?;

    Ok(target_patch_ids
        .values()
        .any(|patch_id| patch_id == &branch_patch_id))
}

fn commits_match_on_paths(
    repo: &Repository,
    left_tip: Oid,
    right_tip: Oid,
    touched_paths: &[String],
) -> Result<bool> {
    let left_tree = repo.find_commit(left_tip)?.tree()?;
    let right_tree = repo.find_commit(right_tip)?.tree()?;

    for path in touched_paths {
        if tree_entry_state(&left_tree, Path::new(path))?
            != tree_entry_state(&right_tree, Path::new(path))?
        {
            return Ok(false);
        }
    }

    Ok(true)
}

fn tree_entry_state(tree: &git2::Tree<'_>, path: &Path) -> Result<Option<(Oid, i32)>> {
    match tree.get_path(path) {
        Ok(entry) => Ok(Some((entry.id(), entry.filemode()))),
        Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn compute_range_patch_id(
    repo_root: &Path,
    range_spec: &str,
    touched_paths: &[String],
) -> Result<Option<String>> {
    let mut diff_child = Command::new("git")
        .arg("diff")
        // Must match the width used for the commit patch ids this is compared
        // against, in `compute_commit_patch_ids_for_paths`.
        .arg(DiffContext::Zero.arg().unwrap_or("-U0"))
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

/// Enumerates stack commits for interactive selection.
///
/// Final ordering is newest branch first, and within each branch commits are tip first.
/// Commits that appear in multiple branches are emitted once via a global `seen` set, except
/// each branch tip is always included so branches that share a head still appear in the picker.
pub fn enumerate_stack_commits(
    repo: &Repository,
    stack_branches: &[StackBranch],
    upstream_name: &str,
) -> Result<Vec<StackCommit>> {
    let mut ordered_branches = stack_branches.to_vec();
    sort_branches_topologically(repo, &mut ordered_branches)?;

    let upstream_id = repo.revparse_single(upstream_name)?.peel_to_commit()?.id();
    let mut seen = HashSet::new();
    let mut branch_chunks = Vec::new();

    for branch in ordered_branches {
        let merge_base = repo.merge_base(upstream_id, branch.id)?;

        let mut revwalk = repo.revwalk()?;
        revwalk.push(branch.id)?;
        revwalk.hide(merge_base)?;
        revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;

        let mut branch_commits = Vec::new();
        // Walk each branch once and de-dupe globally so commits shared with an older branch
        // are not shown multiple times in the picker, while always retaining this branch tip.
        for oid in revwalk {
            let id = oid?;
            if id == branch.id {
                branch_commits.push(id);
                seen.insert(id);
                continue;
            }

            if seen.insert(id) {
                branch_commits.push(id);
            }
        }

        let total = branch_commits.len();
        // Reverse to tip-first so position is computed from the newest commit downward.
        branch_commits.reverse();

        let mut chunk = Vec::new();
        for (index, oid) in branch_commits.iter().enumerate() {
            let commit = repo.find_commit(*oid)?;
            chunk.push(StackCommit {
                commit_id: *oid,
                branch_name: branch.name.clone(),
                position: (total - index, total),
                message: commit.summary().unwrap_or("").to_string(),
            });
        }
        branch_chunks.push(chunk);
    }

    // Reverse chunks so branches are presented in stack order from newest to oldest.
    branch_chunks.reverse();

    let mut commits = Vec::new();
    for mut chunk in branch_chunks {
        commits.append(&mut chunk);
    }

    Ok(commits)
}

/// Enumerate commits accepted by `kin commit --fixup` for the current branch.
///
/// Feature branches are limited to the current stack. When HEAD is the base
/// branch itself, its first-parent history is eligible too: those commits are
/// excluded from the private stack range by definition, but are precisely the
/// commits a user on the base branch needs to be able to amend or autosquash.
pub fn enumerate_fixup_commits(
    repo: &Repository,
    stack_branches: &[StackBranch],
    upstream_name: &str,
    current_branch_name: &str,
    head_id: Oid,
) -> Result<Vec<StackCommit>> {
    let mut commits = enumerate_stack_commits(repo, stack_branches, upstream_name)?;
    if current_branch_name != upstream_name {
        return Ok(commits);
    }

    let mut ids = Vec::new();
    let mut commit = repo.find_commit(head_id)?;
    loop {
        ids.push(commit.id());
        if commit.parent_count() == 0 {
            break;
        }
        commit = commit.parent(0)?;
    }

    let total = ids.len();
    let mut seen: HashSet<Oid> = commits.iter().map(|commit| commit.commit_id).collect();
    let mut base_commits = Vec::with_capacity(total);
    for (index, id) in ids.into_iter().enumerate() {
        if !seen.insert(id) {
            continue;
        }
        let commit = repo.find_commit(id)?;
        base_commits.push(StackCommit {
            commit_id: id,
            branch_name: upstream_name.to_string(),
            position: (total - index, total),
            message: commit.summary().unwrap_or("").to_string(),
        });
    }
    base_commits.append(&mut commits);
    Ok(base_commits)
}

/// Discover and enumerate the commits accepted by `kin commit --fixup` from the
/// currently checked-out branch. Centralizes the branch / upstream / stack
/// discovery for callers such as the dynamic completion provider.
pub fn enumerate_current_fixup_commits(repo: &Repository) -> Result<Vec<StackCommit>> {
    if repo.head_detached()? {
        return Ok(Vec::new());
    }
    let Some(upstream_name) = crate::commands::find_upstream(repo)? else {
        return Ok(Vec::new());
    };
    let upstream_id = repo.revparse_single(&upstream_name)?.id();
    let head = repo.head()?;
    let head_id = head.peel_to_commit()?.id();
    let Some(current_branch_name) = head.shorthand() else {
        return Ok(Vec::new());
    };
    let stack_branches = get_stack_branches_for_head(repo, head_id, upstream_id, &upstream_name)?;
    enumerate_fixup_commits(
        repo,
        &stack_branches,
        &upstream_name,
        current_branch_name,
        head_id,
    )
}

/// Discover the complete connected stack, including siblings at unnamed forks.
/// Only shared commits outside upstream connect branches; sharing main alone
/// must not pull unrelated stacks into the tree. Keep lineage-only discovery
/// separate for commands that operate on ancestors and descendants of HEAD.
pub fn get_full_stack_branches_for_head(
    repo: &Repository,
    head_id: Oid,
    upstream_id: Oid,
    upstream_name: &str,
) -> Result<Vec<StackBranch>> {
    // Upstream already contains HEAD, so HEAD has no private history and there
    // is no component to select. Preserve the existing overview of stacks
    // descending from the current base.
    if head_id == upstream_id || repo.graph_descendant_of(upstream_id, head_id)? {
        return get_stack_branches_for_head(repo, head_id, upstream_id, upstream_name);
    }

    // The sources are HEAD followed by every local branch. Walking all of them
    // together, bounded by upstream, costs one traversal of the private history
    // rather than one per branch.
    let mut sources = vec![head_id];
    let mut candidates = Vec::new();
    for entry in repo.branches(Some(git2::BranchType::Local))? {
        let (branch, _) = entry?;
        let Some(name) = branch.name()? else { continue };
        if name == upstream_name {
            continue;
        }
        let Some(id) = branch.get().target() else {
            continue;
        };
        candidates.push(StackBranch {
            name: name.to_string(),
            id,
        });
        sources.push(id);
    }

    let mut walk = repo.revwalk()?;
    walk.set_sorting(git2::Sort::TOPOLOGICAL)?;
    for &id in &sources {
        walk.push(id)?;
    }
    walk.hide(upstream_id)?;
    let order = walk.collect::<std::result::Result<Vec<_>, _>>()?;
    let private: HashSet<Oid> = order.iter().copied().collect();

    // Two sources belong to the same stack when their private histories share a
    // commit. Handing each commit's group down to its parents discovers exactly
    // those overlaps, and the disjoint set closes them transitively.
    let mut seeds: HashMap<Oid, Vec<usize>> = HashMap::new();
    for (index, id) in sources.iter().enumerate() {
        if private.contains(id) {
            seeds.entry(*id).or_default().push(index);
        }
    }
    let mut groups = DisjointSet::new(sources.len());
    let mut group_of: HashMap<Oid, usize> = HashMap::new();
    for oid in order {
        let mut group = group_of.remove(&oid);
        for &seed in seeds.get(&oid).map(Vec::as_slice).unwrap_or_default() {
            group = Some(match group {
                Some(group) => groups.union(group, seed),
                None => seed,
            });
        }
        let Some(group) = group else { continue };
        let group = groups.find(group);
        for parent in repo.find_commit(oid)?.parent_ids() {
            // Commits already in upstream are shared by unrelated stacks, so
            // they must not be allowed to connect them.
            if !private.contains(&parent) {
                continue;
            }
            let merged = match group_of.get(&parent) {
                Some(&existing) => groups.union(existing, group),
                None => group,
            };
            group_of.insert(parent, merged);
        }
    }

    let head_group = groups.find(0);
    let mut branches = Vec::new();
    for (index, branch) in candidates.into_iter().enumerate() {
        // `sources` carries HEAD at index 0, so branch `index` sits at `index + 1`.
        if private.contains(&branch.id) && groups.find(index + 1) == head_group {
            branches.push(branch);
        }
    }
    sort_branches_topologically(repo, &mut branches)?;
    Ok(branches)
}

/// Discover the stack for `head_id` relative to `upstream_id`, computing the
/// merge base internally before delegating to [`get_stack_branches_from_merge_base`].
///
/// This is the shared entry point for callers that only have HEAD and the
/// upstream (e.g. `push`, `pr`) and would otherwise each repeat the
/// `repo.merge_base(...)` + `get_stack_branches_from_merge_base(...)` boilerplate.
pub fn get_stack_branches_for_head(
    repo: &Repository,
    head_id: Oid,
    upstream_id: Oid,
    upstream_name: &str,
) -> Result<Vec<StackBranch>> {
    let merge_base = repo.merge_base(head_id, upstream_id)?;
    get_stack_branches_from_merge_base(repo, merge_base, head_id, upstream_id, upstream_name)
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

/// The branches at `root_id` together with every branch descending from it.
/// This is the sub-stack an operation that rewrites `root_id` must carry along.
pub fn collect_sub_stack_from_id(
    repo: &Repository,
    root_id: Oid,
    all_branches: &[StackBranch],
) -> Result<Vec<StackBranch>> {
    let mut branches: Vec<_> = all_branches
        .iter()
        .filter(|b| b.id == root_id)
        .cloned()
        .collect();
    collect_descendants_of_id(repo, root_id, all_branches, &mut branches)?;
    Ok(branches)
}

/// Whether `tip` lies on `head`'s own history (at or below it). A rewrite of
/// that history with `--update-refs` moves such a tip by itself; only branches
/// off the path need an explicit replay.
pub fn is_on_history_of(repo: &Repository, tip: Oid, head: Oid) -> Result<bool> {
    Ok(tip == head || repo.graph_descendant_of(head, tip)?)
}

/// Whether a branch forking from `fork_point` is the root of its side path:
/// its stack parent is the fork point itself, or lies outside the fork point's
/// descendants. A parent strictly above the fork point is restacked in its
/// own right and the branch simply follows it.
pub fn is_side_path_root(repo: &Repository, parent: Oid, fork_point: Oid) -> Result<bool> {
    Ok(parent == fork_point || !repo.graph_descendant_of(parent, fork_point)?)
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

/// Strict ancestry between a fixed set of branch tips, precomputed so callers
/// can ask "is A a descendant of B?" without walking the graph per question.
///
/// Resolving a stack's shape needs that answer for every pair of tips, and
/// `graph_descendant_of` walks history on each call. One walk over the commits
/// the tips actually span answers them all.
pub(crate) struct TipAncestry {
    /// Slot for every distinct tip, plus the boundary commit.
    slots: HashMap<Oid, usize>,
    /// `descendants[i]` holds the slots of tips that are strict descendants of
    /// the tip in slot `i`.
    descendants: Vec<HashSet<usize>>,
}

impl TipAncestry {
    /// Record ancestry between `tips`, or `None` when they share no common
    /// ancestor to bound the walk and the caller must query the graph directly.
    ///
    /// The bound is `merge_base_octopus`, the commit every tip descends from.
    /// Commits at or below it are shared by all tips and so cannot distinguish
    /// them, which is what keeps the walk proportional to the stack instead of
    /// to all of history. Note that `merge_base_many` is not usable here: it
    /// bases the later commits against a hypothetical merge of them, so it can
    /// return a commit some tip does not descend from, and bounding the walk
    /// there would hide that tip's ancestry and drop edges.
    pub(crate) fn new(repo: &Repository, tips: &[Oid]) -> Result<Option<Self>> {
        let Ok(boundary) = repo.merge_base_octopus(tips) else {
            return Ok(None);
        };
        let mut slots: HashMap<Oid, usize> = HashMap::new();
        for &tip in tips {
            let next = slots.len();
            slots.entry(tip).or_insert(next);
        }

        let mut walk = repo.revwalk()?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL)?;
        for &oid in slots.keys() {
            walk.push(oid)?;
        }
        // Hiding the boundary's parents rather than the boundary itself keeps
        // the boundary in the walk, so tips can still be related to it.
        for parent in repo.find_commit(boundary)?.parent_ids() {
            walk.hide(parent)?;
        }

        // `reach[c]` accumulates the tips that can reach commit `c`. Topological
        // order visits a commit only after every child that feeds into it, so
        // the entry is complete by the time it is read.
        let mut reach: HashMap<Oid, HashSet<usize>> = HashMap::new();
        let mut descendants = vec![HashSet::new(); slots.len()];
        for oid in walk {
            let oid = oid?;
            let mut here = reach.remove(&oid).unwrap_or_default();
            if let Some(&slot) = slots.get(&oid) {
                descendants[slot] = here.clone();
                here.insert(slot);
            }
            if here.is_empty() {
                continue;
            }
            for parent in repo.find_commit(oid)?.parent_ids() {
                reach
                    .entry(parent)
                    .or_default()
                    .extend(here.iter().copied());
            }
        }

        Ok(Some(Self { slots, descendants }))
    }

    /// Ancestry for every branch in `branches`, with `merge_base` included so
    /// it can be compared against too.
    pub(crate) fn for_branches(
        repo: &Repository,
        branches: &[StackBranch],
        merge_base: Oid,
    ) -> Result<Option<Self>> {
        let mut tips = branches.iter().map(|b| b.id).collect::<Vec<_>>();
        tips.push(merge_base);
        Self::new(repo, &tips)
    }

    /// Whether `descendant` is a strict descendant of `ancestor`, matching
    /// `Repository::graph_descendant_of` (a commit is not its own descendant).
    ///
    /// Returns `false` for commits this was not built for.
    pub(crate) fn is_descendant(&self, descendant: Oid, ancestor: Oid) -> bool {
        match (self.slots.get(&descendant), self.slots.get(&ancestor)) {
            (Some(&d), Some(&a)) => self.descendants[a].contains(&d),
            _ => false,
        }
    }
}

/// Disjoint-set over source indices, used to group branches whose private
/// histories overlap.
struct DisjointSet {
    parent: Vec<usize>,
}

impl DisjointSet {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
        }
    }

    fn find(&mut self, mut index: usize) -> usize {
        while self.parent[index] != index {
            self.parent[index] = self.parent[self.parent[index]];
            index = self.parent[index];
        }
        index
    }

    /// Merge the sets holding `a` and `b`, returning the surviving root.
    fn union(&mut self, a: usize, b: usize) -> usize {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.parent[b] = a;
        }
        a
    }
}

/// Whether `a` is a strict descendant of `b`, answered from `ancestry` when it
/// could be built and from the commit graph otherwise.
fn descends(repo: &Repository, ancestry: Option<&TipAncestry>, a: Oid, b: Oid) -> Result<bool> {
    match ancestry {
        Some(ancestry) => Ok(ancestry.is_descendant(a, b)),
        None => is_descendant(repo, a, b),
    }
}

/// The deepest branch in `all_branches` that `branch` descends from, or
/// `merge_base` when it descends from none of them.
fn nearest_parent_in_stack(
    repo: &Repository,
    ancestry: Option<&TipAncestry>,
    branch: &StackBranch,
    all_branches: &[StackBranch],
    merge_base: Oid,
) -> Result<Oid> {
    let mut best_parent = merge_base;
    for b in all_branches {
        if b.name == branch.name || b.id == branch.id {
            continue;
        }
        if !descends(repo, ancestry, branch.id, b.id)? {
            continue;
        }
        if best_parent == merge_base || descends(repo, ancestry, b.id, best_parent)? {
            best_parent = b.id;
        }
    }
    Ok(best_parent)
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

    let ancestry = TipAncestry::for_branches(repo, all_branches, merge_base)?;
    nearest_parent_in_stack(repo, ancestry.as_ref(), branch, all_branches, merge_base)
}

/// [`find_parent_in_stack`] for every branch at once, sharing a single ancestry
/// walk instead of repeating one per branch.
pub fn find_parents_in_stack(
    repo: &Repository,
    all_branches: &[StackBranch],
    merge_base: Oid,
) -> Result<HashMap<String, Oid>> {
    let ancestry = TipAncestry::for_branches(repo, all_branches, merge_base)?;
    let mut parents = HashMap::new();
    for branch in all_branches {
        let parent =
            nearest_parent_in_stack(repo, ancestry.as_ref(), branch, all_branches, merge_base)?;
        parents.insert(branch.name.clone(), parent);
    }
    Ok(parents)
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

    // Ordering needs the ancestry of every pair, so precompute it in one walk.
    // That walk has to be bounded by a commit every tip descends from, which is
    // what merge_base_octopus returns. merge_base_many is not interchangeable
    // here: it bases the later commits against a hypothetical merge of them, so
    // its result can be a commit some tip does not descend from, which would
    // prune that tip out of the walk. No common ancestor exists for unrelated
    // tips, in which case fall back to asking the graph pair by pair.
    let tips = original.iter().map(|b| b.id).collect::<Vec<_>>();
    let ancestry = TipAncestry::new(repo, &tips)?;

    for (idx, branch) in original.iter().enumerate() {
        for (other_idx, other) in original.iter().enumerate() {
            if idx == other_idx || branch.id == other.id {
                continue;
            }

            if descends(repo, ancestry.as_ref(), branch.id, other.id)? {
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

pub struct TreeSyncPlan {
    pub remaining: Vec<String>,
    pub bases: HashMap<String, String>,
    pub parents: HashMap<String, String>,
    pub merged: Vec<String>,
}

/// Plan a branching sync before any refs move. Each child cuts at its original
/// parent tip, unless sync's integration boundary lies beyond that parent.
pub fn plan_tree_sync(
    repo: &Repository,
    branches: &[StackBranch],
    upstream: &str,
    merge_base: Oid,
) -> Result<TreeSyncPlan> {
    let mut ordered = branches.to_vec();
    sort_branches_topologically(repo, &mut ordered)?;
    let mut remaining = Vec::new();
    let mut bases = HashMap::new();
    let mut parents = HashMap::new();
    let mut merged = HashSet::new();
    for branch in &ordered {
        let boundary = find_sync_boundary(repo, &branch.name, upstream, branches)?;
        merged.extend(boundary.merged_branches);
        let Some(mut base) = boundary.old_base else {
            continue;
        };
        let parent_id = find_parent_in_stack(repo, &branch.name, branches, merge_base)?;
        let parent = ordered
            .iter()
            .find(|p| p.id == parent_id && remaining.contains(&p.name));
        let onto = if let Some(parent) = parent
            && (parent.id == base || repo.graph_descendant_of(parent.id, base)?)
        {
            base = parent.id;
            parent.name.clone()
        } else {
            upstream.to_string()
        };
        bases.insert(branch.name.clone(), base.to_string());
        parents.insert(branch.name.clone(), onto);
        remaining.push(branch.name.clone());
    }
    // A branch retained by any boundary must survive cleanup after the rebases.
    merged.retain(|branch| !bases.contains_key(branch));
    let mut merged: Vec<_> = merged.into_iter().collect();
    merged.sort();
    Ok(TreeSyncPlan {
        remaining,
        bases,
        parents,
        merged,
    })
}

/// Local side branches whose merge base with HEAD lies strictly inside the
/// rewritten range. Descendants of HEAD and branches on HEAD's history are
/// handled by the ordinary restack and update-refs paths respectively.
pub fn branches_forking_from_range(
    repo: &Repository,
    base: Oid,
    head: Oid,
) -> Result<Vec<(StackBranch, Oid)>> {
    let mut branches = Vec::new();
    for entry in repo.branches(Some(git2::BranchType::Local))? {
        let (branch, _) = entry?;
        let Some(name) = branch.name()? else {
            continue;
        };
        let tip = branch.get().peel_to_commit()?.id();
        if tip == head
            || tip == base
            || repo.graph_descendant_of(head, tip)?
            || repo.graph_descendant_of(tip, head)?
            || !repo.graph_descendant_of(tip, base)?
        {
            continue;
        }
        let fork = repo.merge_base(tip, head)?;
        if fork != base {
            branches.push((
                StackBranch {
                    name: name.to_string(),
                    id: tip,
                },
                fork,
            ));
        }
    }
    Ok(branches)
}
