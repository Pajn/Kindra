use anyhow::{Result, anyhow};
use git2::{BranchType, Oid, Repository};

#[derive(Clone)]
pub struct StackBranch {
    pub name: String,
    pub id: Oid,
}

pub struct VisualizedBranch {
    pub name: String,
    pub display_name: String,
}

pub fn get_stack_branches(
    repo: &Repository,
    head_id: Oid,
    upstream_id: Oid,
    upstream_name: &str,
) -> Result<Vec<StackBranch>> {
    let mut stack_branches = Vec::new();
    let local_branches = repo.branches(Some(BranchType::Local))?;

    for res in local_branches {
        let (branch, _) = res?;
        let name = branch
            .name()?
            .ok_or_else(|| anyhow!("Branch has no name"))?
            .to_string();

        if name == upstream_name {
            continue;
        }

        if let Some(target_id) = branch.get().target() {
            let is_linear = repo.graph_descendant_of(head_id, target_id)?
                || repo.graph_descendant_of(target_id, head_id)?
                || head_id == target_id;

            let is_merged = repo.graph_descendant_of(upstream_id, target_id)?;

            if is_linear && !is_merged {
                stack_branches.push(StackBranch {
                    name,
                    id: target_id,
                });
            }
        }
    }

    Ok(stack_branches)
}

pub fn get_all_stack_branches(
    repo: &Repository,
    merge_base: Oid,
    upstream_name: &str,
) -> Result<Vec<StackBranch>> {
    let mut stack_branches = Vec::new();
    let local_branches = repo.branches(Some(BranchType::Local))?;

    for res in local_branches {
        let (branch, _) = res?;
        let name = branch
            .name()?
            .ok_or_else(|| anyhow!("Branch has no name"))?
            .to_string();

        if name == upstream_name {
            continue;
        }

        if let Some(target_id) = branch.get().target() {
            let is_descendant =
                repo.graph_descendant_of(target_id, merge_base)? || target_id == merge_base;
            let is_merged =
                repo.graph_descendant_of(merge_base, target_id)? && target_id != merge_base;

            if is_descendant && !is_merged {
                stack_branches.push(StackBranch {
                    name,
                    id: target_id,
                });
            }
        }
    }

    Ok(stack_branches)
}

pub fn visualize_stack(
    repo: &Repository,
    base_id: Oid,
    stack_branches: &[StackBranch],
    current_branch_name: Option<&str>,
) -> Result<Vec<VisualizedBranch>> {
    let mut result = Vec::new();
    visualize_recursive(
        repo,
        base_id,
        stack_branches,
        0,
        &mut result,
        current_branch_name,
    )?;
    Ok(result)
}

fn visualize_recursive(
    repo: &Repository,
    parent_id: Oid,
    stack_branches: &[StackBranch],
    depth: usize,
    result: &mut Vec<VisualizedBranch>,
    current_branch_name: Option<&str>,
) -> Result<()> {
    let mut successors = get_immediate_successors(repo, parent_id, stack_branches)?;
    successors.sort(); // Sort siblings alphabetically

    for name in successors {
        let branch = stack_branches
            .iter()
            .find(|b| b.name == name)
            .ok_or_else(|| anyhow!("Branch '{}' not found in stack branches", name))?;
        let indent = "  ".repeat(depth);
        let marker = if Some(name.as_str()) == current_branch_name {
            "* "
        } else {
            "  "
        };

        result.push(VisualizedBranch {
            name: name.clone(),
            display_name: format!("{}{}{}", indent, marker, name),
        });

        visualize_recursive(
            repo,
            branch.id,
            stack_branches,
            depth + 1,
            result,
            current_branch_name,
        )?;
    }

    Ok(())
}

pub fn get_immediate_successors(
    repo: &Repository,
    current_id: Oid,
    stack_branches: &[StackBranch],
) -> Result<Vec<String>> {
    let mut successors = Vec::new();

    let candidates: Vec<&StackBranch> = stack_branches
        .iter()
        .filter(|b| {
            b.id != current_id
                && (repo.graph_descendant_of(b.id, current_id).unwrap_or(false)
                    || current_id.is_zero())
        })
        .collect();

    for candidate in &candidates {
        let is_immediate = !candidates.iter().any(|other| {
            other.id != candidate.id
                && repo
                    .graph_descendant_of(candidate.id, other.id)
                    .unwrap_or(false)
        });

        if is_immediate && !successors.contains(&candidate.name) {
            successors.push(candidate.name.clone());
        }
    }

    Ok(successors)
}

pub fn get_stack_tips(repo: &Repository, stack_branches: &[StackBranch]) -> Result<Vec<String>> {
    let mut tips = Vec::new();

    for branch in stack_branches {
        let has_descendant = stack_branches.iter().any(|other| {
            other.id != branch.id
                && repo
                    .graph_descendant_of(other.id, branch.id)
                    .unwrap_or(false)
        });

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
            && repo.graph_descendant_of(b.id, root_id).unwrap_or(false)
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
            && (repo.graph_descendant_of(branch.id, b.id).unwrap_or(false) || branch.id == b.id)
        {
            if b.id == branch.id {
                continue;
            }
            if best_parent == merge_base
                || repo.graph_descendant_of(b.id, best_parent).unwrap_or(false)
            {
                best_parent = b.id;
            }
        }
    }
    Ok(best_parent)
}

pub fn sort_branches_topologically(repo: &Repository, branches: &mut [StackBranch]) {
    branches.sort_by(|a, b| {
        use std::cmp::Ordering;
        if a.id == b.id {
            return Ordering::Equal;
        }
        let a_desc_b = repo.graph_descendant_of(a.id, b.id).unwrap_or(false);
        let b_desc_a = repo.graph_descendant_of(b.id, a.id).unwrap_or(false);
        match (a_desc_b, b_desc_a) {
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            _ => a.name.cmp(&b.name),
        }
    });
}
