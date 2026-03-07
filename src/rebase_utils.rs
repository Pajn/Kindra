use anyhow::{Result, anyhow};
use git2::{Oid, Repository};
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
    #[serde(default)]
    pub parent_id_map: HashMap<String, String>,
    /// branch_name -> original_parent_name (if it was a branch in the sub-stack)
    #[serde(default)]
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
        return Err(anyhow!("git checkout failed for branch '{}'", branch_name));
    }
    Ok(())
}

pub fn check_worktrees(branches: &[String], force: bool) -> Result<()> {
    if force {
        return Ok(());
    }

    let current_worktree_output = Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()?;
    if !current_worktree_output.status.success() {
        return Err(anyhow!("Failed to determine current worktree path."));
    }
    let current_worktree = String::from_utf8_lossy(&current_worktree_output.stdout)
        .trim()
        .to_string();

    let worktree_list_output = Command::new("git")
        .arg("worktree")
        .arg("list")
        .arg("--porcelain")
        .output()?;
    if !worktree_list_output.status.success() {
        return Err(anyhow!("Failed to list git worktrees."));
    }

    let stdout = String::from_utf8_lossy(&worktree_list_output.stdout);
    let mut worktree_map: HashMap<String, String> = HashMap::new(); // branch_name -> worktree_path
    let mut current_path = String::new();

    for line in stdout.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = path.trim().to_string();
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            let branch_name = branch_ref
                .strip_prefix("refs/heads/")
                .unwrap_or(branch_ref)
                .trim()
                .to_string();
            worktree_map.insert(branch_name, current_path.clone());
        }
    }

    for branch in branches {
        if let Some(path) = worktree_map.get(branch)
            && path != &current_worktree
        {
            return Err(anyhow!(
                "{} is checked out in {}, aborting as a full rebase can not be completed. Use --force to ignore this check.",
                branch,
                path
            ));
        }
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
    ensure_git_supports_update_refs()?;

    let mut started_any = false;
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

        // Check if the branch is already rebased (e.g. by a previous --update-refs)
        let current_id = repo.revparse_single(&current_name)?.id();
        let new_base_id = repo.revparse_single(&new_base)?.id();
        let mut is_done =
            repo.graph_descendant_of(current_id, new_base_id)? || current_id == new_base_id;

        if is_done && current_id != new_base_id {
            // Stricter check: the first commit in the branch's delta (relative to its new base)
            // must now be a child of new_base_id.
            // We walk from current_id back to new_base_id.
            let mut walk = repo.revwalk()?;
            walk.push(current_id)?;
            walk.hide(new_base_id)?;
            let mut commits: Vec<Oid> = walk.filter_map(|id| id.ok()).collect();
            commits.reverse(); // Now oldest to newest

            if let Some(&first_id) = commits.first() {
                let first_commit = repo.find_commit(first_id)?;
                if first_commit.parent_count() > 0 && first_commit.parent_id(0)? != new_base_id {
                    // It's a descendant of new_base, but not immediately building on it.
                    // This could mean it's based on an old version of the base.
                    is_done = false;
                }
            }
        }

        if is_done
            && (is_resuming || started_any)
            && !repo.path().join("rebase-merge").exists()
            && !repo.path().join("rebase-apply").exists()
        {
            println!("Branch {} already rebased.", current_name);
            state.remaining_branches.remove(0);
            if is_resuming {
                state.in_progress_branch = None;
                started_any = true;
            }
            save_state(repo, &state)?;
            continue;
        }

        if !is_resuming {
            state.in_progress_branch = Some(current_name.clone());
            save_state(repo, &state)?;
        }

        println!("Rebasing {}...", current_name);
        let status = Command::new("git")
            .arg("rebase")
            .arg("--no-ff")
            .arg("--update-refs")
            .arg("--onto")
            .arg(&new_base)
            .arg(old_parent_id_str)
            .arg(&current_name)
            .status()?;

        if status.success() {
            state.remaining_branches.remove(0);
            state.in_progress_branch = None;
            started_any = true;
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

pub fn ensure_git_supports_update_refs() -> Result<()> {
    let output = Command::new("git").arg("--version").output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "This operation requires Git >= 2.38.0 because it uses '--update-refs', but 'git --version' failed."
        ));
    }

    let version_output = String::from_utf8_lossy(&output.stdout);
    let version = parse_git_semver(&version_output).ok_or_else(|| {
        anyhow!(
            "This operation requires Git >= 2.38.0 because it uses '--update-refs', but could not parse `git --version` output: {}",
            version_output.trim()
        )
    })?;

    if version < (2, 38, 0) {
        return Err(anyhow!(
            "This operation requires Git >= 2.38.0 because '--update-refs' is used during rebase. Detected Git {}.{}.{}.",
            version.0,
            version.1,
            version.2
        ));
    }

    Ok(())
}

fn parse_git_semver(version_output: &str) -> Option<(u64, u64, u64)> {
    let version_token = version_output
        .split_whitespace()
        .find(|part| part.as_bytes().first().is_some_and(u8::is_ascii_digit))?;

    let numbers = version_token
        .split('.')
        .filter_map(|segment| {
            let digits: String = segment
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect();
            (!digits.is_empty())
                .then_some(digits)
                .and_then(|d| d.parse::<u64>().ok())
        })
        .collect::<Vec<u64>>();

    if numbers.len() < 3 {
        return None;
    }

    Some((numbers[0], numbers[1], numbers[2]))
}

#[cfg(test)]
mod tests {
    use super::parse_git_semver;

    #[test]
    fn parse_git_semver_ignores_non_numeric_dot_segments() {
        let parsed = parse_git_semver("git version 2.44.0.windows.1");
        assert_eq!(parsed, Some((2, 44, 0)));
    }

    #[test]
    fn parse_git_semver_requires_three_components() {
        let parsed = parse_git_semver("git version 2.44");
        assert_eq!(parsed, None);
    }
}
