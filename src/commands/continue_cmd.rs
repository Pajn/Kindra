use crate::rebase_utils::{
    Operation, ReconcileMode, git_rebase_in_progress, reconcile_saved_rebase_state, run_rebase_loop,
};
use anyhow::{Result, anyhow};
use std::process::Command;

pub fn continue_cmd() -> Result<()> {
    let repo = crate::open_repo()?;
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
        if std::env::var_os("GIT_EDITOR").is_none() {
            if let Some(editor) = std::env::var_os("EDITOR") {
                git.env("GIT_EDITOR", editor);
            } else if let Some(editor) = std::env::var_os("VISUAL") {
                git.env("GIT_EDITOR", editor);
            }
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
