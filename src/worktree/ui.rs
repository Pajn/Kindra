use anyhow::{Result, anyhow};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeListRow {
    pub role: String,
    pub branch: String,
    pub state: Vec<String>,
    pub path: PathBuf,
}

pub fn confirm_or_abort(message: &str, assume_yes: bool) -> Result<()> {
    if assume_yes || crate::commands::prompt_confirm(message)? {
        return Ok(());
    }

    Err(anyhow!("Aborted."))
}

pub fn print_list(rows: &[WorktreeListRow]) {
    let role_width = rows
        .iter()
        .map(|row| row.role.len())
        .max()
        .unwrap_or(0)
        .max("ROLE".len());
    let branch_width = rows
        .iter()
        .map(|row| row.branch.len())
        .max()
        .unwrap_or(0)
        .max("BRANCH".len());
    let state_width = rows
        .iter()
        .map(|row| format_state_flags(&row.state).len())
        .max()
        .unwrap_or(0)
        .max("STATE".len());

    println!(
        "{:<role_width$} {:<branch_width$} {:<state_width$} PATH",
        "ROLE",
        "BRANCH",
        "STATE",
        role_width = role_width,
        branch_width = branch_width,
        state_width = state_width
    );
    for row in rows {
        let state = format_state_flags(&row.state);
        println!(
            "{:<role_width$} {:<branch_width$} {:<state_width$} {}",
            row.role,
            row.branch,
            state,
            row.path.display(),
            role_width = role_width,
            branch_width = branch_width,
            state_width = state_width
        );
    }
}

pub fn format_state_flags(flags: &[String]) -> String {
    if flags.is_empty() {
        "clean".to_string()
    } else {
        flags.join(",")
    }
}
