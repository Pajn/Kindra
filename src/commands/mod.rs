pub mod abort_cmd;
pub mod checkout;
pub mod commit;
pub mod continue_cmd;
pub mod move_cmd;
pub mod pr;
pub mod push;
pub mod split;
pub mod status_cmd;
pub mod sync;

use anyhow::{Context, Result, anyhow};
use clap::Subcommand;
use git2::{BranchType, Repository};
use serde::Deserialize;
use std::collections::HashSet;
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

pub fn prompt_confirm(message: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        println!("{} (non-interactive mode: auto-denying)", message);
        return Ok(false);
    }
    inquire::Confirm::new(message)
        .with_default(false)
        .prompt()
        .context("Confirmation failed")
}

pub fn find_upstream(repo: &Repository) -> Result<String> {
    if let Some(upstream) = read_repo_upstream_override(repo)? {
        return Ok(upstream);
    }

    let mut local_candidates = Vec::new();
    if let Ok(default_branch) = repo.config()?.get_string("init.defaultBranch") {
        let default_branch = default_branch.trim();
        if !default_branch.is_empty() {
            local_candidates.push(default_branch.to_string());
        }
    }
    local_candidates.extend(["main", "master", "trunk"].iter().map(|s| s.to_string()));

    let mut seen_local = HashSet::new();
    local_candidates.retain(|candidate| seen_local.insert(candidate.clone()));

    for name in &local_candidates {
        if repo.find_branch(name, BranchType::Local).is_ok() {
            return Ok(name.clone());
        }
    }

    let mut candidates = local_candidates.clone();
    for name in &local_candidates {
        if !name.starts_with("origin/") {
            candidates.push(format!("origin/{name}"));
        }
    }
    let mut seen_candidates = HashSet::new();
    candidates.retain(|candidate| seen_candidates.insert(candidate.clone()));

    for name in candidates {
        if branch_exists(repo, &name) {
            return Ok(name);
        }
    }

    Err(anyhow!(
        "Could not find a base branch (init.defaultBranch, main, master, or trunk)"
    ))
}

fn branch_exists(repo: &Repository, name: &str) -> bool {
    repo.find_branch(name, BranchType::Local).is_ok()
        || repo.find_branch(name, BranchType::Remote).is_ok()
}

fn resolve_branch_name(repo: &Repository, name: &str) -> Option<String> {
    if branch_exists(repo, name) {
        return Some(name.to_string());
    }

    if !name.starts_with("origin/") {
        let origin_name = format!("origin/{name}");
        if branch_exists(repo, &origin_name) {
            return Some(origin_name);
        }
    }

    None
}

#[derive(Deserialize)]
struct RepoConfig {
    upstream_branch: Option<String>,
}

fn read_repo_upstream_override(repo: &Repository) -> Result<Option<String>> {
    let config_path = repo.path().join("gits.toml");
    if !config_path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&config_path).with_context(|| {
        format!(
            "Failed to read repository config at {}",
            config_path.display()
        )
    })?;
    let cfg: RepoConfig = toml::from_str(&raw).with_context(|| {
        format!(
            "Failed to parse repository config at {}",
            config_path.display()
        )
    })?;

    let upstream = cfg
        .upstream_branch
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    match upstream {
        Some(upstream) => resolve_branch_name(repo, &upstream)
            .map(Some)
            .ok_or_else(|| {
                anyhow!(
                    "Configured upstream branch '{}' in .git/gits.toml was not found",
                    upstream
                )
            }),
        None => Ok(None),
    }
}
