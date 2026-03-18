use crate::rebase_utils::{
    Operation, git_rebase_in_progress, load_state, run_rebase_loop, state_path,
};
use anyhow::{Result, anyhow};
use std::process::Command;

pub fn continue_cmd() -> Result<()> {
    let repo = crate::open_repo()?;
    let has_state = state_path(&repo).exists();

    if git_rebase_in_progress(&repo) {
        if !has_state {
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

    if !has_state {
        println!("No operation in progress.");
        return Ok(());
    }

    let state = load_state(&repo)?;

    match state.operation {
        Operation::Sync => crate::commands::sync::finish_sync_after_rebase(&repo, state),
        _ => run_rebase_loop(&repo, state),
    }
}
