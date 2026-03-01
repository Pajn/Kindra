use crate::commands::find_upstream;
use crate::rebase_utils::{RebaseState, load_state, run_rebase_loop, save_state, state_path};
use crate::stack::{
    collect_descendants, find_parent_in_stack, get_stack_branches_from_merge_base, visualize_stack,
};
use anyhow::{Context, Result, anyhow};
use clap::{Args, Subcommand};
use git2::Repository;
use std::collections::HashMap;
use std::fs;
use std::process::Command;

#[derive(Args)]
pub struct MoveArgs {
    #[command(subcommand)]
    pub subcommand: Option<MoveSubcommand>,
    /// Target branch to move onto
    #[arg(long)]
    pub onto: Option<String>,
    /// List all local branches instead of just the stack
    #[arg(long)]
    pub all: bool,
}

#[derive(Subcommand)]
pub enum MoveSubcommand {
    /// Continue an in-progress move after resolving conflicts
    Continue,
    /// Abort an in-progress move
    Abort,
    /// Show the status of an in-progress move
    Status,
}

pub fn move_cmd(args: &MoveArgs) -> Result<()> {
    let repo = Repository::open(".").context("Failed to open git repository.")?;

    match &args.subcommand {
        Some(MoveSubcommand::Continue) => continue_move(&repo),
        Some(MoveSubcommand::Abort) => abort_move(&repo),
        Some(MoveSubcommand::Status) => show_status(&repo),
        None => start_move(&repo, args),
    }
}

fn start_move(repo: &Repository, args: &MoveArgs) -> Result<()> {
    let path = state_path(repo);
    if path.exists() {
        return Err(anyhow!(
            "A move or commit operation is already in progress. Use 'gits move continue' or 'gits move abort'."
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
        let all_branches_in_stack =
            get_stack_branches_from_merge_base(repo, merge_base, &upstream_name)?;

        let visualized = visualize_stack(
            repo,
            merge_base,
            &all_branches_in_stack,
            Some(&current_branch_name),
        )?;

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
        get_stack_branches_from_merge_base(repo, merge_base, &upstream_name)?;

    let mut sub_stack = Vec::new();
    collect_descendants(
        repo,
        &current_branch_name,
        &all_branches_in_stack,
        &mut sub_stack,
    )?;

    crate::stack::sort_branches_topologically(repo, &mut sub_stack);

    let mut parent_id_map = HashMap::new();
    let mut parent_name_map = HashMap::new();
    for sb in &sub_stack {
        let parent_id = find_parent_in_stack(repo, &sb.name, &all_branches_in_stack, merge_base)?;
        parent_id_map.insert(sb.name.clone(), parent_id.to_string());

        if let Some(parent_branch) = sub_stack
            .iter()
            .find(|p| p.id == parent_id && p.name != sb.name)
        {
            parent_name_map.insert(sb.name.clone(), parent_branch.name.clone());
        } else if sb.name == current_branch_name {
            // The root of our move rebases onto the selected target
            parent_name_map.insert(sb.name.clone(), selected_target_name.clone());
        }
    }

    let state = RebaseState {
        original_branch: current_branch_name,
        target_branch: selected_target_name.clone(),
        remaining_branches: sub_stack
            .into_iter()
            .map(|sb| sb.name)
            .filter(|name| name != &selected_target_name)
            .collect(),
        in_progress_branch: None,
        parent_id_map,
        parent_name_map,
    };

    save_state(repo, &state)?;
    run_rebase_loop(repo, state)
}

fn continue_move(repo: &Repository) -> Result<()> {
    let state = load_state(repo)?;

    if repo.path().join("rebase-merge").exists() || repo.path().join("rebase-apply").exists() {
        return Err(anyhow!(
            "A git rebase is still in progress. Resolve conflicts and run 'git rebase --continue' first."
        ));
    }

    run_rebase_loop(repo, state)
}

fn abort_move(repo: &Repository) -> Result<()> {
    let path = state_path(repo);
    if path.exists() {
        // Only try to abort a git rebase if we were actually in a gits move
        if repo.path().join("rebase-merge").exists() || repo.path().join("rebase-apply").exists() {
            println!("Aborting active git rebase...");
            let status = Command::new("git").arg("rebase").arg("--abort").status()?;
            if !status.success() {
                return Err(anyhow!("Failed to abort git rebase."));
            }
        }

        fs::remove_file(path)?;
        println!("Move operation aborted (state cleared).");
    } else {
        println!("No move operation in progress.");
    }

    Ok(())
}

fn show_status(repo: &Repository) -> Result<()> {
    let state = load_state(repo)?;
    println!(
        "Move in progress: {} onto {}",
        state.original_branch, state.target_branch
    );
    println!(
        "Remaining branches: {}",
        state.remaining_branches.join(", ")
    );
    Ok(())
}
