use crate::commands::{prompt_multi_select, resolve_restack_history_limit};
use crate::rebase_utils::{
    Operation, RebaseState, passively_reconcile_rebase_state, run_rebase_loop,
};
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
    /// Show interactive picker to select which branches to restack
    #[arg(long)]
    pub pick: bool,
}

pub fn restack(args: &RestackArgs) -> Result<()> {
    let repo = crate::open_repo()?;
    let _lock = crate::state_io::RepoLock::acquire(&repo)?;

    if passively_reconcile_rebase_state(&repo)? || crate::commands::run::run_state_exists(&repo) {
        return Err(anyhow!(
            "A Kindra-managed operation is already in progress. Use 'kin continue' or 'kin abort'."
        ));
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
    let children =
        find_floating_children(&repo, &head_commit, &current_branch_name, history_limit)?;

    if children.is_empty() {
        println!("No floating children found.");
        return Ok(());
    }

    // Now that there is real work to do, enforce the clean-or-autostash contract
    // before the (possibly interactive) `--pick` prompt, so a dirty tree with
    // `--no-autostash` fails fast rather than after the user makes a selection.
    // (Kept after the no-op check above so a restack with nothing to do still
    // succeeds on a dirty tree, matching its previous behavior.)
    let autostash =
        crate::commands::resolve_and_check_autostash(&repo, args.autostash, args.no_autostash)?;

    let children = if args.pick {
        // prompt_multi_select resolves the selection per mode: interactive shows
        // the picker, scripted uses the seeded selection, and non-interactive
        // (no terminal, no script) is a hard error rather than a silent no-op —
        // `--pick` inherently needs a chooser, so failing loudly (input-required
        // exit code) beats exiting 0 having restacked nothing.
        let branch_names: Vec<String> = children.iter().map(|(name, _)| name.clone()).collect();
        let selected = prompt_multi_select(
            "Select branches to restack (Space to toggle, Enter to confirm):",
            branch_names,
            crate::commands::Fallback::Require(
                "Run without --pick to restack all floating children, or use a terminal to choose.",
            ),
        )?;
        if selected.is_empty() {
            println!("No branches selected.");
            return Ok(());
        }
        let selected_set: std::collections::HashSet<_> = selected.into_iter().collect();
        children
            .into_iter()
            .filter(|(name, _)| selected_set.contains(name))
            .collect()
    } else {
        children
    };

    // Resolve every child's current tip once; needed for chaining below and the
    // undo tip map.
    let mut entries = Vec::with_capacity(children.len());
    for (name, old_base) in children {
        let tip = repo.revparse_single(&name)?.id();
        entries.push((name, old_base, tip));
    }

    // Floating children can themselves be stacked on one another (e.g. the
    // bottom branch of a stack was rewritten out from under the rest, setting
    // the whole chain adrift). Rebasing each child directly onto the current
    // branch would flatten that chain into parallel branches that each replay
    // their ancestors' commits, so chain every child onto its nearest floating
    // ancestor and only rebase chain roots onto the current branch.
    let mut nearest_ancestor: Vec<Option<usize>> = vec![None; entries.len()];
    let mut ancestor_count = vec![0usize; entries.len()];
    for idx in 0..entries.len() {
        for other_idx in 0..entries.len() {
            if idx == other_idx || entries[idx].2 == entries[other_idx].2 {
                continue;
            }
            if !repo.graph_descendant_of(entries[idx].2, entries[other_idx].2)? {
                continue;
            }
            ancestor_count[idx] += 1;
            let closer = match nearest_ancestor[idx] {
                None => true,
                Some(best) => repo.graph_descendant_of(entries[other_idx].2, entries[best].2)?,
            };
            if closer {
                nearest_ancestor[idx] = Some(other_idx);
            }
        }
    }

    // Rebase ancestors before their descendants so each chained child lands on
    // its parent's already-moved tip. Within a chain every branch has strictly
    // more floating ancestors than its parent, so the ancestor count is a valid
    // topological key.
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by_key(|&idx| ancestor_count[idx]);

    // Construct RebaseState
    let mut parent_id_map = HashMap::new();
    let mut parent_name_map = HashMap::new();
    let mut original_tip_map = HashMap::new();
    let mut remaining = Vec::new();

    for &idx in &order {
        let (name, old_base, tip) = &entries[idx];
        match nearest_ancestor[idx] {
            Some(parent_idx) => {
                let (parent_name, _, parent_tip) = &entries[parent_idx];
                println!(" - {} (stacked on {})", name, parent_name);
                // Cut at the parent's original tip so only this branch's own
                // commits are replayed onto the parent's new position.
                parent_id_map.insert(name.clone(), parent_tip.to_string());
                parent_name_map.insert(name.clone(), parent_name.clone());
            }
            None => {
                println!(" - {} (matches old base {})", name, old_base);
                parent_id_map.insert(name.clone(), old_base.to_string());
                parent_name_map.insert(name.clone(), current_branch_name.clone());
            }
        }
        remaining.push(name.clone());
        original_tip_map.insert(name.clone(), tip.to_string());
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
        owned_tip_map: HashMap::new(),
        stash_ref: None,
        stash_apply_index: false,
        unstage_on_restore: false,
        autostash,
        cleanup_merged_branches: Vec::new(),
        cleanup_checkout_fallback: None,
    };

    // Snapshot for undo only now that the no-op checks ("No floating children",
    // "No branches selected") have passed and we are about to mutate branches.
    // The guard settles the snapshot on every exit from here on, so no path can
    // leave a stale pending snapshot behind.
    let _snapshot = crate::oplog::begin(&repo, "restack")?;
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
            // If another branch still points at the detected old base, this branch is
            // part of an intact alternate stack rather than floating off the current one.
            if has_other_local_branch_at_tip(repo, &name, old_base)? {
                continue;
            }
            results.push((name, old_base));
        }
    }
    Ok(results)
}

fn has_other_local_branch_at_tip(repo: &Repository, branch_name: &str, tip: Oid) -> Result<bool> {
    let branches = repo.branches(Some(BranchType::Local))?;
    for branch_res in branches {
        let (branch, _) = branch_res?;
        let name = match branch.name() {
            Ok(Some(name)) => name,
            _ => continue,
        };
        if name == branch_name {
            continue;
        }

        if branch.get().target() == Some(tip) {
            return Ok(true);
        }
    }
    Ok(false)
}
