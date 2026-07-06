use crate::rebase_utils::{
    StashApplyOutcome, checkout_branch, drop_stash, git_rebase_in_progress, load_state,
    owned_tip_state_matches, save_state, state_path, unstage_all,
};
use anyhow::{Result, anyhow};
use git2::Oid;
use std::collections::HashMap;
use std::process::Command;

pub fn abort_cmd() -> Result<()> {
    let repo = crate::open_repo()?;
    let _lock = crate::state_io::RepoLock::acquire(&repo)?;
    let path = state_path(&repo);
    let has_rebase_state = path.exists();
    let has_run_state = crate::commands::run::run_state_exists(&repo);
    // Settle the pending oplog snapshot on *every* exit from here on, including
    // the early `?` returns below. Default is `Leave`: only once we have actually
    // finished handling the saved state do we switch to `Discard` (pre-operation
    // refs were restored, nothing to undo) or `Finalize` (divergent state cleared
    // without restoring refs, so the effects stay live and must remain undoable).
    // Any error before that point leaves an orphaned snapshot untouched, so the
    // next operation's `begin` can still flush it into an undo entry rather than
    // this abort silently dropping it.
    let mut settle = AbortOplogSettle {
        repo: &repo,
        action: SettleAction::Leave,
    };

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
            let restore_branch = parsed_state
                .caller_branch
                .clone()
                .unwrap_or_else(|| parsed_state.original_branch.clone());

            if parsed_state.preserve_content_on_abort {
                // Content the operation already committed (absorb's fixup or
                // folded commits) is only reachable from the original branch's
                // current tip, and the set-aside stash is based on that
                // content. Check the branch out and apply the stash *now*,
                // while the worktree matches the stash's base; the tip restore
                // below then moves the ref out from underneath, so the
                // discarded content reappears as staged changes.
                checkout_branch(&parsed_state.original_branch)?;
                apply_abort_stash(&repo, &mut parsed_state)?;
                restore_original_branch_tips(&parsed_state.original_tip_map)?;
                if restore_branch != parsed_state.original_branch {
                    checkout_branch(&restore_branch)?;
                }
            } else {
                restore_original_branch_tips(&parsed_state.original_tip_map)?;
                checkout_branch(&restore_branch)?;
                apply_abort_stash(&repo, &mut parsed_state)?;
            }

            if parsed_state.unstage_on_restore {
                unstage_all()?;
            }
        }

        std::fs::remove_file(path)?;
        // State handled successfully: now it is safe to settle the snapshot.
        settle.action = if kindra_owns_current_state {
            SettleAction::Discard
        } else if git_rebase_active {
            // Divergent state, but a native rebase is still mid-flight, so the
            // refs aren't the operation's final effects yet. Leave the snapshot
            // for a later `begin` to flush rather than recording a half-applied
            // entry now.
            SettleAction::Leave
        } else {
            SettleAction::Finalize
        };
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

    // `settle` drops here (and on every early return above), finalizing or
    // discarding the pending snapshot.
    Ok(())
}

/// How `AbortOplogSettle` should settle the pending snapshot on drop.
enum SettleAction {
    /// Leave any pending snapshot in place (error path, or nothing to abort), so
    /// the next operation's `begin` can flush it rather than losing it here.
    Leave,
    /// Drop the snapshot: pre-operation refs were restored, so there is nothing
    /// to undo.
    Discard,
    /// Record the snapshot as an undo entry: divergent state was cleared without
    /// restoring refs, so the operation's effects are still live.
    Finalize,
}

/// Settles the pending oplog snapshot when `abort_cmd` returns, on success or via
/// any early `?`. Best-effort, mirroring `oplog::finalize`/`discard`.
struct AbortOplogSettle<'repo> {
    repo: &'repo git2::Repository,
    action: SettleAction,
}

impl Drop for AbortOplogSettle<'_> {
    fn drop(&mut self) {
        let _ = match self.action {
            SettleAction::Leave => Ok(()),
            SettleAction::Discard => crate::oplog::discard(self.repo),
            SettleAction::Finalize => crate::oplog::finalize(self.repo),
        };
    }
}

/// Apply the state's stash (if any), honoring `stash_apply_index`, and settle
/// it in the saved state. A conflicted apply keeps the entry as a backup and
/// warns instead of failing the abort.
fn apply_abort_stash(
    repo: &git2::Repository,
    parsed_state: &mut crate::rebase_utils::RebaseState,
) -> Result<()> {
    let Some(stash_ref) = parsed_state.stash_ref.clone() else {
        return Ok(());
    };
    match crate::rebase_utils::apply_state_stash(parsed_state, &stash_ref)? {
        StashApplyOutcome::Applied => {
            parsed_state.stash_ref = None;
            save_state(repo, parsed_state)?;
            if let Err(err) = drop_stash(&stash_ref) {
                eprintln!("Warning: {}", err);
            }
        }
        StashApplyOutcome::ConflictsLeftInTree => {
            // The changes are in the tree as conflict markers; do not reapply,
            // keep the entry as a backup.
            parsed_state.stash_ref = None;
            save_state(repo, parsed_state)?;
            eprintln!(
                "Warning: restoring the set-aside changes left conflicts in the working tree; the stash entry '{}' was preserved as a backup.",
                stash_ref
            );
        }
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
