use crate::rebase_utils::state_path;
use anyhow::{Result, anyhow};
use std::process::Command;

pub fn abort_cmd() -> Result<()> {
    let repo = crate::open_repo()?;
    let path = state_path(&repo);
    if path.exists() {
        // Only try to abort a git rebase if we were actually in a gits operation
        if repo.path().join("rebase-merge").exists() || repo.path().join("rebase-apply").exists() {
            println!("Aborting active git rebase...");
            let status = Command::new("git").arg("rebase").arg("--abort").status()?;
            if !status.success() {
                return Err(anyhow!("Failed to abort git rebase."));
            }
        }

        std::fs::remove_file(path)?;
        println!("Operation aborted (state cleared).");
    } else {
        println!("No operation in progress.");
    }

    Ok(())
}
