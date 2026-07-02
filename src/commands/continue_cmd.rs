use crate::rebase_utils::{
    Operation, ReconcileMode, git_rebase_in_progress, reconcile_saved_rebase_state, run_rebase_loop,
};
use anyhow::{Result, anyhow};
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

        println!("Continuing git rebase...");
        let mut git = Command::new("git");
        git.envs(std::env::vars_os());
        // If the caller didn't pin GIT_EDITOR, resolve the editor the same way
        // the rest of the CLI does (GIT_EDITOR > core.editor > VISUAL > EDITOR >
        // vi) and hand it to git, so `kin continue` never picks a different
        // editor than the original interactive command would have.
        if std::env::var_os("GIT_EDITOR").is_none() {
            git.env("GIT_EDITOR", crate::editor::resolve_editor());
        }
        let status = git.arg("rebase").arg("--continue").status()?;
        if !status.success() {
            return Err(anyhow!(
                "git rebase --continue failed. Resolve conflicts and try again."
            ));
        }
    }

    if let Some(state) = rebase_state {
        return match state.operation {
            Operation::Sync => crate::commands::sync::finish_sync_after_rebase(&repo, state),
            _ => run_rebase_loop(&repo, state),
        };
    }

    if has_run_state {
        return crate::commands::run::continue_run(&repo);
    }

    println!("No operation in progress.");
    Ok(())
}
