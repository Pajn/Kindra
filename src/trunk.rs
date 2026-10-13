//! Trunk resolution: the long-lived branch a stack is built on.
//!
//! Lives below `commands` so stack discovery and worktree management can
//! resolve the trunk without depending on command code.

use anyhow::{Result, anyhow};
use git2::{BranchType, Oid, Repository};
use std::collections::HashSet;

/// Resolve the trunk: the repository config's `upstream_branch` if set (an
/// error if it names no branch), else the first of `init.defaultBranch`,
/// `main`, `master` and `trunk` that exists locally, then as `origin/<name>`.
/// `None` when nothing matches.
pub fn resolve_trunk(repo: &Repository) -> Result<Option<String>> {
    if let Some((configured, config_path)) = crate::config::configured_trunk(repo)? {
        return resolve_branch_name(repo, &configured)
            .map(Some)
            .ok_or_else(|| {
                anyhow!(
                    "Configured upstream branch '{}' in {} was not found",
                    configured,
                    config_path.display()
                )
            });
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

/// The commit a stack on `trunk` is measured from: the tip of `trunk`'s
/// remote-tracking branch when the local trunk is behind it, else the trunk's
/// own tip. Commits the remote merged but the local trunk has not pulled are
/// then trunk history, not stack work, so a branch left at one of them is not
/// a stack branch. A local trunk that is ahead or has diverged keeps its tip,
/// as does one whose remote-tracking commit can't be read: the stack is then
/// measured as it was before the remote trunk was consulted.
pub fn stack_base(repo: &Repository, trunk: &str) -> Result<Oid> {
    let local = repo.revparse_single(trunk)?.peel_to_commit()?.id();
    let remote = repo
        .find_branch(trunk, BranchType::Local)
        .and_then(|branch| branch.upstream())
        .ok()
        .and_then(|upstream| upstream.get().target());
    match remote {
        Some(remote)
            if remote != local && repo.graph_descendant_of(remote, local).unwrap_or(false) =>
        {
            Ok(remote)
        }
        _ => Ok(local),
    }
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
