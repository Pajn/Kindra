use crate::commands::find_upstream;
use crate::rebase_utils::{checkout_branch, state_path};
use crate::stack::{find_sync_boundary, get_stack_branches_from_merge_base, get_stack_tips};
use anyhow::{Result, anyhow};
use clap::Args;
use git2::BranchType;
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
}

pub fn sync(args: &SyncArgs) -> Result<()> {
    let repo = crate::open_repo()?;

    let path = state_path(&repo);
    if path.exists() {
        return Err(anyhow!(
            "A move or commit operation is already in progress. Use 'gits continue' or 'gits abort'."
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

    let upstream_name = find_upstream(&repo)?;
    let local_upstream = upstream_name.clone();
    if current_branch_name.as_deref() == Some(&upstream_name) {
        return Err(anyhow!(
            "Branch '{}' is the upstream branch. Switch to a stack branch before running 'sync'.",
            upstream_name
        ));
    }
    let (rebase_onto_name, fetch_remote) = resolve_sync_onto(&repo, &upstream_name)?;
    fetch_sync_remote(fetch_remote.as_deref())?;

    let upstream_obj = repo.revparse_single(&rebase_onto_name)?;
    let upstream_id = upstream_obj.id();
    let merge_base = repo.merge_base(upstream_id, head_id)?;
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

    if current_branch_name.as_deref() != Some(top_branch.as_str()) {
        checkout_branch(&top_branch)?;
    }

    let boundary = find_sync_boundary(&repo, &top_branch, &rebase_onto_name)?;

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

        let status = Command::new("git")
            .arg("rebase")
            .arg("--update-refs")
            .arg("--onto")
            .arg(&rebase_onto_name)
            .arg(old_base.to_string())
            .arg(&top_branch)
            .status()?;

        if !status.success() {
            return Err(anyhow!(
                "git rebase failed. Resolve conflicts and run 'git rebase --continue' or 'git rebase --abort'."
            ));
        }
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

fn ensure_no_native_git_operation(repo: &git2::Repository) -> Result<()> {
    let git_dir = repo.path();
    let rebase_in_progress =
        git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists();
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
