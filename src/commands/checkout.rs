use super::CheckoutSubcommand;
use super::find_upstream;
use crate::stack::{get_immediate_successors, get_stack_branches, get_stack_tips, visualize_stack};
use anyhow::{Context, Result, anyhow};
use git2::{BranchType, Repository};
use std::process::Command;

pub fn checkout(subcommand: &Option<CheckoutSubcommand>, all: bool) -> Result<()> {
    let repo = Repository::open(".").context("Failed to open git repository.")?;

    if all && subcommand.is_none() {
        let mut branch_names = Vec::new();
        let local_branches = repo.branches(Some(BranchType::Local))?;
        for res in local_branches {
            let (branch, _) = res?;
            if let Some(name) = branch.name()? {
                branch_names.push(name.to_string());
            }
        }
        branch_names.sort();

        if branch_names.is_empty() {
            println!("No local branches found.");
            return Ok(());
        }

        let selected_name =
            crate::commands::prompt_select("Select branch to checkout:", branch_names)?;
        return perform_git_checkout(&selected_name);
    }

    let upstream_name = find_upstream(&repo)?;
    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let upstream_id = upstream_obj.id();
    let head = repo.head()?;
    let head_id = head.peel_to_commit()?.id();

    let current_branch_name = if !repo.head_detached()? {
        head.shorthand().map(|s| s.to_string())
    } else {
        None
    };

    match subcommand {
        Some(CheckoutSubcommand::Up) => {
            let merge_base = repo.merge_base(upstream_id, head_id)?;
            let branches = crate::stack::get_stack_branches_from_merge_base(
                &repo,
                merge_base,
                head_id,
                upstream_id,
                &upstream_name,
            )?;
            let mut successors = get_immediate_successors(&repo, head_id, &branches)?;
            successors.sort();

            match successors.len() {
                0 => Err(anyhow!("Already at the top of the stack")),
                1 => perform_git_checkout(&successors[0]),
                _ => {
                    let selected = crate::commands::prompt_select(
                        "Multiple branches ahead. Select one:",
                        successors,
                    )?;
                    perform_git_checkout(&selected)
                }
            }
        }
        Some(CheckoutSubcommand::Down) => {
            let mut branches = get_stack_branches(&repo, head_id, upstream_id, &upstream_name)?;
            crate::stack::sort_branches_topologically(&repo, &mut branches)?;

            let current_name = current_branch_name.ok_or_else(|| anyhow!("Not on a branch"))?;
            let idx = branches
                .iter()
                .position(|b| b.name == current_name)
                .ok_or_else(|| anyhow!("Current branch '{}' not in stack", current_name))?;

            if idx > 0 {
                perform_git_checkout(&branches[idx - 1].name)
            } else {
                perform_git_checkout(&upstream_name)
            }
        }
        Some(CheckoutSubcommand::Top) => {
            let merge_base = repo.merge_base(upstream_id, head_id)?;
            let branches = crate::stack::get_stack_branches_from_merge_base(
                &repo,
                merge_base,
                head_id,
                upstream_id,
                &upstream_name,
            )?;
            let mut tips = get_stack_tips(&repo, &branches)?;
            tips.sort();
            match tips.len() {
                0 => Err(anyhow!("No branches in stack")),
                1 => perform_git_checkout(&tips[0]),
                _ => {
                    let selected = crate::commands::prompt_select(
                        "Multiple stack tips found. Select one:",
                        tips,
                    )?;
                    perform_git_checkout(&selected)
                }
            }
        }
        None => {
            let merge_base = repo.merge_base(upstream_id, head_id)?;
            let all_branches = crate::stack::get_stack_branches_from_merge_base(
                &repo,
                merge_base,
                head_id,
                upstream_id,
                &upstream_name,
            )?;

            let visualized = visualize_stack(&repo, &all_branches, current_branch_name.as_deref())?;

            if visualized.is_empty() {
                println!(
                    "No branches found in the current stack (excluding {}). Use --all to see everything.",
                    upstream_name
                );
                return Ok(());
            }

            let options: Vec<String> = visualized.iter().map(|v| v.display_name.clone()).collect();
            let selected_display =
                crate::commands::prompt_select("Select branch to checkout:", options)?;

            let selected_name = visualized
                .iter()
                .find(|v| v.display_name == selected_display)
                .map(|v| v.name.clone())
                .ok_or_else(|| anyhow!("Failed to find selected branch '{}'", selected_display))?;

            perform_git_checkout(&selected_name)
        }
    }
}

fn perform_git_checkout(name: &str) -> Result<()> {
    let status = Command::new("git").arg("checkout").arg(name).status()?;

    if !status.success() {
        return Err(anyhow!("git checkout failed"));
    }

    Ok(())
}
