use crate::rebase_utils::{
    apply_stash, checkout_branch, drop_stash, load_state, save_state, state_path, unstage_all,
};
use anyhow::{Result, anyhow};
use std::process::Command;

pub fn abort_cmd() -> Result<()> {
    let repo = crate::open_repo()?;
    let path = state_path(&repo);
    if path.exists() {
        let mut parsed_state = load_state(&repo)?;

        // Only try to abort a git rebase if we were actually in a gits operation
        if repo.path().join("rebase-merge").exists() || repo.path().join("rebase-apply").exists() {
            println!("Aborting active git rebase...");
            let status = Command::new("git").arg("rebase").arg("--abort").status()?;
            if !status.success() {
                return Err(anyhow!("Failed to abort git rebase."));
            }
        }

        let restore_branch = parsed_state
            .caller_branch
            .clone()
            .unwrap_or_else(|| parsed_state.original_branch.clone());
        checkout_branch(&restore_branch)?;
        if let Some(stash_ref) = parsed_state.stash_ref.clone() {
            apply_stash(&stash_ref)?;
            parsed_state.stash_ref = None;
            save_state(&repo, &parsed_state)?;
            if let Err(err) = drop_stash(&stash_ref) {
                eprintln!("Warning: {}", err);
            }
        }
        if parsed_state.unstage_on_restore {
            unstage_all()?;
        }

        std::fs::remove_file(path)?;
        println!("Operation aborted (state cleared).");
    } else {
        println!("No operation in progress.");
    }

    Ok(())
}
