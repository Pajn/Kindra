use crate::worktree::WorktreeRole;
use crate::worktree::cleanup::find_cleanup_candidates;
use crate::worktree::config::{WorktreeConfig, load_worktree_config};
use crate::worktree::git::{
    LiveWorktree, add_worktree, checkout_worktree_branch, checkout_worktree_detached,
    create_local_branch_from_start_point_strict, current_branch, current_head_oid,
    delete_local_branch_if_tip_matches, ensure_local_branch_exists,
    ensure_local_branch_exists_from_start_point, is_worktree_dirty, list_live_worktrees,
    live_worktree_map, remove_worktree,
};
use crate::worktree::hooks::{HookEvent, run_hooks};
use crate::worktree::path_resolver::{
    WorktreeTarget, expand_path_template, normalize_path, parse_target, temp_template_root,
};
use crate::worktree::ui::{WorktreeListRow, confirm_or_abort};
use anyhow::{Result, anyhow};
use git2::{BranchType, ErrorCode, Oid, Repository};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Classify a worktree by its path, using the configured layout as the single
/// source of truth: the managed set and each worktree's role are *derived* from
/// git's own worktree list plus config, never stored. Returns `None` for a
/// worktree that isn't Kindra-managed (its path is outside every configured
/// location).
pub(crate) fn role_for_path(
    config: &WorktreeConfig,
    normalized: &Path,
) -> Result<Option<WorktreeRole>> {
    if normalized == normalize_path(&config.main.path).as_path() {
        return Ok(Some(WorktreeRole::Main));
    }
    if normalized == normalize_path(&config.review.path).as_path() {
        return Ok(Some(WorktreeRole::Review));
    }
    // Only consult the temp template when temp worktrees are enabled: a disabled
    // `[worktrees.temp]` may carry an invalid `path_template` that
    // `temp_template_root` would reject, and that must not break main/review
    // classification.
    if config.temp.enabled {
        let temp_root = temp_template_root(&config.temp.path_template)?;
        if normalized.starts_with(&temp_root) {
            return Ok(Some(WorktreeRole::Temp));
        }
    }
    Ok(None)
}

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
}

#[derive(Clone, Debug, Default)]
pub struct CleanupSummary {
    pub candidates: usize,
    pub removed: Vec<RemoveResult>,
    pub skipped: usize,
}

pub fn ensure_main(repo: &Repository) -> Result<EnsureResult> {
    let ctx = load_context(repo)?;
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
    let ctx = load_context(repo)?;
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
            return Ok(EnsureResult {
                path,
                created: false,
                switched: false,
            });
        }

        let dirty = is_worktree_dirty(&path)?;
        let discard_local_changes = force || (dirty && ctx.config.review.clean_before_switch);
        if dirty && !force && ctx.config.review.clean_before_switch {
            confirm_or_abort(&format!(
                "Review worktree '{}' has uncommitted changes. Discard them and switch to '{}'?",
                path.display(),
                branch
            ))?;
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

    Ok(EnsureResult {
        path,
        created: true,
        switched: false,
    })
}

pub fn ensure_temp(repo: &Repository, requested_branch: Option<&str>) -> Result<EnsureResult> {
    let ctx = load_context(repo)?;
    if !ctx.config.temp.enabled {
        return Err(anyhow!("Temp worktrees are disabled in .git/kindra.toml."));
    }

    let branch = resolve_requested_branch(repo, requested_branch)?;
    ensure_local_branch_exists(repo, &branch)?;

    // A managed temp worktree may already exist for this branch at a differently
    // named path — e.g. it was created as `old` and then `git branch -m`'d to
    // `new`, leaving the directory at the old sanitized path. Reuse it (matched by
    // branch, restricted to worktrees that classify as temp) instead of
    // force-creating a duplicate. Classifying by role (not just the temp root)
    // avoids reusing the primary or a main/review worktree that merely has the
    // branch checked out.
    if let Some(existing) = ctx.live_worktrees.iter().find(|worktree| {
        worktree.branch.as_deref() == Some(branch.as_str())
            && role_for_path(&ctx.config, &worktree.normalized_path())
                .ok()
                .flatten()
                == Some(WorktreeRole::Temp)
    }) {
        return Ok(EnsureResult {
            path: existing.path.clone(),
            created: false,
            switched: false,
        });
    }

    // The temp path is a deterministic function of the branch, so it is recomputed
    // rather than remembered.
    let path = expand_path_template(&ctx.config.temp.path_template, &branch)?;

    ensure_temp_path_available(&ctx.config, &ctx.live_worktrees, &branch, &path)?;
    let live = ctx.live_by_path().get(&normalize_path(&path)).cloned();

    if let Some(live) = live {
        if live.branch.as_deref() != Some(branch.as_str()) {
            return Err(anyhow!(
                "Managed temp worktree path '{}' is already associated with branch '{}'.",
                path.display(),
                live.branch.unwrap_or_else(|| "<detached>".to_string())
            ));
        }

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
    let ctx = load_context(repo)?;
    if !ctx.config.temp.enabled {
        return Err(anyhow!("Temp worktrees are disabled in .git/kindra.toml."));
    }

    ensure_local_branch_is_new(repo, branch)?;
    let start_point = resolve_requested_start_point(repo, requested_start_point)?;
    let path = expand_path_template(&ctx.config.temp.path_template, branch)?;

    ensure_temp_path_available(&ctx.config, &ctx.live_worktrees, branch, &path)?;

    if path.exists() {
        return Err(anyhow!(
            "Configured temp worktree path '{}' exists but is not a valid git worktree.",
            path.display()
        ));
    }

    let branch_tip_after_create = match create_local_branch_from_start_point_strict(
        repo,
        branch,
        &start_point,
    )? {
        true => local_branch_tip(repo, branch)?,
        false => {
            return Err(anyhow!(
                "A local branch named '{}' appeared while creating the temp worktree. Resolve the race and retry.",
                branch
            ));
        }
    };
    let mut worktree_created = false;
    if let Err(err) = (|| -> Result<()> {
        add_worktree(repo, &path, branch)?;
        worktree_created = true;
        run_hooks(
            &ctx.config,
            WorktreeRole::Temp,
            HookEvent::Create,
            &path,
            branch,
        )?;
        Ok(())
    })() {
        return Err(rollback_created_temp_branch(
            repo,
            &path,
            branch,
            branch_tip_after_create,
            worktree_created,
            err,
        ));
    }

    Ok(EnsureResult {
        path,
        created: true,
        switched: false,
    })
}

pub fn resolve_existing_path(repo: &Repository, target: &str) -> Result<PathBuf> {
    let ctx = load_context(repo)?;
    let resolved = resolve_target(&ctx, target)?;
    Ok(resolved.path)
}

pub fn list_managed_worktrees(repo: &Repository) -> Result<Vec<WorktreeListRow>> {
    let ctx = load_context(repo)?;
    let current_path =
        normalize_path(repo.workdir().ok_or_else(|| {
            anyhow!("Kindra worktree management requires a non-bare repository.")
        })?);
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

    // Derive the managed set straight from git's worktree list, classifying each
    // by its path. There is no separate record to fall out of sync, so the old
    // `stale-meta` / `missing` states (both drift artifacts) no longer exist —
    // `missing` here means git lists a worktree whose directory is gone.
    let mut rows = Vec::new();
    for live in &ctx.live_worktrees {
        let normalized = live.normalized_path();
        let Some(role) = role_for_path(&ctx.config, &normalized)? else {
            continue;
        };

        let mut state = Vec::new();
        if !live.path.exists() {
            state.push("missing".to_string());
        } else {
            if is_worktree_dirty(&live.path)? {
                state.push("dirty".to_string());
            }
            if normalized == current_path {
                state.push("current".to_string());
            }
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

    rows.sort_by(|left, right| {
        left.role
            .cmp(&right.role)
            .then_with(|| left.branch.cmp(&right.branch))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(rows)
}

pub fn remove_target(repo: &Repository, target: &str, force: bool) -> Result<RemoveResult> {
    let ctx = load_context(repo)?;
    let resolved = resolve_target(&ctx, target)?;
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
    confirm_or_abort(&message)?;

    remove_resolved_target(repo, &ctx.config, &resolved, force)?;

    Ok(RemoveResult {
        role: resolved.role,
        branch: resolved.branch,
        path: resolved.path,
    })
}

pub fn cleanup_temp_worktrees(repo: &Repository, force: bool) -> Result<CleanupSummary> {
    let ctx = load_context(repo)?;
    let candidates = find_cleanup_candidates(repo, &ctx.config, &ctx.live_worktrees)?;
    if candidates.is_empty() {
        return Ok(CleanupSummary::default());
    }
    let candidates_with_dirty = candidates
        .into_iter()
        .map(|candidate| {
            let dirty = is_worktree_dirty(&candidate.path)?;
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
            candidate.branch,
            "merged",
            candidate.path.display(),
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
    confirm_or_abort(&confirmation)?;

    let mut removed = Vec::new();
    let mut skipped = 0usize;
    for (candidate, dirty) in candidates_with_dirty {
        let resolved = ResolvedTarget {
            role: WorktreeRole::Temp,
            branch: candidate.branch.clone(),
            path: candidate.path.clone(),
            live: Some(candidate.live.clone()),
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
        remove_resolved_target(repo, &ctx.config, &resolved, force)?;
        removed.push(RemoveResult {
            role: resolved.role,
            branch: resolved.branch,
            path: resolved.path,
        });
    }

    Ok(CleanupSummary {
        candidates: removed.len() + skipped,
        removed,
        skipped,
    })
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

fn local_branch_tip(repo: &Repository, branch: &str) -> Result<Oid> {
    repo.find_branch(branch, BranchType::Local)?
        .get()
        .target()
        .ok_or_else(|| anyhow!("Local branch '{}' has no target commit.", branch))
}

fn ensure_temp_path_available(
    config: &WorktreeConfig,
    live_worktrees: &[LiveWorktree],
    branch: &str,
    path: &Path,
) -> Result<()> {
    let normalized = normalize_path(path);
    let live_by_path = live_worktree_map(live_worktrees);
    let Some(other_live) = live_by_path.get(&normalized) else {
        return Ok(());
    };

    // A live worktree already occupies this path. It is only acceptable when the
    // path resolves to the temp slot for *this* branch (the reuse case). Branch
    // names can collide once sanitized into a path (`feature/x` and `feature-x`),
    // and a misconfigured temp template can overlap the main/review path — both
    // are genuine collisions, reported by the role the path actually belongs to.
    let role = role_for_path(config, &normalized)?;
    if role == Some(WorktreeRole::Temp) && other_live.branch.as_deref() == Some(branch) {
        return Ok(());
    }

    let occupant = other_live
        .branch
        .clone()
        .unwrap_or_else(|| "<detached>".to_string());
    match role {
        Some(role) => Err(anyhow!(
            "Temp worktree path '{}' is already reserved for {} '{}'.",
            path.display(),
            role,
            occupant
        )),
        None => Err(anyhow!(
            "Temp worktree path '{}' is already in use by branch '{}'.",
            path.display(),
            occupant
        )),
    }
}

fn remove_resolved_target(
    repo: &Repository,
    config: &WorktreeConfig,
    resolved: &ResolvedTarget,
    force: bool,
) -> Result<()> {
    // `resolve_target` only yields targets backed by a live worktree, so there is
    // always a tree to remove; git's own worktree list is the record.
    run_hooks(
        config,
        resolved.role,
        HookEvent::Remove,
        &resolved.path,
        &resolved.branch,
    )?;
    remove_worktree(repo, &resolved.path, force)?;
    Ok(())
}

fn resolve_target(ctx: &LoadedContext, target: &str) -> Result<ResolvedTarget> {
    let live_by_path = ctx.live_by_path();
    // Each target maps to a deterministic path from config; a target exists only
    // if git has a live worktree there.
    match parse_target(target) {
        WorktreeTarget::Role(WorktreeRole::Main) => {
            let path = ctx.config.main.path.clone();
            let live = live_by_path.get(&normalize_path(&path)).cloned();
            let Some(live) = live else {
                return Err(anyhow!("No managed main worktree exists."));
            };
            Ok(ResolvedTarget {
                role: WorktreeRole::Main,
                branch: live
                    .branch
                    .clone()
                    .unwrap_or_else(|| ctx.config.main.branch.clone()),
                path,
                live: Some(live),
            })
        }
        WorktreeTarget::Role(WorktreeRole::Review) => {
            let path = ctx.config.review.path.clone();
            let live = live_by_path.get(&normalize_path(&path)).cloned();
            let Some(live) = live else {
                return Err(anyhow!("No managed review worktree exists."));
            };
            Ok(ResolvedTarget {
                role: WorktreeRole::Review,
                branch: live.branch.clone().unwrap_or_else(|| "review".to_string()),
                path,
                live: Some(live),
            })
        }
        WorktreeTarget::Role(WorktreeRole::Temp) => unreachable!(
            "parse_target only yields Role(Main), Role(Review), or TempBranch — there is no bare `temp` keyword"
        ),
        WorktreeTarget::TempBranch(branch) => {
            let path = expand_path_template(&ctx.config.temp.path_template, &branch)?;
            let live = live_by_path
                .get(&normalize_path(&path))
                .cloned()
                .or_else(|| {
                    // After `git branch -m` the worktree keeps its old (differently
                    // named) temp path, so the recomputed path won't find it. Fall
                    // back to matching by branch among worktrees that classify as temp.
                    ctx.live_worktrees
                        .iter()
                        .find(|worktree| {
                            worktree.branch.as_deref() == Some(branch.as_str())
                                && role_for_path(&ctx.config, &worktree.normalized_path())
                                    .ok()
                                    .flatten()
                                    == Some(WorktreeRole::Temp)
                        })
                        .cloned()
                });
            let Some(live) = live else {
                return Err(anyhow!(
                    "No managed temp worktree exists for branch '{}'.",
                    branch
                ));
            };
            Ok(ResolvedTarget {
                role: WorktreeRole::Temp,
                branch,
                path: live.path.clone(),
                live: Some(live),
            })
        }
    }
}

fn load_context(repo: &Repository) -> Result<LoadedContext> {
    Ok(LoadedContext {
        config: load_worktree_config(repo)?,
        live_worktrees: list_live_worktrees(repo)?,
    })
}

struct LoadedContext {
    config: WorktreeConfig,
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

fn rollback_created_temp_branch(
    repo: &Repository,
    path: &Path,
    branch: &str,
    branch_tip_after_create: Oid,
    worktree_created: bool,
    original_err: anyhow::Error,
) -> anyhow::Error {
    let mut rollback_errors = Vec::new();

    if (worktree_created || path.exists())
        && let Err(remove_err) = remove_worktree(repo, path, true)
    {
        rollback_errors.push(format!(
            "Failed to roll back worktree '{}': {}",
            path.display(),
            remove_err
        ));
    }

    match delete_local_branch_if_tip_matches(repo, branch, branch_tip_after_create) {
        Ok(true) => {}
        Ok(false) => match repo.find_branch(branch, BranchType::Local) {
            Ok(current_branch) => match current_branch.get().target() {
                Some(current_tip) => rollback_errors.push(format!(
                "Left branch '{}' in place for manual cleanup because its tip moved from {} to {}.",
                branch, branch_tip_after_create, current_tip
                )),
                None => rollback_errors.push(format!(
                "Left branch '{}' in place for manual cleanup because its current tip could not be resolved after it was created at {}.",
                branch, branch_tip_after_create
                )),
            },
            Err(err) if err.code() == ErrorCode::NotFound => {}
            Err(err) => rollback_errors.push(format!(
                "Failed to inspect branch '{}' during rollback: {}",
                branch, err
            )),
        },
        Err(delete_err) => rollback_errors.push(format!(
            "Failed to roll back branch '{}' at {}: {}",
            branch, branch_tip_after_create, delete_err
        )),
    }

    if rollback_errors.is_empty() {
        original_err
    } else {
        anyhow!(
            "{original_err}\nAdditionally:\n{}",
            rollback_errors.join("\n")
        )
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
