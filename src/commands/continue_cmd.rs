use crate::rebase_utils::{load_state, run_rebase_loop};
use anyhow::{Result, anyhow};
use std::process::Command;

pub fn continue_cmd() -> Result<()> {
    let repo = crate::open_repo()?;
    let state = load_state(&repo)?;

    if repo.path().join("rebase-merge").exists() || repo.path().join("rebase-apply").exists() {
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

    run_rebase_loop(&repo, state)
}
