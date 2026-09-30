use crate::operation_state::{KindraOperation, NativeOperation};
use crate::rebase_utils::{
    CreatedBranch, RebaseState, checkout_branch, first_held_elsewhere, git_rebase_in_progress,
    load_state, owned_tip_state_matches, save_state, unstage_all,
};
use crate::repository::git_command;
use crate::set_aside::{self, Phase};
use anyhow::{Result, anyhow};
use git2::Oid;
use std::collections::HashMap;

pub fn abort_cmd(clear_state_only: bool) -> Result<()> {
    let repo = crate::open_repo()?;
    let _lock = crate::state_io::RepoLock::acquire(&repo)?;
    if clear_state_only {
        return abort_locked(&repo, clear_state_only);
    }
    let active = crate::operation_state::query(&repo);
    match (&active.kindra, active.native) {
        (KindraOperation::Conflicting(kinds), _) => {
            return Err(anyhow!(crate::operation_state::conflicting_advice(kinds)));
        }
        // Rolling a paused operation back checks out and resets branches,
        // which would tear through another native operation's state.
        (
            KindraOperation::Rebase(_) | KindraOperation::Run,
            native @ (NativeOperation::Am
            | NativeOperation::Merge
            | NativeOperation::CherryPick
            | NativeOperation::Revert
            | NativeOperation::Bisect),
        ) => {
            return Err(anyhow!(
                "{} Then run 'kin abort', or run 'kin abort --clear-state' to discard Kindra's saved state without touching Git.",
                native.advice()
            ));
        }
        _ => {}
    }
    crate::overrides::with_planned(&repo, true, || abort_locked(&repo, false))
}

fn abort_locked(repo: &git2::Repository, clear_state_only: bool) -> Result<()> {
    let kindra = crate::operation_state::kindra_operation(repo);
    // Settle the pending oplog snapshot on *every* exit from here on, including
    // the early `?` returns below. Default is `Leave`: only once we have actually
    // finished handling the saved state do we switch to `Discard` (pre-operation
    // refs were restored, nothing to undo) or `Finalize` (divergent state cleared
    // without restoring refs, so the effects stay live and must remain undoable).
    // Any error before that point leaves an orphaned snapshot untouched, so the
    // next operation's `begin` can still flush it into an undo entry rather than
    // this abort silently dropping it.
    let mut settle = AbortOplogSettle {
        repo,
        action: SettleAction::Leave,
    };

    if clear_state_only {
        // This escape hatch deliberately does not deserialize state: it must
        // also work for malformed files or overlapping interrupted operations.
        // Keep Git's rebase, refs, index, worktree and stash entries untouched.
        for kind in crate::operation_state::PersistedOperation::ALL {
            match std::fs::remove_file(kind.path(repo)) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
        if !git_rebase_in_progress(repo) {
            settle.action = SettleAction::Finalize;
        }
        println!("Kindra operation state cleared. Git state and saved stashes were left intact.");
        match crate::operation_state::native_operation(repo) {
            NativeOperation::None => {}
            NativeOperation::Rebase => println!(
                "The Git rebase is still in progress; manage it with git rebase --continue or --abort."
            ),
            native => println!("{}", native.advice()),
        }
        return Ok(());
    }

    if let KindraOperation::Conflicting(kinds) = &kindra {
        return Err(anyhow!(crate::operation_state::conflicting_advice(kinds)));
    }
    if kindra == KindraOperation::Hydration {
        crate::commands::checkout::abort_hydration(repo)?;
        settle.action = if git_rebase_in_progress(repo) {
            SettleAction::Leave
        } else {
            SettleAction::Finalize
        };
        return Ok(());
    }

    if matches!(kindra, KindraOperation::Rebase(_)) {
        let mut parsed_state = load_state(repo)?;
        // Kindra before 0.2.0 saved no record of which branch tips it owns, so
        // nothing proves the repository is still as the operation left it.
        // Leave everything as it is rather than clear the state or restore refs.
        if parsed_state.owned_tip_map.is_empty() {
            return Err(anyhow!(
                "This operation was saved by an older Kindra that did not record which branches it owns, so 'kin abort' cannot undo it safely. Run 'kin continue' to finish it, or 'kin abort --clear-state' to discard Kindra's record and then undo the rest with Git (for example 'git rebase --abort')."
            ));
        }
        let git_rebase_active = git_rebase_in_progress(repo);
        let kindra_owns_current_state = owned_tip_state_matches(repo, &parsed_state)?;

        // Returning to the branch a created branch came from is the last
        // step of the rollback; when another worktree holds that branch, say
        // so before anything moves.
        if kindra_owns_current_state
            && let Some(created) = &parsed_state.created_branch
            && let Some((branch, held)) =
                first_held_elsewhere(repo, std::slice::from_ref(&created.from))?
        {
            return Err(anyhow!(
                "Cannot abort: '{branch}' is {held}, and 'kin abort' returns to it. Switch that worktree to another branch, then run 'kin abort' again."
            ));
        }

        if git_rebase_active && kindra_owns_current_state {
            println!("Aborting active git rebase...");
            let status = git_command(repo).arg("rebase").arg("--abort").status()?;
            if !status.success() {
                return Err(anyhow!("Failed to abort git rebase."));
            }
        }

        if kindra_owns_current_state {
            // A branch the command created is undone too: return to the
            // branch it was created from.
            let restore_branch = match &parsed_state.created_branch {
                Some(created) => created.from.clone(),
                None => parsed_state
                    .caller_branch
                    .clone()
                    .unwrap_or_else(|| parsed_state.original_branch.clone()),
            };

            if parsed_state.preserve_content_on_abort {
                // Content the operation already committed (absorb's fixup or
                // folded commits) is only reachable from the original branch's
                // current tip, and the set-aside stash is based on that
                // content. Check the branch out and apply the stash *now*,
                // while the worktree matches the stash's base; the tip restore
                // below then moves the ref out from underneath, so the
                // discarded content reappears as staged changes.
                checkout_branch(repo, &parsed_state.original_branch)?;
                set_aside::restore_all(repo, &mut parsed_state, Phase::Abort)?;
                restore_original_branch_tips(repo, &parsed_state.original_tip_map)?;
                if restore_branch != parsed_state.original_branch {
                    checkout_after_restoring_tips(repo, &mut parsed_state, &restore_branch)?;
                }
            } else {
                restore_original_branch_tips(repo, &parsed_state.original_tip_map)?;
                checkout_after_restoring_tips(repo, &mut parsed_state, &restore_branch)?;
                set_aside::restore_all(repo, &mut parsed_state, Phase::Abort)?;
            }

            if parsed_state.unstage_on_restore {
                unstage_all(repo)?;
            }

            if let Some(created) = &parsed_state.created_branch {
                delete_created_branch(repo, created);
            }
        }

        crate::rebase_utils::clear_state(repo)?;
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
            if let Some(changes) = parsed_state.set_asides.changes() {
                println!(
                    "Kindra state cleared without touching the active git rebase because the repository no longer matches Kindra's saved state. Saved stash '{}' was left untouched for manual recovery.",
                    changes.stash
                );
            } else {
                println!(
                    "Kindra state cleared without touching the active git rebase because the repository no longer matches Kindra's saved state."
                );
            }
        } else {
            if let Some(changes) = parsed_state.set_asides.changes() {
                println!(
                    "Kindra state cleared without restoring refs because the repository no longer matches Kindra's saved state. Saved stash '{}' was left untouched for manual recovery.",
                    changes.stash
                );
            } else {
                println!(
                    "Kindra state cleared without restoring refs because the repository no longer matches Kindra's saved state."
                );
            }
        }
    } else if kindra == KindraOperation::Run {
        crate::commands::run::abort_run(repo)?;
    } else {
        match crate::operation_state::native_operation(repo) {
            NativeOperation::None => println!("No operation in progress."),
            native => println!("{}", native.advice()),
        }
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

/// Check out `branch` once the branch tips are restored. The journal still
/// records the tips the operation left, so if the checkout fails, save it
/// again (recording the restored tips as the ones it owns) and only for
/// `kin abort`: another abort then finds the repository as this one left it
/// and finishes the rollback, where it would otherwise take the restored tips
/// for someone else's changes and clear the journal without returning.
fn checkout_after_restoring_tips(
    repo: &git2::Repository,
    state: &mut RebaseState,
    branch: &str,
) -> Result<()> {
    let Err(err) = checkout_branch(repo, branch) else {
        return Ok(());
    };
    state.abort_only = true;
    if let Err(save_err) = save_state(repo, state) {
        return Err(err.context(format!(
            "The branches are restored, but checking out '{branch}' failed and saving that progress failed too ({save_err:#}). Check out '{branch}' yourself, then run 'kin abort --clear-state'."
        )));
    }
    Err(err.context(format!(
        "The branches are restored, but checking out '{branch}' failed. Fix what blocks it, then run 'kin abort' again to finish."
    )))
}

/// Delete the branch the operation created, now that its tip is back where it
/// was created. One that points anywhere else holds commits of its own, so it
/// is kept. Everything else is already restored, so a branch that cannot be
/// deleted is only reported.
fn delete_created_branch(repo: &git2::Repository, created: &CreatedBranch) {
    let Ok(mut branch) = repo.find_branch(&created.name, git2::BranchType::Local) else {
        return;
    };
    let from_tip = repo
        .find_branch(&created.from, git2::BranchType::Local)
        .ok()
        .and_then(|from| from.get().target());
    if branch.get().target().is_none() || branch.get().target() != from_tip {
        println!(
            "Kept branch '{}': it no longer points where it was created from '{}'.",
            created.name, created.from
        );
        return;
    }
    // Delete the reference that was just checked: libgit2 deletes it only
    // while it still points at the tip read here, so a concurrent update
    // makes the delete fail rather than drop the commits it brought.
    if let Err(err) = branch.delete() {
        eprintln!(
            "Could not delete branch '{}' ({err}); remove it with 'git branch -D {}'.",
            created.name, created.name
        );
    }
}

fn restore_original_branch_tips(
    repo: &git2::Repository,
    original_tip_map: &HashMap<String, String>,
) -> Result<()> {
    for (branch_name, original_tip) in original_tip_map {
        let oid = Oid::from_str(original_tip).map_err(|_| {
            anyhow!(
                "Saved original tip for branch '{}' is invalid: '{}'.",
                branch_name,
                original_tip
            )
        })?;

        let status = git_command(repo)
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
