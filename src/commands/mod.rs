pub mod abort_cmd;
pub mod checkout;
pub mod commit;
pub mod continue_cmd;
pub mod move_cmd;
pub mod pr;
pub(crate) mod pr_merge;
pub mod push;
pub mod reorder;
pub mod restack;
pub mod run;
pub mod split;
pub mod status_cmd;
pub mod sync;
pub mod tree;
pub mod worktree;

use anyhow::{Context, Result, anyhow};
use clap::Subcommand;
use git2::{BranchType, Repository};
use serde::Deserialize;
use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static TEST_SELECTION_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

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
        if let Ok(selection_values) = std::env::var("KIN_TEST_SELECTIONS") {
            let call_index = TEST_SELECTION_CALL_COUNT.fetch_add(1, Ordering::Relaxed);
            let selected_idx = selection_values
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .nth(call_index)
                .and_then(|s| s.parse::<usize>().ok());

            if let Some(idx) = selected_idx
                && idx < options.len()
            {
                println!("Options:");
                for (i, opt) in options.iter().enumerate() {
                    println!("{}: {}", i, opt);
                }
                println!(
                    "{} (test override: auto-selecting option {})",
                    message, options[idx]
                );
                return Ok(options[idx].clone());
            }
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

pub fn find_upstream(repo: &Repository) -> Result<Option<String>> {
    if let Some(upstream) = read_repo_upstream_override(repo)? {
        return Ok(Some(upstream));
    }

    let mut candidates = Vec::new();
    if let Ok(default_branch) = repo.config()?.get_string("init.defaultBranch") {
        let default_branch = default_branch.trim();
        if !default_branch.is_empty() {
            candidates.push(default_branch.to_string());
        }
    }
    candidates.extend(["main", "master", "trunk"].iter().map(|s| s.to_string()));

    let mut seen = HashSet::new();
    candidates.retain(|candidate| seen.insert(candidate.clone()));

    for name in &candidates {
        if repo.find_branch(name, BranchType::Local).is_ok() {
            return Ok(Some(name.clone()));
        }
    }

    let mut remote_candidates = Vec::new();
    for name in &candidates {
        if !name.starts_with("origin/") {
            remote_candidates.push(format!("origin/{name}"));
        }
    }

    for name in remote_candidates {
        if branch_exists(repo, &name) {
            return Ok(Some(name));
        }
    }

    Ok(None)
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
    restack: Option<RestackConfig>,
    rebase: Option<RebaseConfig>,
}

#[derive(Deserialize, Clone, Copy)]
struct RestackConfig {
    history_limit: Option<usize>,
}

#[derive(Deserialize, Clone, Copy)]
struct RebaseConfig {
    autostash: Option<bool>,
}

#[derive(Deserialize)]
struct GlobalConfig {
    restack: Option<RestackConfig>,
    rebase: Option<RebaseConfig>,
}

pub const DEFAULT_RESTACK_HISTORY_LIMIT: usize = 100;

pub fn resolve_restack_history_limit(
    repo: &Repository,
    cli_override: Option<usize>,
) -> Result<usize> {
    if let Some(limit) = cli_override {
        return Ok(limit);
    }

    if let Some(limit) = read_repo_config(repo)?
        .restack
        .and_then(|cfg| cfg.history_limit)
    {
        return Ok(limit);
    }

    if let Some(limit) =
        read_global_config()?.and_then(|cfg| cfg.restack.and_then(|r| r.history_limit))
    {
        return Ok(limit);
    }

    Ok(DEFAULT_RESTACK_HISTORY_LIMIT)
}

pub fn resolve_rebase_autostash(repo: &Repository, cli_override: Option<bool>) -> Result<bool> {
    if let Some(autostash) = cli_override {
        return Ok(autostash);
    }

    if let Some(autostash) = read_repo_config(repo)?.rebase.and_then(|cfg| cfg.autostash) {
        return Ok(autostash);
    }

    if let Some(autostash) =
        read_global_config()?.and_then(|cfg| cfg.rebase.and_then(|r| r.autostash))
    {
        return Ok(autostash);
    }

    // Check git's native rebase.autostash config
    if let Ok(autostash) = repo.config()?.get_bool("rebase.autostash") {
        return Ok(autostash);
    }

    Ok(false)
}

fn read_repo_upstream_override(repo: &Repository) -> Result<Option<String>> {
    let cfg = read_repo_config(repo)?;

    let upstream = cfg
        .upstream_branch
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    match upstream {
        Some(upstream) => resolve_branch_name(repo, &upstream)
            .map(Some)
            .ok_or_else(|| {
                anyhow!(
                    "Configured upstream branch '{}' in .git/kindra.toml was not found",
                    upstream
                )
            }),
        None => Ok(None),
    }
}

fn read_repo_config(repo: &Repository) -> Result<RepoConfig> {
    read_toml_config(repo.path().join("kindra.toml"), "repository")?.map_or_else(
        || {
            Ok(RepoConfig {
                upstream_branch: None,
                restack: None,
                rebase: None,
            })
        },
        Ok,
    )
}

fn read_global_config() -> Result<Option<GlobalConfig>> {
    let Some(config_path) = global_config_path() else {
        return Ok(None);
    };
    read_toml_config(config_path, "global")
}

fn read_toml_config<T: for<'de> Deserialize<'de>>(
    config_path: PathBuf,
    config_kind: &str,
) -> Result<Option<T>> {
    if !config_path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&config_path).with_context(|| {
        format!(
            "Failed to read {config_kind} config at {}",
            config_path.display()
        )
    })?;
    let cfg = toml::from_str(&raw).with_context(|| {
        format!(
            "Failed to parse {config_kind} config at {}",
            config_path.display()
        )
    })?;
    Ok(Some(cfg))
}

fn global_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("kindra").join("config.toml"))
}
