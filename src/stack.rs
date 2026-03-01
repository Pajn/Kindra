use crate::commands::find_upstream;
use anyhow::{Result, anyhow};
use git2::{Oid, Repository};

#[derive(Clone, Debug)]
pub struct StackBranch {
    pub name: String,
    pub id: Oid,
}

pub fn get_stack_branches(
    repo: &Repository,
    head_id: Oid,
    upstream_id: Oid,
    upstream_name: &str,
) -> Result<Vec<StackBranch>> {
    let mut branches = Vec::new();
    let local_branches = repo.branches(Some(git2::BranchType::Local))?;

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

        if (repo.graph_descendant_of(id, upstream_id)? || id == upstream_id)
            && (repo.graph_descendant_of(head_id, id)? || head_id == id)
        {
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
    upstream_name: &str,
) -> Result<Vec<StackBranch>> {
    let mut branches = Vec::new();
    let local_branches = repo.branches(Some(git2::BranchType::Local))?;

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

        if repo.graph_descendant_of(id, merge_base)? || id == merge_base {
            branches.push(StackBranch {
                name: name.to_string(),
                id,
            });
        }
    }

    Ok(branches)
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
            && (repo.graph_descendant_of(b.id, current_id)? || current_id.is_zero())
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

#[derive(Clone)]
pub struct VisualBranch {
    pub name: String,
    pub display_name: String,
}

pub fn visualize_stack(
    repo: &Repository,
    merge_base: Oid,
    all_branches: &[StackBranch],
    current_branch_name: Option<&str>,
) -> Result<Vec<VisualBranch>> {
    let mut result = Vec::new();
    let _ = find_upstream(repo)?;

    let mut revwalk = repo.revwalk()?;
    revwalk.push_range(&format!("{}..HEAD", merge_base))?;
    revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;

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
