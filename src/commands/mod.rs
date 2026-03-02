pub mod abort_cmd;
pub mod checkout;
pub mod commit;
pub mod continue_cmd;
pub mod move_cmd;
pub mod pr;
pub mod push;
pub mod split;
pub mod status_cmd;

use anyhow::{Context, Result, anyhow};
use clap::Subcommand;
use git2::{BranchType, Repository};
use std::io::IsTerminal;

#[derive(Subcommand, Clone, Copy)]
pub enum CheckoutSubcommand {
    /// Checkout the branch above the current one
    Up,
    /// Checkout the branch below the current one
    Down,
    /// Checkout the top branch in the stack
    Top,
}

pub struct CommitInfo {
    pub id: String,
    pub summary: String,
}

pub fn prompt_select(message: &str, options: Vec<String>) -> Result<String> {
    if !std::io::stdin().is_terminal() {
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
    if !std::io::stdin().is_terminal() {
        println!("{} (non-interactive mode: auto-selecting NONE)", message);
        return Ok(Vec::new());
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
