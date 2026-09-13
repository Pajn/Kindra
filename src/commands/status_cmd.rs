use crate::rebase_utils::{Operation, ReconcileMode, reconcile_saved_rebase_state};
use anyhow::Result;

pub fn status_cmd() -> Result<()> {
    let repo = crate::open_repo()?;
    if crate::overrides::state_path(&repo).exists() {
        println!(
            "Local overrides are suspended or awaiting recovery. Run 'kin continue' or 'kin abort' after resolving the operation or apply-hook failure."
        );
    }
    if crate::overrides::is_disabled(&repo) {
        println!(
            "Local overrides are disabled in this worktree. Run 'kin overrides apply' to re-enable them."
        );
    }
    if crate::commands::run::run_state_exists(&repo) {
        let run_state = crate::commands::run::load_run_state(&repo)?;
        let processed = run_state.current_index.min(run_state.target_branches.len());
        let status_name = match run_state.status {
            crate::commands::run::RunStatus::InProgress => "in progress",
            crate::commands::run::RunStatus::Failed => "failed",
            crate::commands::run::RunStatus::Aborted => "aborted",
        };
        println!(
            "Run {}: {} of {} branch(es) processed",
            status_name,
            processed,
            run_state.target_branches.len()
        );
        if processed < run_state.target_branches.len() {
            println!("Next branch: {}", run_state.target_branches[processed]);
        }
        if !run_state.failed_branches.is_empty() {
            println!("Failed branches: {}", run_state.failed_branches.join(", "));
        }
        if let Some(error) = run_state.last_error {
            println!("Last error: {}", error);
        }
        return Ok(());
    }

    let state = match reconcile_saved_rebase_state(&repo, ReconcileMode::Passive)? {
        Some(state) => state,
        None => {
            println!("No Kindra operation active.");
            return Ok(());
        }
    };
    let op_name = match state.operation {
        Operation::Move => "Move",
        Operation::Reorder => "Reorder",
        Operation::Commit => "Commit",
        Operation::Sync => "Sync",
    };
    if state.operation == Operation::Reorder {
        println!("{} in progress from {}", op_name, state.original_branch);
    } else {
        println!(
            "{} in progress: {} onto {}",
            op_name, state.original_branch, state.target_branch
        );
    }
    println!(
        "Remaining branches: {}",
        state.remaining_branches.join(", ")
    );
    Ok(())
}
