use anyhow::{Result, anyhow};
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Move,
    Commit,
}

#[derive(Serialize, Deserialize)]
pub struct RebaseState {
    pub operation: Operation,
    /// Branch that acts as the rebase-root for this operation.
    pub original_branch: String,
    /// Operation target branch (for move: onto branch, for commit: commit target).
    pub target_branch: String,
    /// Branch to restore at the end (set for commit --on from another branch).
    #[serde(default)]
    pub caller_branch: Option<String>,
    /// List of branches remaining to be moved
    pub remaining_branches: Vec<String>,
    /// The branch currently being rebased
    pub in_progress_branch: Option<String>,
    /// branch_name -> original_parent_id_str
    pub parent_id_map: HashMap<String, String>,
    /// branch_name -> original_parent_name (if it was a branch in the sub-stack)
    pub parent_name_map: HashMap<String, String>,
    /// Optional stash token created by `gits commit --on` to preserve non-staged files.
    #[serde(default)]
    pub stash_ref: Option<String>,
    /// Whether to run `git reset` when returning to the original branch.
    #[serde(default)]
    pub unstage_on_restore: bool,
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

pub fn checkout_branch(branch_name: &str) -> Result<()> {
    let status = Command::new("git")
        .arg("checkout")
        .arg(branch_name)
        .status()?;
    if !status.success() {
        return Err(anyhow!("Failed to checkout branch '{}'.", branch_name));
    }
    Ok(())
}

pub fn apply_stash(stash_ref: &str) -> Result<()> {
    let resolved_ref = resolve_stash_reference(stash_ref)?;
    let status = Command::new("git")
        .arg("stash")
        .arg("apply")
        .arg(&resolved_ref)
        .status()?;
    if !status.success() {
        return Err(anyhow!(
            "Failed to apply stashed changes from '{}'. Resolve conflicts and run 'gits continue' or 'gits abort'.",
            stash_ref
        ));
    }
    Ok(())
}

pub fn drop_stash(stash_ref: &str) -> Result<()> {
    let resolved_ref = resolve_stash_reference(stash_ref)?;
    let status = Command::new("git")
        .arg("stash")
        .arg("drop")
        .arg(&resolved_ref)
        .status()?;
    if !status.success() {
        return Err(anyhow!("Failed to drop stash entry '{}'.", stash_ref));
    }
    Ok(())
}

fn resolve_stash_reference(stash_ref: &str) -> Result<String> {
    if stash_ref.starts_with("stash@{") {
        return Ok(stash_ref.to_string());
    }

    let output = Command::new("git")
        .arg("stash")
        .arg("list")
        .arg("--format=%gd%x09%gs")
        .output()?;
    if !output.status.success() {
        return Err(anyhow!("Failed to list stash entries."));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some((reference, subject)) = line.split_once('\t') {
            let parsed_message = subject
                .split_once(": ")
                .map(|(_, message)| message.trim())
                .unwrap_or_else(|| subject.trim());
            if parsed_message == stash_ref {
                return Ok(reference.to_string());
            }
        }
    }

    Err(anyhow!("Could not locate stash entry '{}'.", stash_ref))
}

pub fn unstage_all() -> Result<()> {
    let status = Command::new("git").arg("reset").status()?;
    if !status.success() {
        return Err(anyhow!(
            "Failed to unstage files after returning to the original branch."
        ));
    }
    Ok(())
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
                    "Rebase failed for branch {}. Resolve conflicts and run 'gits continue'.",
                    current_name
                ));
            } else {
                return Err(anyhow!(
                    "Rebase failed for branch {}. It seems to have failed before starting (e.g., dirty working tree). Fix the issue and run 'gits continue'.",
                    current_name
                ));
            }
        }
    }

    let restore_branch = state
        .caller_branch
        .clone()
        .unwrap_or_else(|| state.original_branch.clone());
    println!(
        "Operation completed. Checking out original branch {}...",
        restore_branch
    );
    checkout_branch(&restore_branch).map_err(|e| {
        anyhow!(
            "Failed to checkout back to original branch '{}'. State file preserved. {}",
            restore_branch,
            e
        )
    })?;

    if let Some(stash_ref) = state.stash_ref.clone() {
        println!("Restoring stashed non-staged files...");
        apply_stash(&stash_ref)?;
        state.stash_ref = None;
        save_state(repo, &state)?;
        if let Err(err) = drop_stash(&stash_ref) {
            eprintln!("Warning: {}", err);
        }
    }

    if state.unstage_on_restore {
        unstage_all()?;
    }

    let path = state_path(repo);
    if path.exists() {
        fs::remove_file(path)?;
    }

    Ok(())
}
