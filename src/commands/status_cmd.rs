use crate::rebase_utils::{Operation, load_state};
use anyhow::Result;

pub fn status_cmd() -> Result<()> {
    let repo = crate::open_repo()?;
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
