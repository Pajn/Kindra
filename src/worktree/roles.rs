use crate::worktree::WorktreeRole;
use crate::worktree::cleanup::{CleanupReason, find_cleanup_candidates};
use crate::worktree::config::{WorktreeConfig, load_worktree_config};
use crate::worktree::git::{
    LiveWorktree, add_worktree, checkout_worktree_branch, checkout_worktree_detached,
    current_branch, current_head_oid, ensure_local_branch_exists,
    ensure_local_branch_exists_from_start_point, is_worktree_dirty, list_live_worktrees,
    live_worktree_map, remove_worktree,
};
use crate::worktree::hooks::{HookEvent, run_hooks};
use crate::worktree::metadata::{ManagedWorktreeRecord, WorktreeMetadata};
use crate::worktree::path_resolver::{
    WorktreeTarget, expand_path_template, normalize_path, parse_target, temp_template_root,
};
use crate::worktree::ui::{WorktreeListRow, confirm_or_abort};
use anyhow::{Result, anyhow};
use git2::{BranchType, ErrorCode, Repository};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnsureResult {
    pub path: PathBuf,
    pub created: bool,
    pub switched: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoveResult {
    pub role: WorktreeRole,
    pub branch: String,
    pub path: PathBuf,
    pub metadata_only: bool,
}

#[derive(Clone, Debug, Default)]
pub struct CleanupSummary {
    pub candidates: usize,
    pub removed: Vec<RemoveResult>,
    pub skipped: usize,
}

pub fn ensure_main(repo: &Repository) -> Result<EnsureResult> {
    let mut ctx = load_context(repo)?;
    if !ctx.config.main.enabled {
        return Err(anyhow!("Main worktrees are disabled in .git/kindra.toml."));
    }

    let branch = ctx.config.main.branch.clone();
    ensure_local_branch_exists_from_start_point(repo, &branch, &ctx.config.trunk)?;
    let path = ctx.config.main.path.clone();
    let live = ctx.live_by_path().get(&normalize_path(&path)).cloned();

    if let Some(live) = live {
        if live.branch.as_deref() != Some(branch.as_str()) {
            return Err(anyhow!(
                "Managed main worktree at '{}' is on '{}' but should stay pinned to '{}'.",
                path.display(),
                live.branch.unwrap_or_else(|| "<detached>".to_string()),
                branch
            ));
        }

        ctx.metadata.upsert(WorktreeRole::Main, &branch, &path);
        ctx.metadata.save(repo)?;
        return Ok(EnsureResult {
            path,
            created: false,
            switched: false,
        });
    }

    if path.exists() {
        return Err(anyhow!(
            "Configured main worktree path '{}' exists but is not a valid git worktree.",
            path.display()
        ));
    }

    add_worktree(repo, &path, &branch)?;
    run_create_hooks(repo, &ctx.config, WorktreeRole::Main, &path, &branch)?;
    ctx.metadata.upsert(WorktreeRole::Main, &branch, &path);
    ctx.metadata.save(repo)?;

    Ok(EnsureResult {
        path,
        created: true,
        switched: false,
    })
}

pub fn ensure_review(
    repo: &Repository,
    requested_branch: Option<&str>,
    force: bool,
) -> Result<EnsureResult> {
    let mut ctx = load_context(repo)?;
    if !ctx.config.review.enabled {
        return Err(anyhow!(
            "Review worktrees are disabled in .git/kindra.toml."
        ));
    }
    if !ctx.config.review.reuse {
        return Err(anyhow!(
            "worktrees.review.reuse = false is not supported by the current MVP."
        ));
    }

    let branch = resolve_requested_branch(repo, requested_branch)?;
    ensure_local_branch_exists(repo, &branch)?;
    let path = ctx.config.review.path.clone();
    let live = ctx.live_by_path().get(&normalize_path(&path)).cloned();

    if let Some(live) = live {
        if live.branch.as_deref() == Some(branch.as_str()) {
            ctx.metadata.upsert(WorktreeRole::Review, &branch, &path);
            ctx.metadata.save(repo)?;
            return Ok(EnsureResult {
                path,
                created: false,
                switched: false,
            });
        }

        let dirty = is_worktree_dirty(&path)?;
        let discard_local_changes = force || (dirty && ctx.config.review.clean_before_switch);
        if dirty && !force && ctx.config.review.clean_before_switch {
            confirm_or_abort(
                &format!(
                    "Review worktree '{}' has uncommitted changes. Discard them and switch to '{}'?",
                    path.display(),
                    branch
                ),
                false,
            )?;
        }

        let rollback = live
            .branch
            .clone()
            .map(RollbackTarget::Branch)
            .map_or_else(|| current_head_oid(&path).map(RollbackTarget::Detached), Ok)?;
        checkout_worktree_branch(&path, &branch, discard_local_changes)?;
        run_checkout_hooks(
            &ctx.config,
            &path,
            &branch,
            dirty,
            discard_local_changes,
            &rollback,
        )?;
        ctx.metadata.upsert(WorktreeRole::Review, &branch, &path);
        ctx.metadata.save(repo)?;
        return Ok(EnsureResult {
            path,
            created: false,
            switched: true,
        });
    }

    if path.exists() {
        return Err(anyhow!(
            "Configured review worktree path '{}' exists but is not a valid git worktree.",
            path.display()
        ));
    }

    add_worktree(repo, &path, &branch)?;
    run_create_hooks(repo, &ctx.config, WorktreeRole::Review, &path, &branch)?;
    ctx.metadata.upsert(WorktreeRole::Review, &branch, &path);
    ctx.metadata.save(repo)?;

    Ok(EnsureResult {
        path,
        created: true,
        switched: false,
    })
}

pub fn ensure_temp(repo: &Repository, requested_branch: Option<&str>) -> Result<EnsureResult> {
    let mut ctx = load_context(repo)?;
    if !ctx.config.temp.enabled {
        return Err(anyhow!("Temp worktrees are disabled in .git/kindra.toml."));
    }

    let branch = resolve_requested_branch(repo, requested_branch)?;
    ensure_local_branch_exists(repo, &branch)?;
    let path = ctx.metadata.find_temp_branch(&branch).map_or_else(
        || expand_path_template(&ctx.config.temp.path_template, &branch),
        |record| Ok(record.path_buf()),
    )?;

    ensure_temp_path_available(
        &ctx.config,
        &ctx.metadata,
        &ctx.live_worktrees,
        &branch,
        &path,
    )?;
    let live = ctx.live_by_path().get(&normalize_path(&path)).cloned();

    if let Some(live) = live {
        if live.branch.as_deref() != Some(branch.as_str()) {
            return Err(anyhow!(
                "Managed temp worktree path '{}' is already associated with branch '{}'.",
                path.display(),
                live.branch.unwrap_or_else(|| "<detached>".to_string())
            ));
        }

        ctx.metadata.upsert(WorktreeRole::Temp, &branch, &path);
        ctx.metadata.save(repo)?;
        return Ok(EnsureResult {
            path,
            created: false,
            switched: false,
        });
    }

    if path.exists() {
        return Err(anyhow!(
            "Configured temp worktree path '{}' exists but is not a valid git worktree.",
            path.display()
        ));
    }

    add_worktree(repo, &path, &branch)?;
    run_create_hooks(repo, &ctx.config, WorktreeRole::Temp, &path, &branch)?;
    ctx.metadata.upsert(WorktreeRole::Temp, &branch, &path);
    ctx.metadata.save(repo)?;

    Ok(EnsureResult {
        path,
        created: true,
        switched: false,
    })
}

pub fn ensure_temp_new_branch(
    repo: &Repository,
    branch: &str,
    requested_start_point: Option<&str>,
) -> Result<EnsureResult> {
    let mut ctx = load_context(repo)?;
    if !ctx.config.temp.enabled {
        return Err(anyhow!("Temp worktrees are disabled in .git/kindra.toml."));
    }

    ensure_local_branch_is_new(repo, branch)?;
    let start_point = resolve_requested_start_point(repo, requested_start_point)?;
    let path = ctx.metadata.find_temp_branch(branch).map_or_else(
        || expand_path_template(&ctx.config.temp.path_template, branch),
        |record| Ok(record.path_buf()),
    )?;

    ensure_temp_path_available(
        &ctx.config,
        &ctx.metadata,
        &ctx.live_worktrees,
        branch,
        &path,
    )?;

    if path.exists() {
        return Err(anyhow!(
            "Configured temp worktree path '{}' exists but is not a valid git worktree.",
            path.display()
        ));
    }

    ensure_local_branch_exists_from_start_point(repo, branch, &start_point)?;
    add_worktree(repo, &path, branch)?;
    run_create_hooks(repo, &ctx.config, WorktreeRole::Temp, &path, branch)?;
    ctx.metadata.upsert(WorktreeRole::Temp, branch, &path);
    ctx.metadata.save(repo)?;

    Ok(EnsureResult {
        path,
        created: true,
        switched: false,
    })
}

pub fn resolve_existing_path(repo: &Repository, target: &str) -> Result<PathBuf> {
    let ctx = load_context(repo)?;
    let resolved = resolve_target(&ctx, target, false)?;
    Ok(resolved.path)
}

pub fn list_managed_worktrees(repo: &Repository) -> Result<Vec<WorktreeListRow>> {
    let ctx = load_context(repo)?;
    let current_path =
        normalize_path(repo.workdir().ok_or_else(|| {
            anyhow!("Kindra worktree management requires a non-bare repository.")
        })?);
    let live_by_path = ctx.live_by_path();
    let merged_branches = if ctx.config.temp.delete_merged {
        crate::stack::collect_merged_local_branches(
            repo,
            &ctx.config.trunk,
            &[ctx.config.trunk.as_str()],
        )?
        .into_iter()
        .collect::<HashSet<_>>()
    } else {
        HashSet::new()
    };

    let mut rows = Vec::new();
    let mut seen_paths = HashSet::new();

    for record in ctx.metadata.records() {
        let path = record.path_buf();
        let normalized = record.normalized_path();
        let live = live_by_path.get(&normalized);
        let mut state = Vec::new();

        if let Some(live) = live {
            if is_worktree_dirty(&path)? {
                state.push("dirty".to_string());
            }
            if normalized == current_path {
                state.push("current".to_string());
            }
            if live.branch.as_deref() != Some(record.branch.as_str()) {
                state.push("stale-meta".to_string());
            }
        } else if !path.exists() {
            state.push("missing".to_string());
        } else {
            state.push("stale-meta".to_string());
        }

        if record.role == WorktreeRole::Temp && merged_branches.contains(&record.branch) {
            state.push("merged".to_string());
        }

        rows.push(WorktreeListRow {
            role: record.role.to_string(),
            branch: record.branch.clone(),
            state,
            path: path.clone(),
        });
        seen_paths.insert(normalized);
    }

    let inferred = inferred_live_rows(
        &ctx.config,
        &ctx.live_worktrees,
        &merged_branches,
        &current_path,
    )?;
    for row in inferred {
        if seen_paths.insert(normalize_path(&row.path)) {
            rows.push(row);
        }
    }

    rows.sort_by(|left, right| {
        left.role
            .cmp(&right.role)
            .then_with(|| left.branch.cmp(&right.branch))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(rows)
}

pub fn remove_target(
    repo: &Repository,
    target: &str,
    assume_yes: bool,
    force: bool,
) -> Result<RemoveResult> {
    let mut ctx = load_context(repo)?;
    let resolved = resolve_target(&ctx, target, true)?;
    let dirty = resolved
        .live
        .as_ref()
        .map(|_| is_worktree_dirty(&resolved.path))
        .transpose()?
        .unwrap_or(false);

    if dirty && !force {
        return Err(anyhow!(
            "Worktree '{}' for {} '{}' has uncommitted changes. Re-run with --force to remove it.",
            resolved.path.display(),
            resolved.role,
            resolved.branch
        ));
    }

    let message = if dirty {
        format!(
            "Worktree '{}' for {} '{}' has uncommitted changes. Remove it anyway?",
            resolved.path.display(),
            resolved.role,
            resolved.branch
        )
    } else {
        format!(
            "Remove {} worktree '{}' at '{}'?",
            resolved.role,
            resolved.branch,
            resolved.path.display()
        )
    };
    confirm_or_abort(&message, assume_yes)?;

    let metadata_only =
        remove_resolved_target(repo, &ctx.config, &mut ctx.metadata, &resolved, force)?;
    ctx.metadata.save(repo)?;

    Ok(RemoveResult {
        role: resolved.role,
        branch: resolved.branch,
        path: resolved.path,
        metadata_only,
    })
}

pub fn cleanup_temp_worktrees(
    repo: &Repository,
    assume_yes: bool,
    force: bool,
) -> Result<CleanupSummary> {
    let mut ctx = load_context(repo)?;
    let candidates =
        find_cleanup_candidates(repo, &ctx.config, &ctx.metadata, &ctx.live_worktrees)?;
    if candidates.is_empty() {
        return Ok(CleanupSummary::default());
    }
    let candidates_with_dirty = candidates
        .into_iter()
        .map(|candidate| {
            let dirty = candidate
                .live
                .as_ref()
                .map(|_| is_worktree_dirty(&candidate.record.path_buf()))
                .transpose()?
                .unwrap_or(false);
            Ok((candidate, dirty))
        })
        .collect::<Result<Vec<_>>>()?;
    let dirty_count = candidates_with_dirty
        .iter()
        .filter(|(_, dirty)| *dirty)
        .count();

    println!("Cleanup candidates:");
    for (candidate, dirty) in &candidates_with_dirty {
        println!(
            "  temp {:<20} {:<14} {}{}",
            candidate.record.branch,
            match candidate.reason {
                CleanupReason::Merged => "merged",
                CleanupReason::StaleMetadata => "stale-meta",
            },
            candidate.record.path,
            if *dirty { " [dirty]" } else { "" }
        );
    }

    let confirmation = if dirty_count == 0 {
        format!(
            "Remove {} temp worktree candidate(s)?",
            candidates_with_dirty.len()
        )
    } else if force {
        format!(
            "Remove {} temp worktree candidate(s)? {} dirty candidate(s) will be removed.",
            candidates_with_dirty.len(),
            dirty_count
        )
    } else {
        format!(
            "Remove {} temp worktree candidate(s)? {} dirty candidate(s) will be skipped without --force.",
            candidates_with_dirty.len(),
            dirty_count
        )
    };
    confirm_or_abort(&confirmation, assume_yes)?;

    let mut removed = Vec::new();
    let mut skipped = 0usize;
    for (candidate, dirty) in candidates_with_dirty {
        let resolved = ResolvedTarget {
            role: WorktreeRole::Temp,
            branch: candidate.record.branch.clone(),
            path: candidate.record.path_buf(),
            live: candidate.live.clone(),
        };

        if dirty && !force {
            println!(
                "Skipping dirty temp worktree '{}' at '{}'. Re-run with --force to remove it.",
                resolved.branch,
                resolved.path.display()
            );
            skipped += 1;
            continue;
        }
        let metadata_only =
            remove_resolved_target(repo, &ctx.config, &mut ctx.metadata, &resolved, force)?;
        removed.push(RemoveResult {
            role: resolved.role,
            branch: resolved.branch,
            path: resolved.path,
            metadata_only,
        });
        std::mem::take(&mut ctx.metadata).save(repo)?;
        ctx.metadata = WorktreeMetadata::load(repo)?;
    }

    Ok(CleanupSummary {
        candidates: removed.len() + skipped,
        removed,
        skipped,
    })
}

fn inferred_live_rows(
    config: &WorktreeConfig,
    live_worktrees: &[LiveWorktree],
    merged_branches: &HashSet<String>,
    current_path: &Path,
) -> Result<Vec<WorktreeListRow>> {
    let temp_root = temp_template_root(&config.temp.path_template)?;
    let mut rows = Vec::new();

    for live in live_worktrees {
        let normalized = live.normalized_path();
        let role = if normalized == normalize_path(&config.main.path) {
            Some(WorktreeRole::Main)
        } else if normalized == normalize_path(&config.review.path) {
            Some(WorktreeRole::Review)
        } else if normalized.starts_with(&temp_root) {
            Some(WorktreeRole::Temp)
        } else {
            None
        };

        let Some(role) = role else {
            continue;
        };

        let mut state = vec!["stale-meta".to_string()];
        if is_worktree_dirty(&live.path)? {
            state.push("dirty".to_string());
        }
        if normalized == normalize_path(current_path) {
            state.push("current".to_string());
        }
        if role == WorktreeRole::Temp
            && let Some(branch) = &live.branch
            && merged_branches.contains(branch)
        {
            state.push("merged".to_string());
        }

        rows.push(WorktreeListRow {
            role: role.to_string(),
            branch: live
                .branch
                .clone()
                .unwrap_or_else(|| "<detached>".to_string()),
            state,
            path: live.path.clone(),
        });
    }

    Ok(rows)
}

fn resolve_requested_branch(repo: &Repository, requested_branch: Option<&str>) -> Result<String> {
    match requested_branch {
        Some(branch) => Ok(branch.to_string()),
        None => current_branch(repo)?.ok_or_else(|| {
            anyhow!("Current HEAD is detached; please specify a branch explicitly.")
        }),
    }
}

fn resolve_requested_start_point(
    repo: &Repository,
    requested_start_point: Option<&str>,
) -> Result<String> {
    match requested_start_point {
        Some(start_point) => Ok(start_point.to_string()),
        None => current_branch(repo)?.ok_or_else(|| {
            anyhow!("Current HEAD is detached; please specify a start point explicitly.")
        }),
    }
}

fn ensure_local_branch_is_new(repo: &Repository, branch: &str) -> Result<()> {
    match repo.find_branch(branch, BranchType::Local) {
        Ok(_) => Err(anyhow!("A local branch named '{}' already exists.", branch)),
        Err(err) if err.code() == ErrorCode::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn ensure_temp_path_available(
    config: &WorktreeConfig,
    metadata: &WorktreeMetadata,
    live_worktrees: &[LiveWorktree],
    branch: &str,
    path: &Path,
) -> Result<()> {
    let normalized = normalize_path(path);
    let existing_metadata = metadata.find_by_path(path);
    if let Some(other) = existing_metadata
        && (other.role != WorktreeRole::Temp || other.branch != branch)
    {
        return Err(anyhow!(
            "Temp worktree path '{}' is already reserved for {} '{}'.",
            path.display(),
            other.role,
            other.branch
        ));
    }

    if let Some(other_live) = live_worktree_map(live_worktrees).get(&normalized) {
        let reserved_role = existing_metadata.map(|record| record.role).or_else(|| {
            if normalize_path(&config.main.path) == normalized {
                Some(WorktreeRole::Main)
            } else if normalize_path(&config.review.path) == normalized {
                Some(WorktreeRole::Review)
            } else {
                None
            }
        });
        let is_matching_temp = matches!(
            existing_metadata,
            Some(record) if record.role == WorktreeRole::Temp && record.branch == branch
        );

        if !is_matching_temp {
            return Err(anyhow!(
                "Temp worktree path '{}' is already in use by {}.",
                path.display(),
                reserved_role
                    .map(|role| format!(
                        "{} '{}'",
                        role,
                        other_live.branch.clone().unwrap_or_default()
                    ))
                    .unwrap_or_else(|| {
                        format!(
                            "branch '{}'",
                            other_live
                                .branch
                                .clone()
                                .unwrap_or_else(|| "<detached>".to_string())
                        )
                    })
            ));
        }

        if other_live.branch.as_deref() != Some(branch) {
            return Err(anyhow!(
                "Temp worktree path '{}' is already in use by branch '{}'.",
                path.display(),
                other_live
                    .branch
                    .clone()
                    .unwrap_or_else(|| "<detached>".to_string())
            ));
        }
    }

    Ok(())
}

fn remove_resolved_target(
    repo: &Repository,
    config: &WorktreeConfig,
    metadata: &mut WorktreeMetadata,
    resolved: &ResolvedTarget,
    force: bool,
) -> Result<bool> {
    if let Some(_live) = &resolved.live {
        run_hooks(
            config,
            resolved.role,
            HookEvent::Remove,
            &resolved.path,
            &resolved.branch,
        )?;
        remove_worktree(repo, &resolved.path, force)?;
    } else if resolved.path.exists() {
        return Err(anyhow!(
            "Managed worktree path '{}' exists on disk but git does not recognize it as a live worktree.",
            resolved.path.display()
        ));
    }

    match resolved.role {
        WorktreeRole::Main => metadata.remove_role(WorktreeRole::Main),
        WorktreeRole::Review => metadata.remove_role(WorktreeRole::Review),
        WorktreeRole::Temp => metadata.remove_temp_branch(&resolved.branch),
    }

    Ok(resolved.live.is_none())
}

fn resolve_target(
    ctx: &LoadedContext,
    target: &str,
    allow_missing_path_metadata: bool,
) -> Result<ResolvedTarget> {
    let live_by_path = ctx.live_by_path();
    match parse_target(target) {
        WorktreeTarget::Role(WorktreeRole::Main) => {
            let metadata = ctx.metadata.find_role(WorktreeRole::Main).cloned();
            let path = metadata
                .as_ref()
                .map(ManagedWorktreeRecord::path_buf)
                .unwrap_or_else(|| ctx.config.main.path.clone());
            let live = live_by_path.get(&normalize_path(&path)).cloned();
            if live.is_none() && metadata.is_none() {
                return Err(anyhow!("No managed main worktree exists."));
            }
            if live.is_none() && !allow_missing_path_metadata {
                return Err(anyhow!("No managed main worktree exists."));
            }
            Ok(ResolvedTarget {
                role: WorktreeRole::Main,
                branch: live
                    .as_ref()
                    .and_then(|worktree| worktree.branch.clone())
                    .or_else(|| metadata.as_ref().map(|record| record.branch.clone()))
                    .unwrap_or_else(|| ctx.config.main.branch.clone()),
                path,
                live,
            })
        }
        WorktreeTarget::Role(WorktreeRole::Review) => {
            let metadata = ctx.metadata.find_role(WorktreeRole::Review).cloned();
            let path = metadata
                .as_ref()
                .map(ManagedWorktreeRecord::path_buf)
                .unwrap_or_else(|| ctx.config.review.path.clone());
            let live = live_by_path.get(&normalize_path(&path)).cloned();
            if live.is_none() && metadata.is_none() {
                return Err(anyhow!("No managed review worktree exists."));
            }
            if live.is_none() && !allow_missing_path_metadata {
                return Err(anyhow!("No managed review worktree exists."));
            }
            Ok(ResolvedTarget {
                role: WorktreeRole::Review,
                branch: live
                    .as_ref()
                    .and_then(|worktree| worktree.branch.clone())
                    .or_else(|| metadata.as_ref().map(|record| record.branch.clone()))
                    .unwrap_or_else(|| "review".to_string()),
                path,
                live,
            })
        }
        WorktreeTarget::Role(WorktreeRole::Temp) => unreachable!(),
        WorktreeTarget::TempBranch(branch) => {
            let metadata = ctx.metadata.find_temp_branch(&branch).cloned();
            let path = metadata.as_ref().map_or_else(
                || expand_path_template(&ctx.config.temp.path_template, &branch),
                |record| Ok(record.path_buf()),
            )?;
            let live = live_by_path.get(&normalize_path(&path)).cloned();
            if live.is_none() && metadata.is_none() {
                return Err(anyhow!(
                    "No managed temp worktree exists for branch '{}'.",
                    branch
                ));
            }
            if live.is_none() && !allow_missing_path_metadata {
                return Err(anyhow!(
                    "No managed temp worktree exists for branch '{}'.",
                    branch
                ));
            }
            Ok(ResolvedTarget {
                role: WorktreeRole::Temp,
                branch,
                path,
                live,
            })
        }
    }
}

fn load_context(repo: &Repository) -> Result<LoadedContext> {
    Ok(LoadedContext {
        config: load_worktree_config(repo)?,
        metadata: WorktreeMetadata::load(repo)?,
        live_worktrees: list_live_worktrees(repo)?,
    })
}

struct LoadedContext {
    config: WorktreeConfig,
    metadata: WorktreeMetadata,
    live_worktrees: Vec<LiveWorktree>,
}

impl LoadedContext {
    fn live_by_path(&self) -> std::collections::HashMap<PathBuf, LiveWorktree> {
        live_worktree_map(&self.live_worktrees)
    }
}

#[derive(Clone, Debug)]
struct ResolvedTarget {
    role: WorktreeRole,
    branch: String,
    path: PathBuf,
    live: Option<LiveWorktree>,
}

enum RollbackTarget {
    Branch(String),
    Detached(String),
}

fn run_create_hooks(
    repo: &Repository,
    config: &WorktreeConfig,
    role: WorktreeRole,
    path: &Path,
    branch: &str,
) -> Result<()> {
    if let Err(hook_err) = run_hooks(config, role, HookEvent::Create, path, branch) {
        return Err(rollback_created_worktree(repo, path, hook_err));
    }
    Ok(())
}

fn rollback_created_worktree(
    repo: &Repository,
    path: &Path,
    hook_err: anyhow::Error,
) -> anyhow::Error {
    match remove_worktree(repo, path, true) {
        Ok(()) => hook_err,
        Err(remove_err) => anyhow!(
            "{hook_err}\nAdditionally failed to roll back worktree '{}': {remove_err}",
            path.display()
        ),
    }
}

fn run_checkout_hooks(
    config: &WorktreeConfig,
    path: &Path,
    branch: &str,
    was_dirty: bool,
    discard_local_changes: bool,
    rollback: &RollbackTarget,
) -> Result<()> {
    if let Err(hook_err) = run_hooks(
        config,
        WorktreeRole::Review,
        HookEvent::Checkout,
        path,
        branch,
    ) {
        return Err(rollback_review_checkout(
            path,
            was_dirty,
            discard_local_changes,
            rollback,
            hook_err,
        ));
    }
    Ok(())
}

fn rollback_review_checkout(
    path: &Path,
    was_dirty: bool,
    discard_local_changes: bool,
    rollback: &RollbackTarget,
    hook_err: anyhow::Error,
) -> anyhow::Error {
    let force_rollback = discard_local_changes || !was_dirty;
    let rollback_result = match rollback {
        RollbackTarget::Branch(branch) => checkout_worktree_branch(path, branch, force_rollback),
        RollbackTarget::Detached(oid) => checkout_worktree_detached(path, oid, force_rollback),
    };

    match rollback_result {
        Ok(()) => hook_err,
        Err(rollback_err) => anyhow!(
            "{hook_err}\nAdditionally failed to restore worktree '{}': {rollback_err}",
            path.display()
        ),
    }
}
