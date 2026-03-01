pub mod checkout;
pub mod commit;
pub mod move_cmd;
pub mod push;
pub mod split;

use anyhow::{Result, anyhow};
use git2::{BranchType, Repository};

pub struct CommitInfo {
    pub id: String,
    pub summary: String,
}

pub fn find_upstream(repo: &Repository) -> Result<String> {
    let candidates = ["main", "master", "origin/main", "origin/master"];
    for name in candidates {
        if repo.revparse_single(name).is_ok() {
            // Check if it's a local branch first
            if repo.find_branch(name, BranchType::Local).is_ok() {
                return Ok(name.to_string());
            }
        }
    }
    // Fallback to the first candidate that exists at all
    for name in candidates {
        if repo.revparse_single(name).is_ok() {
            return Ok(name.to_string());
        }
    }
    Err(anyhow!("Could not find a base branch (main or master)"))
}
