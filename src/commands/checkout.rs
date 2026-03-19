use super::CheckoutSubcommand;
use super::find_upstream;
use crate::stack::{get_immediate_successors, get_stack_tips, visualize_stack};
use anyhow::{Result, anyhow};
use git2::BranchType;
use std::process::Command;

pub fn checkout(subcommand: &Option<CheckoutSubcommand>, all: bool) -> Result<()> {
    let repo = crate::open_repo()?;

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

    let upstream_name = find_upstream(&repo)?.ok_or_else(|| {
        anyhow!("Could not find a base branch (init.defaultBranch, main, master, or trunk)")
    })?;
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
            let current_name = current_branch_name.ok_or_else(|| anyhow!("Not on a branch"))?;
            let parent_branches =
                find_first_parent_branches_via_git_log(&repo, &upstream_name, &current_name)?;

            match parent_branches.len() {
                0 => perform_git_checkout(&upstream_name),
                1 => perform_git_checkout(&parent_branches[0]),
                _ => {
                    let selected = crate::commands::prompt_select(
                        "Multiple parent branches found. Select one:",
                        parent_branches,
                    )?;
                    perform_git_checkout(&selected)
                }
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

fn find_first_parent_branches_via_git_log(
    repo: &git2::Repository,
    upstream_name: &str,
    current_branch: &str,
) -> Result<Vec<String>> {
    let repo_root = if let Some(workdir) = repo.workdir() {
        workdir.to_path_buf()
    } else {
        repo.path()
            .parent()
            .ok_or_else(|| anyhow!("Failed to resolve repository root path."))?
            .to_path_buf()
    };

    let output = Command::new("git")
        .args([
            "log",
            "--first-parent",
            "--decorate=full",
            "--format=%H%x00%D",
            "HEAD",
            &format!("^{upstream_name}"),
        ])
        .current_dir(repo_root)
        .output()?;

    if !output.status.success() {
        return Err(anyhow!(
            "Failed to inspect first-parent ancestry via git log"
        ));
    }

    let stdout = String::from_utf8(output.stdout)?;
    let mut lines = stdout.lines();
    let _ = lines.next();

    for line in lines {
        let mut parts = line.splitn(2, '\0');
        let _commit = parts.next();
        let decorations = parts.next().unwrap_or("");
        let mut names = Vec::new();

        for token in decorations.split(',') {
            let item = token.trim();
            if item.is_empty() {
                continue;
            }

            let maybe_ref = if let Some(rest) = item.strip_prefix("HEAD -> ") {
                rest.trim()
            } else {
                item
            };

            if let Some(local) = maybe_ref.strip_prefix("refs/heads/")
                && local != current_branch
                && local != upstream_name
            {
                names.push(local.to_string());
            }
        }

        if !names.is_empty() {
            names.sort();
            names.dedup();
            return Ok(names);
        }
    }

    Ok(Vec::new())
}
