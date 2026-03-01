use crate::commands::find_upstream;
use crate::stack::{StackBranch, get_all_stack_branches, visualize_stack};
use anyhow::{Context, Result, anyhow};
use clap::{Args, Subcommand};
use git2::{Oid, Repository};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
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

#[derive(Serialize, Deserialize)]
struct MoveState {
    original_branch: String,
    target_branch: String,
    /// List of branches remaining to be moved
    remaining_branches: Vec<String>,
    /// branch_name -> original_parent_id_str
    parent_id_map: HashMap<String, String>,
    /// branch_name -> original_parent_name (if it was a branch in the sub-stack)
    parent_name_map: HashMap<String, String>,
}

fn state_path(repo: &Repository) -> PathBuf {
    repo.path().join("gits_move_state.json")
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
            "A move operation is already in progress. Use 'gits move continue' or 'gits move abort'."
        ));
    }

    let upstream_name = find_upstream(repo)?;
    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let upstream_id = upstream_obj.id();
    let head = repo.head()?;
    let head_id = head.peel_to_commit()?.id();

    let current_branch_name = if !repo.head_detached()? {
        head.shorthand().map(|s| s.to_string())
    } else {
        None
    }
    .ok_or_else(|| anyhow!("You must be on a branch to use 'move'"))?;

    let merge_base = repo.merge_base(upstream_id, head_id)?;
    let all_branches_in_stack = get_all_stack_branches(repo, merge_base, &upstream_name)?;

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
        inquire::Select::new("Select target branch to move onto:", branch_names).prompt()?
    } else {
        let visualized = visualize_stack(
            repo,
            merge_base,
            &all_branches_in_stack,
            Some(&current_branch_name),
        )?;

        if visualized.is_empty() {
            println!("No branches found in the stack to move.");
            return Ok(());
        }

        let options: Vec<String> = visualized.iter().map(|v| v.display_name.clone()).collect();
        let selected_display =
            inquire::Select::new("Select target branch to move onto:", options).prompt()?;

        visualized
            .iter()
            .find(|v| v.display_name == selected_display)
            .map(|v| v.name.clone())
            .unwrap()
    };

    // Validate target exists
    repo.revparse_single(&selected_target_name)
        .with_context(|| format!("Target '{}' not found.", selected_target_name))?;

    if selected_target_name == current_branch_name {
        println!("Already on that branch.");
        return Ok(());
    }

    let mut sub_stack = Vec::new();
    collect_descendants(
        repo,
        &current_branch_name,
        &all_branches_in_stack,
        &mut sub_stack,
    )?;

    sub_stack.sort_by(|a, b| {
        if a.id == b.id {
            std::cmp::Ordering::Equal
        } else if repo.graph_descendant_of(a.id, b.id).unwrap_or(false) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Less
        }
    });

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
        }
    }

    let state = MoveState {
        original_branch: current_branch_name,
        target_branch: selected_target_name.clone(),
        remaining_branches: sub_stack
            .into_iter()
            .map(|sb| sb.name)
            .filter(|name| name != &selected_target_name)
            .collect(),
        parent_id_map,
        parent_name_map,
    };

    save_state(repo, &state)?;
    run_move_loop(repo, state)
}

fn continue_move(repo: &Repository) -> Result<()> {
    let state = load_state(repo)?;

    if repo.path().join("rebase-merge").exists() || repo.path().join("rebase-apply").exists() {
        return Err(anyhow!(
            "A git rebase is still in progress. Resolve conflicts and run 'git rebase --continue' first."
        ));
    }

    run_move_loop(repo, state)
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

fn run_move_loop(repo: &Repository, mut state: MoveState) -> Result<()> {
    while !state.remaining_branches.is_empty() {
        let current_name = state.remaining_branches[0].clone();
        let old_parent_id_str = state.parent_id_map.get(&current_name).unwrap();

        let new_base = if current_name == state.original_branch {
            state.target_branch.clone()
        } else {
            match state.parent_name_map.get(&current_name) {
                Some(name) => name.clone(),
                None => old_parent_id_str.clone(),
            }
        };

        println!("Rebasing {} onto {}...", current_name, new_base);
        // Use --no-ff to ensure we always create new commits and maintain the hierarchy properly
        // especially when reordering within the same stack.
        let status = Command::new("git")
            .arg("rebase")
            .arg("--no-ff")
            .arg("--onto")
            .arg(&new_base)
            .arg(old_parent_id_str)
            .arg(&current_name)
            .status()?;

        if status.success() {
            state.remaining_branches.remove(0);
            save_state(repo, &state)?;
        } else {
            // Check if a rebase is in progress (meaning it started but hit conflicts)
            if repo.path().join("rebase-merge").exists()
                || repo.path().join("rebase-apply").exists()
            {
                state.remaining_branches.remove(0);
                save_state(repo, &state)?;
                return Err(anyhow!(
                    "Rebase failed for branch {}. Resolve conflicts and run 'git rebase --continue', then 'gits move continue'.",
                    current_name
                ));
            } else {
                return Err(anyhow!(
                    "Rebase failed for branch {}. It seems to have failed before starting (e.g., dirty working tree). Fix the issue and run 'gits move continue'.",
                    current_name
                ));
            }
        }
    }

    println!(
        "Move completed. Checking out original branch {}...",
        state.original_branch
    );
    let status = Command::new("git")
        .arg("checkout")
        .arg(&state.original_branch)
        .status()?;

    if status.success() {
        fs::remove_file(state_path(repo))?;
    } else {
        return Err(anyhow!(
            "Failed to checkout back to original branch '{}'. State file preserved.",
            state.original_branch
        ));
    }

    Ok(())
}

fn save_state(repo: &Repository, state: &MoveState) -> Result<()> {
    let json = serde_json::to_string_pretty(state)?;
    fs::write(state_path(repo), json)?;
    Ok(())
}

fn load_state(repo: &Repository) -> Result<MoveState> {
    let path = state_path(repo);
    if !path.exists() {
        return Err(anyhow!("No move operation in progress."));
    }
    let json = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&json)?)
}

fn collect_descendants(
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

    for b in all_branches {
        if b.name != root_name
            && repo.graph_descendant_of(b.id, root.id).unwrap_or(false)
            && !result.iter().any(|existing| existing.name == b.name)
        {
            result.push(b.clone());
        }
    }
    Ok(())
}

fn find_parent_in_stack(
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
