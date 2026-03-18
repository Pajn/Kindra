use crate::commands::{find_upstream, resolve_rebase_autostash};
use crate::rebase_utils::{Operation, RebaseState, run_rebase_loop, save_state, state_path};
use anyhow::{Result, anyhow};
use clap::Args;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use tempfile::NamedTempFile;

#[derive(Args)]
pub struct ReorderArgs {
    /// Force the reorder even if branches are checked out in other worktrees
    #[arg(long)]
    pub force: bool,
    /// Allow git rebase to autostash tracked worktree changes
    #[arg(long, overrides_with = "no_autostash")]
    pub autostash: bool,
    /// Disable git rebase autostash even if configured
    #[arg(long, overrides_with = "autostash")]
    pub no_autostash: bool,
}

pub fn reorder(args: &ReorderArgs) -> Result<()> {
    let repo = crate::open_repo()?;
    if state_path(&repo).exists() {
        return Err(anyhow!(
            "A gits operation is already in progress. Use 'gits continue' or 'gits abort'."
        ));
    }

    let head = repo.head()?;
    let current_branch_name = head
        .shorthand()
        .ok_or_else(|| anyhow!("You must be on a branch to use 'reorder'"))?
        .to_string();

    let upstream_name = find_upstream(&repo)?;
    if current_branch_name == upstream_name {
        return Err(anyhow!(
            "Branch '{}' is the upstream branch. Cannot reorder the upstream branch itself.",
            current_branch_name
        ));
    }

    let head_id = head.peel_to_commit()?.id();
    let upstream_id = repo.revparse_single(&upstream_name)?.id();
    let merge_base = repo.merge_base(upstream_id, head_id)?;

    let stack_component = crate::stack::collect_stack_component(
        &repo,
        &current_branch_name,
        merge_base,
        upstream_id,
        &upstream_name,
    )?;
    if stack_component.is_empty() {
        println!("No branches found in the current stack.");
        return Ok(());
    }

    let current_parent_map =
        crate::stack::current_parent_name_map(&repo, &stack_component, merge_base, &upstream_name)?;
    let edited_parent_map = edit_parent_map(
        &stack_component,
        &current_parent_map,
        &upstream_name,
        &current_branch_name,
    )?;

    if edited_parent_map == current_parent_map {
        println!("No reorder changes.");
        return Ok(());
    }

    let plan = crate::stack::plan_graph_reorder(
        &repo,
        &stack_component,
        merge_base,
        &upstream_name,
        &edited_parent_map,
    )?;

    let autostash = resolve_rebase_autostash(
        &repo,
        if args.autostash {
            Some(true)
        } else if args.no_autostash {
            Some(false)
        } else {
            None
        },
    )?;

    crate::rebase_utils::check_worktrees(&plan.remaining_branches, args.force)?;

    let original_commit_count_map = stack_component
        .iter()
        .map(|branch| {
            let parent_id = plan
                .parent_id_map
                .get(&branch.name)
                .ok_or_else(|| anyhow!("Missing parent id for '{}'.", branch.name))?;
            let chain = crate::stack::collect_first_parent_chain(
                &repo,
                git2::Oid::from_str(parent_id)?,
                branch.id,
            )?;
            Ok((branch.name.clone(), chain.len()))
        })
        .collect::<Result<HashMap<_, _>>>()?;

    let state = RebaseState {
        operation: Operation::Reorder,
        original_branch: current_branch_name,
        target_branch: upstream_name,
        caller_branch: None,
        remaining_branches: plan.remaining_branches,
        in_progress_branch: None,
        parent_id_map: plan.parent_id_map,
        parent_name_map: current_parent_map,
        new_base_map: plan.new_base_map,
        original_commit_count_map,
        stash_ref: None,
        unstage_on_restore: false,
        autostash,
    };

    save_state(&repo, &state)?;
    run_rebase_loop(&repo, state)
}

fn edit_parent_map(
    branches: &[crate::stack::StackBranch],
    current_parent_map: &HashMap<String, String>,
    upstream_name: &str,
    current_branch_name: &str,
) -> Result<HashMap<String, String>> {
    let mut buffer = String::new();
    let mut previous_branch_name: Option<&str> = None;
    for branch in branches {
        let parent = current_parent_map
            .get(&branch.name)
            .ok_or_else(|| anyhow!("Missing current parent for '{}'.", branch.name))?;
        let uses_shorthand = previous_branch_name == Some(parent.as_str());
        let line = if uses_shorthand {
            format!("branch {}", branch.name)
        } else {
            format!("branch {} parent {}", branch.name, parent)
        };

        if branch.name == current_branch_name {
            buffer.push_str(&format!("{line}  # current\n"));
        } else {
            buffer.push_str(&format!("{line}\n"));
        }

        previous_branch_name = Some(&branch.name);
    }

    buffer.push_str("\n# gits reorder\n");
    buffer.push_str("# Edit only the parent target for each branch.\n");
    buffer.push_str("# Keep exactly one row per branch.\n");
    buffer.push_str(&format!(
        "# Parent targets must be another listed branch or '{}'.\n",
        upstream_name
    ));
    buffer.push_str("# 'branch <name>' means the branch above it is the parent.\n");
    buffer.push_str("# Forks are created by assigning multiple branches the same parent.\n");
    buffer.push_str("# Cycles and self-parenting are not allowed.\n");

    let mut temp_file = NamedTempFile::new()?;
    temp_file.write_all(buffer.as_bytes())?;
    let temp_path = temp_file.path().to_path_buf();

    crate::editor::launch_editor(&temp_path)?;
    let edited_buffer = std::fs::read_to_string(&temp_path)?;
    parse_parent_map(&edited_buffer, branches, upstream_name)
}

fn parse_parent_map(
    edited_buffer: &str,
    branches: &[crate::stack::StackBranch],
    upstream_name: &str,
) -> Result<HashMap<String, String>> {
    let expected_names = branches
        .iter()
        .map(|branch| branch.name.clone())
        .collect::<HashSet<_>>();
    let mut parent_map = HashMap::new();
    let mut previous_branch_name: Option<String> = None;

    for raw_line in edited_buffer.lines() {
        let line = raw_line
            .split('#')
            .next()
            .map(str::trim)
            .unwrap_or_default();
        if line.is_empty() {
            continue;
        }

        let parts = line.split_whitespace().collect::<Vec<_>>();
        let (branch_name, parent_name) = match parts.as_slice() {
            ["branch", branch_name] => {
                let parent_name = previous_branch_name.clone().ok_or_else(|| {
                    anyhow!(
                        "Invalid reorder line '{}'. The first branch row must spell out its parent.",
                        raw_line.trim()
                    )
                })?;
                ((*branch_name).to_string(), parent_name)
            }
            ["branch", branch_name, "parent", parent_name] => {
                ((*branch_name).to_string(), (*parent_name).to_string())
            }
            _ => {
                return Err(anyhow!(
                    "Invalid reorder line '{}'. Expected format: branch <name> [parent <parent>].",
                    raw_line.trim()
                ));
            }
        };

        if !expected_names.contains(&branch_name) {
            return Err(anyhow!(
                "Branch '{}' is not part of the current stack component.",
                branch_name
            ));
        }
        if parent_name != upstream_name && !expected_names.contains(&parent_name) {
            return Err(anyhow!(
                "Branch '{}' has unknown parent '{}'.",
                branch_name,
                parent_name
            ));
        }
        if parent_map
            .insert(branch_name.clone(), parent_name)
            .is_some()
        {
            return Err(anyhow!("Duplicate branch row for '{}'.", branch_name));
        }
        previous_branch_name = Some(branch_name);
    }

    if parent_map.len() != expected_names.len() {
        let missing = expected_names
            .iter()
            .filter(|name| !parent_map.contains_key(*name))
            .cloned()
            .collect::<Vec<_>>();
        return Err(anyhow!(
            "Edited reorder graph is missing branch rows for: {}",
            missing.join(", ")
        ));
    }

    Ok(parent_map)
}
