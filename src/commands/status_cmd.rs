use crate::rebase_utils::{Operation, load_state};
use anyhow::Result;

pub fn status_cmd() -> Result<()> {
    let repo = crate::open_repo()?;
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

    let state = match load_state(&repo) {
        Ok(state) => state,
        Err(_) => {
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
