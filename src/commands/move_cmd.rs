use crate::commands::find_upstream;
use crate::rebase_utils::{Operation, RebaseState, run_rebase_loop, save_state, state_path};
use crate::stack::{collect_descendants, get_stack_branches_from_merge_base, visualize_stack};
use anyhow::{Context, Result, anyhow};
use clap::Args;
use git2::Repository;

#[derive(Args)]
pub struct MoveArgs {
    /// Target branch to move onto
    #[arg(long)]
    pub onto: Option<String>,
    /// List all local branches instead of just the stack
    #[arg(long)]
    pub all: bool,
    /// Force the move even if branches are checked out in other worktrees
    #[arg(long)]
    pub force: bool,
}

pub fn move_cmd(args: &MoveArgs) -> Result<()> {
    let repo = crate::open_repo()?;
    start_move(&repo, args)
}

fn start_move(repo: &Repository, args: &MoveArgs) -> Result<()> {
    let path = state_path(repo);
    if path.exists() {
        return Err(anyhow!(
            "A move or commit operation is already in progress. Use 'gits continue' or 'gits abort'."
        ));
    }

    let head = repo.head()?;
    let head_id = head.peel_to_commit()?.id();

    let current_branch_name = if !repo.head_detached()? {
        head.shorthand().map(|s| s.to_string())
    } else {
        None
    }
    .ok_or_else(|| anyhow!("You must be on a branch to use 'move'"))?;

    let upstream_name = find_upstream(repo)?;
    if current_branch_name == upstream_name {
        return Err(anyhow!(
            "Branch '{}' is the upstream branch. Cannot move the upstream branch itself.",
            current_branch_name
        ));
    }

    // Determine target branch
    let selected_target_name = if let Some(target) = &args.onto {
        target.clone()
    } else if args.all {
        let mut branch_names = Vec::new();
        let local_branches = repo.branches(Some(git2::BranchType::Local))?;
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

        crate::commands::prompt_select("Select target branch to move onto:", branch_names)?
    } else {
        // Only here we MUST have an upstream
        let upstream_name = find_upstream(repo)?;
        let upstream_obj = repo.revparse_single(&upstream_name)?;
        let upstream_id = upstream_obj.id();
        let merge_base = repo.merge_base(upstream_id, head_id)?;
        let all_branches_in_stack = get_stack_branches_from_merge_base(
            repo,
            merge_base,
            head_id,
            upstream_id,
            &upstream_name,
        )?;

        let visualized = visualize_stack(repo, &all_branches_in_stack, Some(&current_branch_name))?;

        if visualized.is_empty() {
            println!("No branches found in the stack to move. Use --all to see everything.");
            return Ok(());
        }

        let options: Vec<String> = visualized.iter().map(|v| v.display_name.clone()).collect();
        let selected_display =
            crate::commands::prompt_select("Select target branch to move onto:", options)?;

        visualized
            .iter()
            .find(|v| v.display_name == selected_display)
            .map(|v| v.name.clone())
            .ok_or_else(|| anyhow!("Failed to find selected branch '{}'", selected_display))?
    };

    // Validate target exists
    repo.revparse_single(&selected_target_name)
        .with_context(|| format!("Target '{}' not found.", selected_target_name))?;

    if selected_target_name == current_branch_name {
        println!("Already on that branch.");
        return Ok(());
    }

    // Now we need the stack info to perform the rebase.
    // Even if we used --all to pick the target, we still need find_upstream to know the sub-stack.
    let upstream_name = find_upstream(repo)?;
    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let upstream_id = upstream_obj.id();
    let merge_base = repo.merge_base(upstream_id, head_id)?;
    let all_branches_in_stack =
        get_stack_branches_from_merge_base(repo, merge_base, head_id, upstream_id, &upstream_name)?;

    let mut sub_stack = Vec::new();
    collect_descendants(
        repo,
        &current_branch_name,
        &all_branches_in_stack,
        &mut sub_stack,
    )?;

    // Reject move target if it lies inside the subtree being moved
    if sub_stack.iter().any(|b| b.name == selected_target_name) {
        return Err(anyhow!(
            "Target branch '{}' is inside the subtree being moved.",
            selected_target_name
        ));
    }

    crate::stack::sort_branches_topologically(repo, &mut sub_stack)?;

    let remaining_branches: Vec<String> = sub_stack
        .iter()
        .map(|sb| sb.name.clone())
        .filter(|name| name != &selected_target_name)
        .collect();

    crate::rebase_utils::check_worktrees(&remaining_branches, args.force)?;

    let (parent_id_map, parent_name_map) = crate::stack::build_parent_maps(
        repo,
        &sub_stack,
        &all_branches_in_stack,
        merge_base,
        head_id,
        &current_branch_name,
    )?;

    let state = RebaseState {
        operation: Operation::Move,
        original_branch: current_branch_name,
        target_branch: selected_target_name.clone(),
        caller_branch: None,
        remaining_branches,
        in_progress_branch: None,
        parent_id_map,
        parent_name_map,
        stash_ref: None,
        unstage_on_restore: false,
    };

    save_state(repo, &state)?;
    run_rebase_loop(repo, state)
}
