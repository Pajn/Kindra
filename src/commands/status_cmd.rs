use crate::rebase_utils::{Operation, load_state};
use anyhow::Result;

pub fn status_cmd() -> Result<()> {
    let repo = crate::open_repo()?;
    let state = load_state(&repo)?;
    let op_name = match state.operation {
        Operation::Move => "Move",
        Operation::Commit => "Commit",
    };
    println!(
        "{} in progress: {} onto {}",
        op_name, state.original_branch, state.target_branch
    );
    println!(
        "Remaining branches: {}",
        state.remaining_branches.join(", ")
    );
    Ok(())
}
