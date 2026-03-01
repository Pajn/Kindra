use anyhow::{Result, anyhow};
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[derive(Serialize, Deserialize)]
pub struct RebaseState {
    pub original_branch: String,
    pub target_branch: String,
    /// List of branches remaining to be moved
    pub remaining_branches: Vec<String>,
    /// The branch currently being rebased
    pub in_progress_branch: Option<String>,
    /// branch_name -> original_parent_id_str
    pub parent_id_map: HashMap<String, String>,
    /// branch_name -> original_parent_name (if it was a branch in the sub-stack)
    pub parent_name_map: HashMap<String, String>,
}

pub fn state_path(repo: &Repository) -> PathBuf {
    repo.path().join("gits_rebase_state.json")
}

pub fn save_state(repo: &Repository, state: &RebaseState) -> Result<()> {
    let json = serde_json::to_string_pretty(state)?;
    fs::write(state_path(repo), json)?;
    Ok(())
}

pub fn load_state(repo: &Repository) -> Result<RebaseState> {
    let path = state_path(repo);
    if !path.exists() {
        return Err(anyhow!("No rebase operation in progress."));
    }
    let json = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&json)?)
}

pub fn run_rebase_loop(repo: &Repository, mut state: RebaseState) -> Result<()> {
    while !state.remaining_branches.is_empty() {
        let current_name = state.remaining_branches[0].clone();

        // Check if we are resuming a rebase that was already in progress
        let is_resuming = state.in_progress_branch.as_ref() == Some(&current_name);

        let old_parent_id_str = state
            .parent_id_map
            .get(&current_name)
            .ok_or_else(|| anyhow!("Parent ID not found for branch '{}'", current_name))?;

        let new_base = if current_name == state.original_branch {
            state.target_branch.clone()
        } else {
            match state.parent_name_map.get(&current_name) {
                Some(name) => name.clone(),        // rebase onto the already-moved branch
                None => old_parent_id_str.clone(), // rebase onto original commit
            }
        };

        if is_resuming {
            // If no rebase is active, check if the user finished it manually
            if !repo.path().join("rebase-merge").exists()
                && !repo.path().join("rebase-apply").exists()
            {
                let branch_id = repo.revparse_single(&current_name)?.id();
                let new_base_id = repo.revparse_single(&new_base)?.id();

                if repo.graph_descendant_of(branch_id, new_base_id)? {
                    println!("Branch {} already rebased.", current_name);
                    state.remaining_branches.remove(0);
                    state.in_progress_branch = None;
                    save_state(repo, &state)?;
                    continue;
                }
            }
        } else {
            state.in_progress_branch = Some(current_name.clone());
            save_state(repo, &state)?;
        }

        println!("Rebasing {}...", current_name);
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
            state.in_progress_branch = None;
            save_state(repo, &state)?;
        } else {
            // Check if a rebase is in progress (meaning it started but hit conflicts)
            if repo.path().join("rebase-merge").exists()
                || repo.path().join("rebase-apply").exists()
            {
                // Persist that this branch is in progress, but do NOT remove it from remaining_branches
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
        let path = state_path(repo);
        if path.exists() {
            fs::remove_file(path)?;
        }
    } else {
        return Err(anyhow!(
            "Failed to checkout back to original branch '{}'. State file preserved.",
            state.original_branch
        ));
    }

    Ok(())
}
