use crate::operation_state::{KindraOperation, NativeOperation};
use crate::rebase_utils::{Operation, load_state};
use anyhow::Result;

pub fn status_cmd() -> Result<()> {
    let repo = crate::open_repo()?;
    // Reconcile under the lock so status reports what the next command will
    // see. If another kin holds the lock, report the saved state as it is
    // rather than blocking or writing behind that process's back.
    let lock = crate::state_io::RepoLock::try_acquire(&repo)?;
    match &lock {
        Some(lock) => crate::operation_state::reconcile(&repo, lock)?,
        None => println!(
            "Another 'kin' process is running in this repository; showing its saved state without reconciling it."
        ),
    }
    let active = crate::operation_state::query(&repo);

    if let Some(recovery) = active.override_recovery {
        if recovery.removing {
            println!(
                "Removing local overrides was interrupted. Run 'kin continue' to finish removing them."
            );
        } else {
            println!(
                "Local overrides are suspended or awaiting recovery. Run 'kin continue' or 'kin abort' after resolving the operation or apply-hook failure."
            );
        }
    }
    if active.overrides_disabled {
        println!(
            "Local overrides are disabled in this worktree. Run 'kin overrides apply' to re-enable them."
        );
    }

    match &active.kindra {
        KindraOperation::None => println!("No Kindra operation active."),
        KindraOperation::Conflicting(kinds) => {
            println!("{}", crate::operation_state::conflicting_advice(kinds))
        }
        KindraOperation::Hydration => println!(
            "Checkout hydration in progress. Run 'kin continue' to resume or 'kin abort' to stop."
        ),
        KindraOperation::Run => print_run_status(&repo)?,
        KindraOperation::Rebase(_) => {
            let state = load_state(&repo)?;
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
        }
    }

    if active.native != NativeOperation::None {
        println!("{}", active.native.advice());
    }
    Ok(())
}

fn print_run_status(repo: &git2::Repository) -> Result<()> {
    let run_state = crate::commands::run::load_run_state(repo)?;
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
    Ok(())
}
