use crate::commands::{resolve_rebase_autostash, resolve_restack_history_limit};
use crate::rebase_utils::{Operation, RebaseState, run_rebase_loop, state_path};
use anyhow::{Result, anyhow};
use clap::Args;
use git2::{BranchType, Commit, Oid, Repository};
use std::collections::HashMap;

#[derive(Args)]
pub struct RestackArgs {
    /// Maximum first-parent history depth to scan when detecting floating branches (0 = unbounded)
    #[arg(long)]
    pub history_limit: Option<usize>,
    /// Allow git rebase to autostash tracked worktree changes
    #[arg(long, overrides_with = "no_autostash")]
    pub autostash: bool,
    /// Disable git rebase autostash even if configured
    #[arg(long, overrides_with = "autostash")]
    pub no_autostash: bool,
}

pub fn restack(args: &RestackArgs) -> Result<()> {
    let repo = crate::open_repo()?;

    if state_path(&repo).exists() {
        return Err(anyhow!("A rebase operation is already in progress."));
    }

    let head = repo.head()?;
    let current_branch_name = head
        .shorthand()
        .ok_or_else(|| anyhow!("Detached HEAD"))?
        .to_string();
    let head_commit = head.peel_to_commit()?;

    println!(
        "Finding branches to restack onto '{}'...",
        current_branch_name
    );

    let history_limit = resolve_restack_history_limit(&repo, args.history_limit)?;
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
    let children =
        find_floating_children(&repo, &head_commit, &current_branch_name, history_limit)?;

    if children.is_empty() {
        println!("No floating children found.");
        return Ok(());
    }

    // Construct RebaseState
    let mut parent_id_map = HashMap::new();
    let mut parent_name_map = HashMap::new();
    let mut remaining = Vec::new();

    for (name, old_base) in children {
        println!(" - {} (matches old base {})", name, old_base);
        remaining.push(name.clone());
        parent_id_map.insert(name.clone(), old_base.to_string());
        parent_name_map.insert(name.clone(), current_branch_name.clone());
    }

    let state = RebaseState {
        operation: Operation::Move,
        original_branch: current_branch_name.clone(),
        target_branch: current_branch_name.clone(),
        caller_branch: Some(current_branch_name.clone()),
        remaining_branches: remaining,
        in_progress_branch: None,
        parent_id_map,
        parent_name_map,
        new_base_map: HashMap::new(),
        original_commit_count_map: HashMap::new(),
        original_tip_map,
        stash_ref: None,
        unstage_on_restore: false,
        autostash,
    };

    crate::rebase_utils::save_state(&repo, &state)?;
    run_rebase_loop(&repo, state)?;

    Ok(())
}

fn find_floating_children(
    repo: &Repository,
    head_commit: &Commit,
    current_branch: &str,
    history_limit: usize,
) -> Result<Vec<(String, Oid)>> {
    let mut results = Vec::new();
    let mut patch_id_cache = HashMap::new();
    let target = crate::stack::build_floating_target_context(
        repo,
        head_commit,
        current_branch,
        history_limit,
        &mut patch_id_cache,
    )?;
    let branches = repo.branches(Some(BranchType::Local))?;

    for branch_res in branches {
        let (branch, _) = branch_res?;
        let name = match branch.name() {
            Ok(Some(n)) => n.to_string(),
            _ => continue,
        };

        if name == current_branch {
            continue;
        }

        let tip = match branch.get().target() {
            Some(t) => t,
            None => continue,
        };

        if let Some(old_base) = crate::stack::find_floating_base(
            repo,
            tip,
            &target,
            history_limit,
            &mut patch_id_cache,
        )? {
            results.push((name, old_base));
        }
    }
    Ok(results)
}
