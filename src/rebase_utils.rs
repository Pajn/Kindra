use anyhow::{Result, anyhow};
use git2::{Oid, Repository};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::stack::collect_first_parent_chain;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Move,
    Reorder,
    Commit,
    Sync,
}

#[derive(Serialize, Deserialize, Clone)]
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
    /// branch_name -> explicit new base (branch name or commit id) for reorder-like flows
    #[serde(default)]
    pub new_base_map: HashMap<String, String>,
    /// branch_name -> number of first-parent commits originally in the branch delta
    #[serde(default)]
    pub original_commit_count_map: HashMap<String, usize>,
    /// branch_name -> original tip commit id before the operation started
    #[serde(default)]
    pub original_tip_map: HashMap<String, String>,
    /// branch_name -> tip commit id Kindra most recently left behind in a resumable state
    #[serde(default)]
    pub owned_tip_map: HashMap<String, String>,
    /// Optional stash token created by `kin commit --on` to preserve non-staged files.
    #[serde(default)]
    pub stash_ref: Option<String>,
    /// Whether to run `git reset` when returning to the original branch.
    #[serde(default)]
    pub unstage_on_restore: bool,
    /// Whether git rebase should use autostash for this operation.
    #[serde(default)]
    pub autostash: bool,
    /// Branches to clean up after a sync rebase finishes.
    #[serde(default)]
    pub cleanup_merged_branches: Vec<String>,
    /// Fallback branch to checkout before deleting the current branch after sync.
    #[serde(default)]
    pub cleanup_checkout_fallback: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcileMode {
    Continue,
    Passive,
}

pub fn state_path(repo: &Repository) -> PathBuf {
    repo.path().join("kindra_rebase_state.json")
}

pub fn save_state(repo: &Repository, state: &RebaseState) -> Result<()> {
    let mut persisted_state = state.clone();
    merge_persisted_original_tips(repo, &mut persisted_state)?;
    augment_original_tip_map(repo, &mut persisted_state)?;
    persisted_state.owned_tip_map = capture_owned_tip_map(repo, &persisted_state);
    let json = serde_json::to_string_pretty(&persisted_state)?;
    crate::state_io::write_atomic(&state_path(repo), &json)?;
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

pub fn git_rebase_in_progress(repo: &Repository) -> bool {
    repo.path().join("rebase-merge").exists() || repo.path().join("rebase-apply").exists()
}

pub fn clear_state(repo: &Repository) -> Result<()> {
    let path = state_path(repo);
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn reconcile_saved_rebase_state(
    repo: &Repository,
    mode: ReconcileMode,
) -> Result<Option<RebaseState>> {
    if !state_path(repo).exists() {
        return Ok(None);
    }

    let mut state = load_state(repo)?;
    if git_rebase_in_progress(repo) {
        if !active_git_rebase_matches_state(repo, &state)? {
            return Err(anyhow!(
                "Active git rebase does not match saved Kindra rebase state. Resolve or abort the active git rebase before continuing."
            ));
        }
        return Ok(Some(state));
    }

    let mut changed = false;
    if state.operation == Operation::Sync {
        if sync_rebase_completed(repo, &state)? {
            state.remaining_branches.clear();
            state.in_progress_branch = None;
            changed = true;
        }
    } else {
        while let Some(current_name) = state.remaining_branches.first().cloned() {
            if !branch_rebase_completed(repo, &state, &current_name)? {
                break;
            }

            if mode == ReconcileMode::Continue {
                println!("Branch {} already rebased.", current_name);
            }
            state.remaining_branches.remove(0);
            if state.in_progress_branch.as_ref() == Some(&current_name) {
                state.in_progress_branch = None;
            }
            changed = true;
        }
    }

    if state.remaining_branches.is_empty()
        && state.in_progress_branch.is_none()
        && mode == ReconcileMode::Passive
        && can_passively_clear_completed_state(repo, &state)?
    {
        clear_state(repo)?;
        return Ok(None);
    }

    if changed {
        save_state(repo, &state)?;
    }

    Ok(Some(state))
}

pub fn passively_reconcile_rebase_state(repo: &Repository) -> Result<bool> {
    if !state_path(repo).exists() {
        return Ok(false);
    }

    match reconcile_saved_rebase_state(repo, ReconcileMode::Passive) {
        Ok(state) => Ok(state.is_some()),
        Err(err) => {
            eprintln!(
                "Warning: failed to reconcile saved Kindra rebase state; treating it as active: {}",
                err
            );
            Ok(true)
        }
    }
}

/// `owned_tip_state_matches` treats an empty `state.owned_tip_map` as a deliberate
/// "no tracked branches" sentinel and also as the migration fallback for legacy
/// on-disk state loaded via `#[serde(default)]`. That means `abort` will skip
/// restoration when ownership cannot be proven. A secondary consequence is that if
/// `capture_owned_tip_map` ever returns an empty map and `save_state` persists it,
/// later `owned_tip_state_matches` checks will also report "not owned" and `abort`
/// will clear Kindra state without restoring refs.
pub fn owned_tip_state_matches(repo: &Repository, state: &RebaseState) -> Result<bool> {
    if state.owned_tip_map.is_empty() {
        return Ok(false);
    }

    let current_tip_map = capture_owned_tip_map(repo, state);
    Ok(current_tip_map == state.owned_tip_map)
}

fn capture_owned_tip_map(repo: &Repository, state: &RebaseState) -> HashMap<String, String> {
    let mut tip_map = HashMap::new();
    let tracked_branch_names = tracked_branch_names(state);
    let rebased_commit_set = collect_rebased_commit_set(repo, state);

    for branch_name in tracked_branch_names {
        if let Ok(branch) = repo.find_branch(&branch_name, git2::BranchType::Local)
            && let Some(oid) = branch.get().target()
        {
            tip_map.insert(branch_name, oid.to_string());
        }
    }

    if let Ok(branches) = repo.branches(Some(git2::BranchType::Local)) {
        for branch_result in branches.flatten() {
            let (branch, _) = branch_result;
            let Ok(Some(branch_name)) = branch.name() else {
                continue;
            };
            let Some(oid) = branch.get().target() else {
                continue;
            };
            if rebased_commit_set.contains(&oid) {
                tip_map
                    .entry(branch_name.to_string())
                    .or_insert(oid.to_string());
            }
        }
    }

    tip_map
}

fn augment_original_tip_map(repo: &Repository, state: &mut RebaseState) -> Result<()> {
    let rebased_commit_set = collect_rebased_commit_set(repo, state);
    if rebased_commit_set.is_empty() {
        return Ok(());
    }

    let branches = repo.branches(Some(git2::BranchType::Local))?;
    for branch_result in branches {
        let (branch, _) = branch_result?;
        let Some(oid) = branch.get().target() else {
            continue;
        };
        if !rebased_commit_set.contains(&oid) {
            continue;
        }

        let Ok(Some(branch_name)) = branch.name() else {
            continue;
        };
        state
            .original_tip_map
            .entry(branch_name.to_string())
            .or_insert_with(|| oid.to_string());
    }

    Ok(())
}

fn merge_persisted_original_tips(repo: &Repository, state: &mut RebaseState) -> Result<()> {
    let path = state_path(repo);
    if !path.exists() {
        return Ok(());
    }

    let json = fs::read_to_string(path)?;
    let previous_state: RebaseState = serde_json::from_str(&json)?;
    for (branch_name, original_tip) in previous_state.original_tip_map {
        state
            .original_tip_map
            .entry(branch_name)
            .or_insert(original_tip);
    }

    Ok(())
}

fn tracked_branch_names(state: &RebaseState) -> HashSet<String> {
    let mut branch_names = HashSet::new();

    branch_names.extend(state.original_tip_map.keys().cloned());
    branch_names.extend(state.remaining_branches.iter().cloned());
    branch_names.insert(state.original_branch.clone());
    if let Some(branch) = &state.caller_branch {
        branch_names.insert(branch.clone());
    }
    if let Some(branch) = &state.in_progress_branch {
        branch_names.insert(branch.clone());
    }

    branch_names
}

// collect_rebased_commit_set iterates state.original_tip_map while reading
// state.parent_id_map. This depends on save_state calling augment_original_tip_map
// before capture_owned_tip_map, so state.original_tip_map contains all branches
// present in state.parent_id_map. Callers must preserve that ordering and ensure
// state.original_tip_map contains the branches to inspect.
fn collect_rebased_commit_set(repo: &Repository, state: &RebaseState) -> HashSet<Oid> {
    let mut rebased_commits = HashSet::new();

    for (branch_name, original_tip_str) in &state.original_tip_map {
        let Some(old_parent_id_str) = state.parent_id_map.get(branch_name) else {
            continue;
        };
        let Ok(original_tip) = Oid::from_str(original_tip_str) else {
            continue;
        };
        let Ok(old_parent_id) = Oid::from_str(old_parent_id_str) else {
            continue;
        };
        if original_tip == old_parent_id {
            continue;
        }

        let Ok(mut walk) = repo.revwalk() else {
            continue;
        };
        if walk.push(original_tip).is_err() || walk.hide(old_parent_id).is_err() {
            continue;
        }

        rebased_commits.extend(walk.filter_map(|id| id.ok()));
    }

    rebased_commits
}

fn branch_rebase_target(state: &RebaseState, branch_name: &str) -> Result<(String, String)> {
    let old_parent_id_str = state
        .parent_id_map
        .get(branch_name)
        .ok_or_else(|| anyhow!("Parent ID not found for branch '{}'", branch_name))?
        .clone();

    let new_base = if let Some(explicit_base) = state.new_base_map.get(branch_name) {
        explicit_base.clone()
    } else if branch_name == state.original_branch {
        state.target_branch.clone()
    } else {
        match state.parent_name_map.get(branch_name) {
            Some(name) => name.clone(),
            None => old_parent_id_str.clone(),
        }
    };

    Ok((old_parent_id_str, new_base))
}

/// Checks rebase completion in three stages that each cover a different edge
/// case. First, `branch_rebase_target` identifies the expected base and the
/// branch tip must be a descendant of it, or equal to it, which handles branches
/// whose commits were fully replayed or intentionally emptied. Second, the
/// first-parent chain length is compared against `original_commit_count_map` so
/// a branch with hidden extra commits past the expected replay is not accepted
/// as complete. Finally, the revwalk from the current tip back to `new_base_id`
/// verifies that the first replayed commit's first parent is exactly the new
/// base, protecting against histories that contain the base but are attached
/// through an unexpected first-parent path.
fn branch_rebase_completed(
    repo: &Repository,
    state: &RebaseState,
    branch_name: &str,
) -> Result<bool> {
    let (_, new_base) = branch_rebase_target(state, branch_name)?;
    let current_id = repo.revparse_single(branch_name)?.id();
    let new_base_id = repo.revparse_single(&new_base)?.id();
    let mut is_done =
        repo.graph_descendant_of(current_id, new_base_id)? || current_id == new_base_id;

    if is_done
        && current_id != new_base_id
        && let Some(original_commit_count) = state.original_commit_count_map.get(branch_name)
    {
        let current_first_parent_chain = collect_first_parent_chain(repo, new_base_id, current_id)?;
        if current_first_parent_chain.len() > *original_commit_count {
            is_done = false;
        }
    }

    if is_done && current_id != new_base_id {
        let mut walk = repo.revwalk()?;
        walk.push(current_id)?;
        walk.hide(new_base_id)?;
        let mut commits: Vec<Oid> = walk.filter_map(|id| id.ok()).collect();
        commits.reverse();

        if let Some(&first_id) = commits.first() {
            let first_commit = repo.find_commit(first_id)?;
            if first_commit.parent_count() > 0 && first_commit.parent_id(0)? != new_base_id {
                is_done = false;
            }
        }
    }

    Ok(is_done)
}

fn active_git_rebase_matches_state(repo: &Repository, state: &RebaseState) -> Result<bool> {
    if let Some(active_branch) = active_git_rebase_branch(repo)? {
        return Ok(state.in_progress_branch.as_deref() == Some(active_branch.as_str()));
    }

    owned_tip_state_matches(repo, state)
}

fn active_git_rebase_branch(repo: &Repository) -> Result<Option<String>> {
    for rebase_dir in ["rebase-merge", "rebase-apply"] {
        let head_name_path = repo.path().join(rebase_dir).join("head-name");
        if !head_name_path.exists() {
            continue;
        }

        let head_name = fs::read_to_string(head_name_path)?;
        let branch_name = head_name
            .trim()
            .strip_prefix("refs/heads/")
            .unwrap_or_else(|| head_name.trim())
            .to_string();
        if !branch_name.is_empty() {
            return Ok(Some(branch_name));
        }
    }

    Ok(None)
}

fn sync_rebase_completed(repo: &Repository, state: &RebaseState) -> Result<bool> {
    let original_tip = repo.revparse_single(&state.original_branch)?.id();
    let target_tip = repo.revparse_single(&state.target_branch)?.id();
    Ok(original_tip == target_tip || repo.graph_descendant_of(original_tip, target_tip)?)
}

fn can_passively_clear_completed_state(repo: &Repository, state: &RebaseState) -> Result<bool> {
    if state.stash_ref.is_some()
        || state.unstage_on_restore
        || !state.cleanup_merged_branches.is_empty()
    {
        return Ok(false);
    }

    if state.operation != Operation::Sync {
        let restore_branch = state
            .caller_branch
            .as_deref()
            .unwrap_or(state.original_branch.as_str());
        if current_branch_name(repo)? != Some(restore_branch.to_string()) {
            return Ok(false);
        }
    }

    Ok(true)
}

fn current_branch_name(repo: &Repository) -> Result<Option<String>> {
    if repo.head_detached()? {
        return Ok(None);
    }

    Ok(repo.head()?.shorthand().map(ToString::to_string))
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
            "Failed to apply stashed changes from '{}'. Resolve conflicts and run 'kin continue' or 'kin abort'.",
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

        let (old_parent_id_str, new_base) = branch_rebase_target(&state, &current_name)?;

        // Check if the branch is already rebased (e.g. by a previous --update-refs)
        let is_done = branch_rebase_completed(repo, &state, &current_name)?;

        if is_done && (is_resuming || started_any) && !git_rebase_in_progress(repo) {
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
            .arg(if state.autostash {
                "--autostash"
            } else {
                "--no-autostash"
            })
            .arg("--update-refs")
            .arg("--onto")
            .arg(&new_base)
            .arg(&old_parent_id_str)
            .arg(&current_name)
            .status()?;

        if status.success() {
            state.remaining_branches.remove(0);
            state.in_progress_branch = None;
            started_any = true;
            save_state(repo, &state)?;
        } else {
            // Check if a rebase is in progress (meaning it started but hit conflicts)
            if git_rebase_in_progress(repo) {
                // Persist that this branch is in progress, but do NOT remove it from remaining_branches
                save_state(repo, &state)?;
                return Err(anyhow!(
                    "Rebase failed for branch {}. Resolve conflicts and run 'kin continue'.",
                    current_name
                ));
            } else {
                state.in_progress_branch = None;
                save_state(repo, &state)?;
                return Err(anyhow!(
                    "Rebase failed for branch {}. It seems to have failed before starting (e.g., dirty working tree). Fix the issue and run 'kin continue'.",
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

    clear_state(repo)?;

    Ok(())
}

pub fn ensure_git_supports_update_refs() -> Result<()> {
    ensure_git_version_at_least(
        (2, 38, 0),
        "This operation requires Git >= 2.38.0 because '--update-refs' is used during rebase.",
        "This operation requires Git >= 2.38.0 because it uses '--update-refs'",
    )
}

pub fn ensure_git_supports_reapply_cherry_picks() -> Result<()> {
    ensure_git_version_at_least(
        (2, 34, 0),
        "This operation requires Git >= 2.34.0 because '--reapply-cherry-picks' and '--empty=keep' are used during rebase.",
        "This operation requires Git >= 2.34.0 because it uses '--reapply-cherry-picks' and '--empty=keep'",
    )
}

fn ensure_git_version_at_least(
    minimum: (u64, u64, u64),
    detected_message_prefix: &str,
    generic_message_prefix: &str,
) -> Result<()> {
    let output = Command::new("git").arg("--version").output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "{}, but 'git --version' failed.",
            generic_message_prefix
        ));
    }

    let version_output = String::from_utf8_lossy(&output.stdout);
    let version = parse_git_semver(&version_output).ok_or_else(|| {
        anyhow!(
            "{}, but could not parse `git --version` output: {}",
            generic_message_prefix,
            version_output.trim()
        )
    })?;

    if version < minimum {
        return Err(anyhow!(
            "{} Detected Git {}.{}.{}.",
            detected_message_prefix,
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
