use crate::rebase_utils::{
    Operation, git_rebase_in_progress, load_state, run_rebase_loop, state_path,
};
use anyhow::{Result, anyhow};
use std::process::Command;

pub fn continue_cmd() -> Result<()> {
    let repo = crate::open_repo()?;
    let has_rebase_state = state_path(&repo).exists();
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
        let status = Command::new("git")
            .arg("rebase")
            .arg("--continue")
            .status()?;
        if !status.success() {
            return Err(anyhow!(
                "git rebase --continue failed. Resolve conflicts and try again."
            ));
        }
    }

    if has_rebase_state {
        let state = load_state(&repo)?;
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
