pub mod checkout;
pub mod commit;
pub mod move_cmd;
pub mod push;
pub mod split;

use anyhow::{Context, Result, anyhow};
use git2::{BranchType, Repository};

pub struct CommitInfo {
    pub id: String,
    pub summary: String,
}

pub fn prompt_select(message: &str, options: Vec<String>) -> Result<String> {
    if std::env::var("TERM").unwrap_or_default() == "dumb" {
        if options.is_empty() {
            return Err(anyhow!("No options available for selection"));
        }
        println!("{} (auto-selecting first option: {})", message, options[0]);
        return Ok(options[0].clone());
    }
    inquire::Select::new(message, options)
        .prompt()
        .context("Selection failed")
}

pub fn prompt_multi_select<T: std::fmt::Display>(message: &str, options: Vec<T>) -> Result<Vec<T>> {
    if std::env::var("TERM").unwrap_or_default() == "dumb" {
        println!("{} (auto-selecting all {} options)", message, options.len());
        return Ok(options);
    }
    inquire::MultiSelect::new(message, options)
        .prompt()
        .context("Multi-selection failed")
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
