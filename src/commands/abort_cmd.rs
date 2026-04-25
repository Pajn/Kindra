use crate::rebase_utils::{
    apply_stash, checkout_branch, drop_stash, git_rebase_in_progress, load_state,
    owned_tip_state_matches, save_state, state_path, unstage_all,
};
use anyhow::{Result, anyhow};
use git2::Oid;
use std::collections::HashMap;
use std::process::Command;

pub fn abort_cmd() -> Result<()> {
    let repo = crate::open_repo()?;
    let path = state_path(&repo);
    let has_rebase_state = path.exists();
    let has_run_state = crate::commands::run::run_state_exists(&repo);

    if has_rebase_state && has_run_state {
        return Err(anyhow!(
            "Multiple Kindra operations are persisted. Resolve state manually before aborting."
        ));
    }

    if has_rebase_state {
        let mut parsed_state = load_state(&repo)?;
        let git_rebase_active = git_rebase_in_progress(&repo);
        let kindra_owns_current_state = owned_tip_state_matches(&repo, &parsed_state)?;

        if git_rebase_active && kindra_owns_current_state {
            println!("Aborting active git rebase...");
            let status = Command::new("git").arg("rebase").arg("--abort").status()?;
            if !status.success() {
                return Err(anyhow!("Failed to abort git rebase."));
            }
        }

        if kindra_owns_current_state {
            restore_original_branch_tips(&parsed_state.original_tip_map)?;

            let restore_branch = parsed_state
                .caller_branch
                .clone()
                .unwrap_or_else(|| parsed_state.original_branch.clone());
            checkout_branch(&restore_branch)?;
            if let Some(stash_ref) = parsed_state.stash_ref.clone() {
                apply_stash(&stash_ref)?;
                parsed_state.stash_ref = None;
                save_state(&repo, &parsed_state)?;
                if let Err(err) = drop_stash(&stash_ref) {
                    eprintln!("Warning: {}", err);
                }
            }
            if parsed_state.unstage_on_restore {
                unstage_all()?;
            }
        }

        std::fs::remove_file(path)?;
        if kindra_owns_current_state {
            println!("Operation aborted (state cleared).");
        } else if git_rebase_active {
            if let Some(stash_ref) = parsed_state.stash_ref.clone() {
                println!(
                    "Kindra state cleared without touching the active git rebase because the repository no longer matches Kindra's saved state. Saved stash '{}' was left untouched for manual recovery.",
                    stash_ref
                );
            } else {
                println!(
                    "Kindra state cleared without touching the active git rebase because the repository no longer matches Kindra's saved state."
                );
            }
        } else {
            if let Some(stash_ref) = parsed_state.stash_ref.clone() {
                println!(
                    "Kindra state cleared without restoring refs because the repository no longer matches Kindra's saved state. Saved stash '{}' was left untouched for manual recovery.",
                    stash_ref
                );
            } else {
                println!(
                    "Kindra state cleared without restoring refs because the repository no longer matches Kindra's saved state."
                );
            }
        }
    } else if has_run_state {
        crate::commands::run::abort_run(&repo)?;
    } else if git_rebase_in_progress(&repo) {
        println!("A native git rebase is in progress. Use 'git rebase --abort'.");
    } else {
        println!("No operation in progress.");
    }

    Ok(())
}

fn restore_original_branch_tips(original_tip_map: &HashMap<String, String>) -> Result<()> {
    for (branch_name, original_tip) in original_tip_map {
        let oid = Oid::from_str(original_tip).map_err(|_| {
            anyhow!(
                "Saved original tip for branch '{}' is invalid: '{}'.",
                branch_name,
                original_tip
            )
        })?;

        let status = Command::new("git")
            .arg("update-ref")
            .arg(format!("refs/heads/{branch_name}"))
            .arg(oid.to_string())
            .status()?;
        if !status.success() {
            return Err(anyhow!(
                "Failed to restore branch '{}' to its original tip.",
                branch_name
            ));
        }
    }

    Ok(())
}
