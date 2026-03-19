use crate::commands::find_upstream;
use crate::commands::resolve_rebase_autostash;
use crate::rebase_utils::{
    Operation, RebaseState, checkout_branch, clear_state, git_rebase_in_progress, save_state,
    state_path,
};
use crate::stack::{
    collect_merged_local_branches, find_sync_boundary, get_stack_branches_from_merge_base,
    get_stack_tips, resolve_merge_base,
};
use anyhow::{Result, anyhow};
use clap::Args;
use git2::BranchType;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::process::Command;

#[derive(Args)]
pub struct SyncArgs {
    /// Force the sync even if branches are checked out in other worktrees
    #[arg(long)]
    pub force: bool,

    /// Do not delete merged branches
    #[arg(long)]
    pub no_delete: bool,

    /// Allow git rebase to autostash tracked worktree changes
    #[arg(long, overrides_with = "no_autostash")]
    pub autostash: bool,

    /// Disable git rebase autostash even if configured
    #[arg(long, overrides_with = "autostash")]
    pub no_autostash: bool,
}

pub fn sync(args: &SyncArgs) -> Result<()> {
    let repo = crate::open_repo()?;

    let path = state_path(&repo);
    if path.exists() {
        return Err(anyhow!(
            "A gits operation is already in progress. Use 'gits continue' or 'gits abort'."
        ));
    }
    ensure_no_native_git_operation(&repo)?;

    let head = repo.head()?;
    let head_id = head.peel_to_commit()?.id();
    let current_branch_name = if !repo.head_detached()? {
        head.shorthand().map(|s| s.to_string())
    } else {
        None
    };

    let upstream_name = find_upstream(&repo)?.ok_or_else(|| {
        anyhow!("Could not find a base branch (init.defaultBranch, main, master, or trunk)")
    })?;
    let local_upstream = upstream_name.clone();
    let (rebase_onto_name, fetch_remote) = resolve_sync_onto(&repo, &upstream_name)?;
    fetch_sync_remote(fetch_remote.as_deref())?;

    if current_branch_name.as_deref() == Some(&upstream_name) {
        return sync_upstream_branch(&repo, args, &upstream_name, &rebase_onto_name);
    }

    let upstream_obj = repo.revparse_single(&rebase_onto_name)?;
    let upstream_id = upstream_obj.id();
    let merge_base = resolve_merge_base(&repo, upstream_id, head_id)?;
    let stack_branches = get_stack_branches_from_merge_base(
        &repo,
        merge_base,
        head_id,
        upstream_id,
        &rebase_onto_name,
    )?;

    let mut tips = get_stack_tips(&repo, &stack_branches)?;
    tips.sort();
    let top_branch = match tips.len() {
        0 => {
            if let Some(ref name) = current_branch_name {
                name.clone()
            } else {
                println!("No branches found in the current stack.");
                return Ok(());
            }
        }
        1 => tips[0].clone(),
        _ => {
            if !std::io::stdin().is_terminal() {
                return Err(anyhow!(
                    "Multiple stack tips found. Run 'gits sync' interactively to choose one, or checkout the desired tip branch and rerun."
                ));
            }
            crate::commands::prompt_select("Multiple stack tips found. Select one:", tips)?
        }
    };

    let top_branch_tip = repo.revparse_single(&top_branch)?.id();

    let boundary = find_sync_boundary(&repo, &top_branch, &rebase_onto_name, &stack_branches)?;

    let mut branches_to_check = stack_branches
        .iter()
        .map(|sb| sb.name.clone())
        .collect::<Vec<_>>();

    if !args.no_delete {
        for mb in &boundary.merged_branches {
            if !branches_to_check.contains(mb) {
                branches_to_check.push(mb.clone());
            }
        }
    }

    if !branches_to_check.is_empty() {
        crate::rebase_utils::check_worktrees(&branches_to_check, args.force)?;
    }

    if let Some(old_base) = boundary.old_base {
        crate::rebase_utils::ensure_git_supports_update_refs()?;
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

        let state = RebaseState {
            operation: Operation::Sync,
            original_branch: top_branch.clone(),
            target_branch: rebase_onto_name.clone(),
            caller_branch: current_branch_name
                .clone()
                .filter(|branch| branch != &top_branch),
            remaining_branches: vec![top_branch.clone()],
            in_progress_branch: None,
            parent_id_map: HashMap::new(),
            parent_name_map: HashMap::new(),
            new_base_map: HashMap::new(),
            original_commit_count_map: HashMap::new(),
            original_tip_map: HashMap::from([(top_branch.clone(), top_branch_tip.to_string())]),
            stash_ref: None,
            unstage_on_restore: false,
            autostash,
            cleanup_merged_branches: if args.no_delete {
                Vec::new()
            } else {
                boundary.merged_branches.clone()
            },
            cleanup_checkout_fallback: Some(local_upstream.clone()),
        };

        save_state(&repo, &state)?;

        if current_branch_name.as_deref() != Some(top_branch.as_str()) {
            checkout_branch(&top_branch)?;
        }

        let mut rebase = Command::new("git");
        rebase
            .arg("rebase")
            .arg("--reapply-cherry-picks")
            .arg("--empty=keep")
            .arg(if autostash {
                "--autostash"
            } else {
                "--no-autostash"
            })
            .arg("--update-refs")
            .arg("--onto")
            .arg(&rebase_onto_name)
            .arg(old_base.to_string())
            .arg(&top_branch);

        return run_sync_rebase(&repo, state, rebase);
    } else {
        println!(
            "All commits in this stack appear to be integrated into {}.",
            rebase_onto_name
        );
    }

    if !args.no_delete {
        delete_merged_branches(&repo, &boundary.merged_branches, &local_upstream)?;
    }

    Ok(())
}

fn sync_upstream_branch(
    repo: &git2::Repository,
    args: &SyncArgs,
    upstream_name: &str,
    rebase_onto_name: &str,
) -> Result<()> {
    let merged_branches = if args.no_delete {
        Vec::new()
    } else {
        collect_merged_local_branches(repo, rebase_onto_name, &[upstream_name])?
    };

    if !merged_branches.is_empty() {
        crate::rebase_utils::check_worktrees(&merged_branches, args.force)?;
    }

    let upstream_id = repo.revparse_single(upstream_name)?.id();
    let rebase_onto_id = repo.revparse_single(rebase_onto_name)?.id();
    if upstream_id != rebase_onto_id {
        crate::rebase_utils::ensure_git_supports_reapply_cherry_picks()?;
        let autostash = resolve_rebase_autostash(
            repo,
            if args.autostash {
                Some(true)
            } else if args.no_autostash {
                Some(false)
            } else {
                None
            },
        )?;

        let state = RebaseState {
            operation: Operation::Sync,
            original_branch: upstream_name.to_string(),
            target_branch: rebase_onto_name.to_string(),
            caller_branch: None,
            remaining_branches: vec![upstream_name.to_string()],
            in_progress_branch: None,
            parent_id_map: HashMap::new(),
            parent_name_map: HashMap::new(),
            new_base_map: HashMap::new(),
            original_commit_count_map: HashMap::new(),
            original_tip_map: HashMap::from([(upstream_name.to_string(), upstream_id.to_string())]),
            stash_ref: None,
            unstage_on_restore: false,
            autostash,
            cleanup_merged_branches: merged_branches.clone(),
            cleanup_checkout_fallback: Some(upstream_name.to_string()),
        };

        let mut rebase = Command::new("git");
        rebase
            .arg("rebase")
            .arg("--reapply-cherry-picks")
            .arg("--empty=keep")
            .arg(if autostash {
                "--autostash"
            } else {
                "--no-autostash"
            })
            .arg(rebase_onto_name);

        return run_sync_rebase(repo, state, rebase);
    } else {
        println!("{} is already up to date.", upstream_name);
    }

    if !args.no_delete {
        delete_merged_branches(repo, &merged_branches, upstream_name)?;
    }

    Ok(())
}

fn delete_merged_branches(
    repo: &git2::Repository,
    branches: &[String],
    checkout_fallback: &str,
) -> Result<()> {
    if branches.is_empty() {
        return Ok(());
    }

    let head = repo.head()?;
    let current_branch = if !repo.head_detached()? {
        head.shorthand()
    } else {
        None
    };

    if let Some(cb) = current_branch
        && branches.iter().any(|b| b == cb)
    {
        println!(
            "Current branch '{}' is merged. Switching to '{}' before deletion.",
            cb, checkout_fallback
        );
        checkout_branch(checkout_fallback).map_err(|e| {
            anyhow!(
                "fallback git checkout failed for branch '{}': {}",
                checkout_fallback,
                e
            )
        })?;
    }

    for branch_name in branches {
        let status = Command::new("git")
            .arg("branch")
            .arg("-D")
            .arg(branch_name)
            .status()?;

        if !status.success() {
            println!(
                "Warning: Failed to delete merged branch: {}. It might be checked out in another worktree.",
                branch_name
            );
        } else {
            println!("Deleted merged branch: {}", branch_name);
        }
    }
    Ok(())
}

fn run_sync_rebase(
    repo: &git2::Repository,
    mut state: RebaseState,
    mut rebase: Command,
) -> Result<()> {
    state.in_progress_branch = Some(state.original_branch.clone());
    save_state(repo, &state)?;

    let status = rebase.status()?;
    if status.success() {
        return finish_sync_after_rebase(repo, state);
    }

    if git_rebase_in_progress(repo) {
        save_state(repo, &state)?;
        return Err(anyhow!(
            "git rebase failed during sync. Resolve conflicts and run 'gits continue' or 'gits abort'."
        ));
    }

    state.in_progress_branch = None;
    save_state(repo, &state)?;
    Err(anyhow!(
        "git rebase failed before sync could enter an in-progress state. Run 'gits abort' to clear the saved state, then run 'gits sync' again (or otherwise fix the rebase)."
    ))
}

pub(crate) fn finish_sync_after_rebase(repo: &git2::Repository, state: RebaseState) -> Result<()> {
    ensure_sync_rebase_completed(repo, &state)?;
    clear_state(repo)?;

    let checkout_fallback = state
        .cleanup_checkout_fallback
        .as_deref()
        .unwrap_or(state.target_branch.as_str());
    delete_merged_branches(repo, &state.cleanup_merged_branches, checkout_fallback)
}

fn ensure_sync_rebase_completed(repo: &git2::Repository, state: &RebaseState) -> Result<()> {
    let original_tip = repo.revparse_single(&state.original_branch)?.id();
    let target_tip = repo.revparse_single(&state.target_branch)?.id();
    let completed =
        original_tip == target_tip || repo.graph_descendant_of(original_tip, target_tip)?;

    if completed {
        return Ok(());
    }

    Err(anyhow!(
        "Sync did not complete: '{}' is not rebased onto '{}'. If the Git rebase was aborted manually, run 'gits abort' to clear the saved sync state or rerun 'gits sync'.",
        state.original_branch,
        state.target_branch
    ))
}

pub(crate) fn ensure_no_native_git_operation(repo: &git2::Repository) -> Result<()> {
    let git_dir = repo.path();
    let rebase_in_progress = git_rebase_in_progress(repo);
    let merge_in_progress = git_dir.join("MERGE_HEAD").exists();
    let cherry_pick_in_progress = git_dir.join("CHERRY_PICK_HEAD").exists();

    if rebase_in_progress || merge_in_progress || cherry_pick_in_progress {
        return Err(anyhow!(
            "A native git operation is in progress. Resolve it first with 'git rebase --continue'/'git rebase --abort', 'git merge --abort', or 'git cherry-pick --continue'/'git cherry-pick --abort'. If this came from a gits-managed rebase, use 'gits continue' or 'gits abort'."
        ));
    }

    Ok(())
}

fn resolve_sync_onto(
    repo: &git2::Repository,
    upstream_name: &str,
) -> Result<(String, Option<String>)> {
    if let Ok(branch) = repo.find_branch(upstream_name, BranchType::Local)
        && let Ok(upstream_branch) = branch.upstream()
        && let Some(upstream_ref) = upstream_branch.name()?
    {
        let remote_name = repo
            .branch_remote_name(upstream_branch.get().name().unwrap())
            .ok()
            .and_then(|buf| buf.as_str().map(|s| s.to_string()));
        return Ok((upstream_ref.to_string(), remote_name));
    }

    let remotes = repo.remotes()?;
    let remote_names: Vec<String> = remotes.iter().flatten().map(|s| s.to_string()).collect();
    if let Some((prefix, _)) = upstream_name.split_once('/')
        && remote_names.iter().any(|remote| remote == prefix)
    {
        return Ok((upstream_name.to_string(), Some(prefix.to_string())));
    }

    let origin_candidate = format!("origin/{upstream_name}");
    if repo.revparse_single(&origin_candidate).is_ok() {
        return Ok((origin_candidate, Some("origin".to_string())));
    }

    if remote_names.len() == 1 {
        let only_remote_candidate = format!("{}/{}", remote_names[0], upstream_name);
        if repo.revparse_single(&only_remote_candidate).is_ok() {
            return Ok((only_remote_candidate, Some(remote_names[0].clone())));
        }
    }

    Ok((upstream_name.to_string(), None))
}

fn fetch_sync_remote(remote_name: Option<&str>) -> Result<()> {
    let Some(remote_name) = remote_name else {
        return Ok(());
    };

    let status = Command::new("git").arg("fetch").arg(remote_name).status()?;
    if !status.success() {
        return Err(anyhow!(
            "git fetch failed for remote '{}' while preparing sync.",
            remote_name
        ));
    }

    Ok(())
}
