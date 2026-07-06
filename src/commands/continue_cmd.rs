use crate::rebase_utils::{
    Operation, RebaseState, ReconcileMode, git_rebase_in_progress, has_staged_changes,
    reconcile_saved_rebase_state, run_rebase_loop,
};
use anyhow::{Result, anyhow};
use git2::Repository;
use std::process::Command;

pub fn continue_cmd() -> Result<()> {
    let repo = crate::open_repo()?;
    let _lock = crate::state_io::RepoLock::acquire(&repo)?;
    let rebase_state = reconcile_saved_rebase_state(&repo, ReconcileMode::Continue)?;
    let has_rebase_state = rebase_state.is_some();
    let has_run_state = crate::commands::run::run_state_exists(&repo);

    if has_rebase_state && has_run_state {
        return Err(anyhow!(
            "Multiple Kindra operations are persisted. Run 'kin abort' to clear state before continuing."
        ));
    }

    if git_rebase_in_progress(&repo) {
        if !has_rebase_state {
            return Err(anyhow!(
                "A native git rebase is in progress. Use 'git rebase --continue'."
            ));
        }

        let repaired = repair_stalled_pick_commit(&repo)?;

        println!("Continuing git rebase...");
        let status = git_rebase_step(rebase_state.as_ref(), "--continue")?;
        if !status.success() {
            // A repaired stall re-executes the pick whose changes were just
            // committed; that replay usually comes up empty and stops. Nothing
            // is staged and nothing conflicts in that case, so finishing it
            // with --skip is the completion of the recovery, not a decision.
            if repaired && rebase_stopped_on_empty_pick(&repo)? {
                println!("The recovered commit made the replayed pick empty; skipping it...");
                let status = git_rebase_step(rebase_state.as_ref(), "--skip")?;
                if !status.success() {
                    return Err(continue_failure_error());
                }
            } else {
                return Err(continue_failure_error());
            }
        }
    }

    if let Some(state) = rebase_state {
        return match state.operation {
            Operation::Sync => crate::commands::sync::finish_sync_after_rebase(&repo, state),
            _ => run_rebase_loop(&repo, state),
        };
    }

    if has_run_state {
        // `kin run` is not resumable: it restores the working tree and clears its
        // state on every normal exit. Leftover state means a run was interrupted
        // before it could restore (e.g. it failed to check the original branch
        // back out), which `kin continue` cannot resolve.
        return Err(anyhow!(
            "A previous 'kin run' was interrupted before it could restore the working tree. Use 'kin abort' to restore it."
        ));
    }

    println!("No operation in progress.");
    Ok(())
}

/// Run `git rebase <step>` with the editor resolved per the saved state.
fn git_rebase_step(
    rebase_state: Option<&RebaseState>,
    step: &str,
) -> Result<std::process::ExitStatus> {
    let mut git = Command::new("git");
    git.envs(std::env::vars_os());
    if rebase_state.is_some_and(|state| state.suppress_editor) {
        // The paused operation ran its rebase editor-less (absorb pins
        // GIT_EDITOR so squash! folds never open a commit-message editor);
        // resuming must do the same or the remaining squashes open the
        // real editor — hanging scripted runs.
        git.env("GIT_EDITOR", "true");
    } else if std::env::var_os("GIT_EDITOR").is_none() {
        // If the caller didn't pin GIT_EDITOR, resolve the editor the same
        // way the rest of the CLI does (GIT_EDITOR > core.editor > VISUAL >
        // EDITOR > vi) and hand it to git, so `kin continue` never picks a
        // different editor than the original interactive command would have.
        git.env("GIT_EDITOR", crate::editor::resolve_editor());
    }
    Ok(git.arg("rebase").arg(step).status()?)
}

fn continue_failure_error() -> anyhow::Error {
    if crate::rebase_utils::unmerged_paths_exist().unwrap_or(false) {
        anyhow!("git rebase --continue failed. Resolve conflicts and run 'kin continue' again.")
    } else {
        anyhow!(
            "git rebase --continue failed. See the git error above for the cause; once addressed, run 'kin continue' again."
        )
    }
}

/// Detect and repair a rebase stalled by a failed pick commit.
///
/// When a pick's changes reach the index but creating the commit itself fails
/// (a signing failure, an interrupted process), git is left with the changes
/// staged, the pick's message in `.git/MERGE_MSG`, and no
/// `rebase-merge/message` — a state `git rebase --continue` refuses to resume
/// ("you have staged changes in your working tree"), demanding a manual
/// commit. Restoring `rebase-merge/message` from `MERGE_MSG` lets the
/// sequencer commit the staged changes itself, with the rebase's own options
/// (author script, signing) intact.
///
/// A normal conflict stop has `rebase-merge/message`, and an `edit` stop has
/// `rebase-merge/amend`; both are left alone.
fn repair_stalled_pick_commit(repo: &Repository) -> Result<bool> {
    let rebase_dir = repo.path().join("rebase-merge");
    if !rebase_dir.exists() {
        return Ok(false);
    }
    let message = rebase_dir.join("message");
    if message.exists() || rebase_dir.join("amend").exists() {
        return Ok(false);
    }
    let merge_msg = repo.path().join("MERGE_MSG");
    if !merge_msg.exists() || !has_staged_changes()? || crate::rebase_utils::unmerged_paths_exist()?
    {
        return Ok(false);
    }

    std::fs::copy(&merge_msg, &message)?;
    println!(
        "The previous run stopped after staging a commit's changes without committing them \
         (e.g. a failed signature or an interrupted process). Restored the rebase state so \
         git can commit them now."
    );
    Ok(true)
}

/// After a repaired stall, the sequencer replays the pick whose changes were
/// already committed by the recovery; git stops when that replay produces no
/// changes. This is that state: rebase still in progress, nothing staged,
/// nothing unmerged.
fn rebase_stopped_on_empty_pick(repo: &Repository) -> Result<bool> {
    if !git_rebase_in_progress(repo) {
        return Ok(false);
    }
    Ok(!has_staged_changes()? && !crate::rebase_utils::unmerged_paths_exist()?)
}
