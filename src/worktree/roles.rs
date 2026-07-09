use crate::worktree::WorktreeRole;
use crate::worktree::cleanup::{CleanupCandidate, find_cleanup_candidates};
use crate::worktree::config::{WorktreeConfig, load_worktree_config};
use crate::worktree::git::{
    LiveWorktree, add_worktree, checkout_worktree_branch, checkout_worktree_detached,
    create_local_branch_from_start_point_strict, current_branch, current_head_oid,
    delete_local_branch_if_tip_matches, ensure_local_branch_exists,
    ensure_local_branch_exists_from_start_point, force_delete_local_branch, is_worktree_dirty,
    list_live_worktrees, live_worktree_map, remove_worktree, repo_root,
};
use crate::worktree::hooks::{HookEvent, run_global_hooks, run_hooks};
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
    /// Display label for the removed worktree's role: `main`/`review`/`temp`, or
    /// `plain` for a worktree that matches no configured role location.
    pub role: String,
    pub branch: String,
    pub path: PathBuf,
    /// Whether the local branch was also deleted as part of the operation.
    pub branch_deleted: bool,
    /// The tip commit of the branch at the time it was deleted (for recovery).
    pub deleted_branch_tip: Option<String>,
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

    add_worktree(repo, &path, &branch, true)?;
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

    add_worktree(repo, &path, &branch, true)?;
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

    add_worktree(repo, &path, &branch, true)?;
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
        add_worktree(repo, &path, branch, true)?;
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

/// Create (or reuse) a durable, plain worktree for a branch — the branch-first
/// `kin wt add`. Unlike the role worktrees this has no policy: it defaults to a
/// sibling directory when the repo has a parent directory, otherwise
/// `<repo>/worktrees` (or an explicit `path`), runs only the global
/// create-hooks, is never force-added, and is never auto-cleaned. If a worktree
/// is already checked out on the branch, its path is returned unchanged
/// (idempotent).
pub fn ensure_added(
    repo: &Repository,
    new_branch: Option<&str>,
    target: Option<&str>,
    path_override: Option<&Path>,
) -> Result<EnsureResult> {
    let ctx = load_context(repo)?;
    let resolved_path_override = match path_override {
        Some(path) => {
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                repo_root(repo)?.join(path)
            };
            Some(normalize_path(path))
        }
        None => None,
    };

    let branch = match new_branch {
        Some(new) => new.to_string(),
        None => resolve_requested_branch(repo, target)?,
    };

    // Idempotent reuse: a branch can only be checked out in one worktree, so if
    // one already exists for it, hand that back rather than failing.
    if new_branch.is_none()
        && let Some(existing) = ctx
            .live_worktrees
            .iter()
            .find(|worktree| worktree.branch.as_deref() == Some(branch.as_str()))
    {
        if let Some(path_override) = resolved_path_override.as_ref()
            && path_override != &normalize_path(&existing.path)
        {
            eprintln!(
                "Warning: ignoring --path '{}' because branch '{}' is already checked out at '{}'.",
                path_override.display(),
                branch,
                existing.path.display()
            );
        }
        return Ok(EnsureResult {
            path: existing.path.clone(),
            created: false,
            switched: false,
        });
    }

    let path = match resolved_path_override {
        Some(path) => path,
        None => expand_path_template(&ctx.config.add_path_template, &branch)?,
    };

    if let Some(other) = ctx.live_by_path().get(&path) {
        return Err(anyhow!(
            "Worktree path '{}' is already in use by branch '{}'.",
            path.display(),
            other
                .branch
                .clone()
                .unwrap_or_else(|| "<detached>".to_string())
        ));
    }
    if path.exists() {
        return Err(anyhow!(
            "Path '{}' already exists but is not a git worktree.",
            path.display()
        ));
    }

    match new_branch {
        Some(new) => {
            // Create the branch, then the worktree + hooks, rolling both back on
            // failure — same guarantee as `kin wt temp -b`.
            ensure_local_branch_is_new(repo, new)?;
            let start_point = resolve_requested_start_point(repo, target)?;
            let branch_tip = match create_local_branch_from_start_point_strict(
                repo,
                new,
                &start_point,
            )? {
                true => local_branch_tip(repo, new)?,
                false => {
                    return Err(anyhow!(
                        "A local branch named '{}' appeared while creating the worktree. Resolve the race and retry.",
                        new
                    ));
                }
            };
            let mut worktree_created = false;
            if let Err(err) = (|| -> Result<()> {
                add_worktree(repo, &path, new, false)?;
                worktree_created = true;
                run_global_hooks(&ctx.config, "add", HookEvent::Create, &path, new)?;
                Ok(())
            })() {
                return Err(rollback_created_temp_branch(
                    repo,
                    &path,
                    new,
                    branch_tip,
                    worktree_created,
                    err,
                ));
            }
        }
        None => {
            ensure_local_branch_exists(repo, &branch)?;
            add_worktree(repo, &path, &branch, false)?;
            if let Err(hook_err) =
                run_global_hooks(&ctx.config, "add", HookEvent::Create, &path, &branch)
            {
                return Err(rollback_created_worktree(repo, &path, hook_err));
            }
        }
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

    // List every worktree git knows about, classifying each by its path. Worktrees
    // that match a configured role location are labeled with it; everything else
    // (added siblings, hand-made worktrees, the primary tree) is a plain `-` entry
    // that Kindra still lists, switches to, and removes — it just applies no role
    // policy. Nothing is stored, so there is no `stale-meta` drift state; `missing`
    // means git lists a worktree whose directory is gone.
    let mut rows = Vec::new();
    for live in &ctx.live_worktrees {
        let normalized = live.normalized_path();
        let role = role_for_path(&ctx.config, &normalized)?;

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
        if role == Some(WorktreeRole::Temp)
            && let Some(branch) = &live.branch
            && merged_branches.contains(branch)
        {
            state.push("merged".to_string());
        }

        rows.push(WorktreeListRow {
            role: role
                .map(|role| role.to_string())
                .unwrap_or_else(|| "-".to_string()),
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

pub fn remove_target(
    repo: &Repository,
    target: &str,
    force: bool,
    keep_branch: bool,
    with_branch: bool,
) -> Result<RemoveResult> {
    let ctx = load_context(repo)?;
    let resolved = resolve_target(&ctx, target)?;
    let role_label = role_label_for_path(&ctx.config, &resolved.path)?;
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
            role_label,
            resolved.branch
        ));
    }

    let is_trunk_branch = resolved.branch == ctx.config.trunk;
    let worktree_role = role_for_path(&ctx.config, &normalize_path(&resolved.path))?;
    let is_persistent_role = is_persistent_worktree_role(worktree_role);
    let auto_delete_allowed =
        ctx.config.temp.delete_merged && !is_trunk_branch && !is_persistent_role;

    let will_delete_branch = if keep_branch {
        false
    } else if with_branch {
        if is_trunk_branch {
            return Err(anyhow!(
                "Refusing to delete the trunk branch '{}'.",
                resolved.branch
            ));
        }
        true
    } else if auto_delete_allowed {
        // Only compute merged set when we actually need it for the default auto-delete path.
        let merged_branches: HashSet<String> = crate::stack::collect_merged_local_branches(
            repo,
            &ctx.config.trunk,
            &[&ctx.config.trunk],
        )?
        .into_iter()
        .collect();
        merged_branches.contains(&resolved.branch)
    } else {
        false
    };

    // If we plan to delete the branch, ensure it isn't checked out in another live worktree.
    // For explicit --with-branch, hard error (user wants the branch gone).
    // For default auto-delete, skip branch deletion but still remove the worktree (consistent with cleanup).
    let explicit_branch_delete = with_branch;
    let mut attempt_branch_delete = will_delete_branch;
    if will_delete_branch {
        match resolve_cross_worktree_branch_delete(
            &ctx.live_worktrees,
            &resolved.branch,
            &resolved.path,
            explicit_branch_delete,
        ) {
            CrossWorktreeBranchDeleteAction::Proceed => {}
            CrossWorktreeBranchDeleteAction::Refuse { other_path } => {
                return Err(anyhow!(
                    "Branch '{}' is checked out in another worktree at '{}'. Remove or switch that worktree before deleting the branch.",
                    resolved.branch,
                    other_path.display()
                ));
            }
            CrossWorktreeBranchDeleteAction::Skip { other_path } => {
                println!(
                    "Skipping branch delete for '{}' (checked out in another worktree at '{}').",
                    resolved.branch,
                    other_path.display()
                );
                attempt_branch_delete = false;
            }
        }
    }

    let branch_tip = if attempt_branch_delete {
        capture_branch_tip_for_deletion(repo, &resolved.branch)
    } else {
        None
    };

    // Build confirmation message that includes branch deletion when applicable.
    let worktree_desc = format!(
        "{} worktree '{}' at '{}'",
        role_label,
        resolved.branch,
        resolved.path.display()
    );
    let message = if branch_tip.is_some() {
        if dirty {
            format!(
                "{} has uncommitted changes. Remove the worktree and delete branch '{}' anyway?",
                worktree_desc, resolved.branch
            )
        } else {
            format!(
                "Remove {} and delete branch '{}'?",
                worktree_desc, resolved.branch
            )
        }
    } else if dirty {
        format!(
            "{} has uncommitted changes. Remove it anyway?",
            worktree_desc
        )
    } else {
        format!("Remove {}?", worktree_desc)
    };
    confirm_or_abort(&message)?;

    remove_resolved_target(repo, &ctx.config, &resolved, force)?;

    let (branch_deleted, deleted_branch_tip) = branch_tip
        .map(|tip| delete_branch_after_removal(repo, &resolved.branch, tip, force))
        .unwrap_or((false, None));

    Ok(RemoveResult {
        role: role_label,
        branch: resolved.branch,
        path: resolved.path,
        branch_deleted,
        deleted_branch_tip,
    })
}

/// Display label for whichever role a path maps to, or `plain` when it matches no
/// configured role location.
fn role_label_for_path(config: &WorktreeConfig, path: &Path) -> Result<String> {
    Ok(role_for_path(config, &normalize_path(path))?
        .map(|role| role.to_string())
        .unwrap_or_else(|| "plain".to_string()))
}

fn is_persistent_worktree_role(role: Option<WorktreeRole>) -> bool {
    matches!(role, Some(WorktreeRole::Main | WorktreeRole::Review))
}

#[derive(Debug)]
enum CrossWorktreeBranchDeleteAction {
    Proceed,
    Skip { other_path: PathBuf },
    Refuse { other_path: PathBuf },
}

fn other_worktree_with_branch<'a>(
    live_worktrees: &'a [LiveWorktree],
    branch: &str,
    excluding_path: &Path,
) -> Option<&'a LiveWorktree> {
    let excluded = normalize_path(excluding_path);
    live_worktrees.iter().find(|worktree| {
        worktree.branch.as_deref() == Some(branch) && worktree.normalized_path() != excluded
    })
}

fn resolve_cross_worktree_branch_delete(
    live_worktrees: &[LiveWorktree],
    branch: &str,
    path: &Path,
    explicit_branch_delete: bool,
) -> CrossWorktreeBranchDeleteAction {
    let Some(other) = other_worktree_with_branch(live_worktrees, branch, path) else {
        return CrossWorktreeBranchDeleteAction::Proceed;
    };
    if explicit_branch_delete {
        CrossWorktreeBranchDeleteAction::Refuse {
            other_path: other.path.clone(),
        }
    } else {
        CrossWorktreeBranchDeleteAction::Skip {
            other_path: other.path.clone(),
        }
    }
}

fn count_cross_worktree_branch_conflicts(
    live_worktrees: &[LiveWorktree],
    candidates: &[(CleanupCandidate, bool)],
) -> usize {
    candidates
        .iter()
        .filter(|(candidate, _)| {
            other_worktree_with_branch(live_worktrees, &candidate.branch, &candidate.path).is_some()
        })
        .count()
}

fn capture_branch_tip_for_deletion(repo: &Repository, branch: &str) -> Option<Oid> {
    match local_branch_tip(repo, branch) {
        Ok(tip) => Some(tip),
        Err(err) => {
            eprintln!(
                "Warning: failed to capture tip for branch '{}': {}. Branch will not be deleted.",
                branch, err
            );
            None
        }
    }
}

/// Delete a branch after its worktree has been removed, using a tip captured
/// beforehand. Warns and returns `(false, None)` on failure so callers can still
/// report successful worktree removal.
fn delete_branch_after_removal(
    repo: &Repository,
    branch: &str,
    tip: Oid,
    force: bool,
) -> (bool, Option<String>) {
    match delete_local_branch_if_tip_matches(repo, branch, tip) {
        Ok(true) => (true, Some(tip.to_string())),
        Ok(false) => {
            if force {
                match force_delete_local_branch(repo, branch) {
                    Ok(()) => (true, Some(tip.to_string())),
                    Err(err) => {
                        eprintln!("Warning: {}", err);
                        (false, None)
                    }
                }
            } else {
                eprintln!(
                    "Note: did not delete branch '{}' (tip no longer matches the captured value; use --force to override).",
                    branch
                );
                (false, None)
            }
        }
        Err(e) => {
            eprintln!("Warning: failed to delete branch '{}': {}", branch, e);
            (false, None)
        }
    }
}

pub fn cleanup_temp_worktrees(
    repo: &Repository,
    force: bool,
    keep_branch: bool,
) -> Result<CleanupSummary> {
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

    let will_delete_branches = !keep_branch;
    let cross_worktree_branch_skip_count = if will_delete_branches {
        count_cross_worktree_branch_conflicts(&ctx.live_worktrees, &candidates_with_dirty)
    } else {
        0
    };
    let base = if will_delete_branches {
        if cross_worktree_branch_skip_count > 0 {
            format!(
                "Remove {} temp worktree candidate(s) and delete branches where possible ({} checked out elsewhere)",
                candidates_with_dirty.len(),
                cross_worktree_branch_skip_count
            )
        } else {
            format!(
                "Remove {} temp worktree candidate(s) and delete their branches",
                candidates_with_dirty.len()
            )
        }
    } else {
        format!(
            "Remove {} temp worktree candidate(s)",
            candidates_with_dirty.len()
        )
    };
    let confirmation = if dirty_count == 0 {
        format!("{}?", base)
    } else if force {
        format!(
            "{}? {} dirty candidate(s) will be removed.",
            base, dirty_count
        )
    } else {
        format!(
            "{}? {} dirty candidate(s) will be skipped without --force.",
            base, dirty_count
        )
    };
    confirm_or_abort(&confirmation)?;

    let mut removed = Vec::new();
    let mut skipped = 0usize;
    for (candidate, dirty) in candidates_with_dirty {
        let resolved = ResolvedTarget {
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

        let mut do_branch_delete = will_delete_branches;
        if do_branch_delete {
            match resolve_cross_worktree_branch_delete(
                &ctx.live_worktrees,
                &resolved.branch,
                &resolved.path,
                false,
            ) {
                CrossWorktreeBranchDeleteAction::Proceed => {}
                CrossWorktreeBranchDeleteAction::Refuse { .. } => {
                    unreachable!(
                        "cleanup never requests explicit branch deletion via --with-branch"
                    )
                }
                CrossWorktreeBranchDeleteAction::Skip { other_path } => {
                    println!(
                        "Skipping branch delete for '{}' (checked out in another worktree at '{}').",
                        resolved.branch,
                        other_path.display()
                    );
                    do_branch_delete = false;
                }
            }
        }

        // Capture tip before removing the worktree (all cleanup candidates are merged).
        let branch_tip = if do_branch_delete {
            capture_branch_tip_for_deletion(repo, &resolved.branch)
        } else {
            None
        };

        remove_resolved_target(repo, &ctx.config, &resolved, force)?;

        let (branch_deleted, deleted_branch_tip) = branch_tip
            .map(|tip| delete_branch_after_removal(repo, &resolved.branch, tip, force))
            .unwrap_or((false, None));

        removed.push(RemoveResult {
            // Cleanup only ever targets temp worktrees.
            role: WorktreeRole::Temp.to_string(),
            branch: resolved.branch,
            path: resolved.path,
            branch_deleted,
            deleted_branch_tip,
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
    // always a tree to remove; git's own worktree list is the record. Run role
    // remove-hooks when the path maps to a role, else just the global ones — a
    // plain worktree carries no role policy.
    match role_for_path(config, &normalize_path(&resolved.path))? {
        Some(role) => run_hooks(
            config,
            role,
            HookEvent::Remove,
            &resolved.path,
            &resolved.branch,
        )?,
        None => run_global_hooks(
            config,
            "-",
            HookEvent::Remove,
            &resolved.path,
            &resolved.branch,
        )?,
    }
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
                branch: live.branch.clone().unwrap_or_else(|| "review".to_string()),
                path,
                live: Some(live),
            })
        }
        WorktreeTarget::Role(WorktreeRole::Temp) => unreachable!(
            "parse_target only yields Role(Main), Role(Review), or Branch — there is no bare `temp` keyword"
        ),
        WorktreeTarget::Branch(branch) => {
            // Prefer the managed temp slot for this branch (a deterministic path):
            // temp force-adds, so a branch can be live in both its temp worktree
            // and the primary, and the temp is the one `path`/`remove` should mean.
            // Otherwise fall back to whichever live worktree is on the branch — an
            // added sibling, any plain worktree, or a temp worktree left at its old
            // path after a `git branch -m` rename.
            let live = if ctx.config.temp.enabled {
                let temp_path = expand_path_template(&ctx.config.temp.path_template, &branch)?;
                live_by_path.get(&normalize_path(&temp_path)).cloned()
            } else {
                None
            }
            .or_else(|| {
                ctx.live_worktrees
                    .iter()
                    .find(|worktree| worktree.branch.as_deref() == Some(branch.as_str()))
                    .cloned()
            });
            let Some(live) = live else {
                return Err(anyhow!("No worktree found for branch '{}'.", branch));
            };
            Ok(ResolvedTarget {
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

#[cfg(test)]
mod branch_delete_tests {
    use super::*;
    use git2::BranchType;
    use std::path::Path;
    use tempfile::TempDir;

    fn live(path: &str, branch: Option<&str>) -> LiveWorktree {
        LiveWorktree {
            path: PathBuf::from(path),
            branch: branch.map(str::to_string),
            detached: branch.is_none(),
        }
    }

    #[test]
    fn resolve_cross_worktree_refuses_explicit_delete() {
        let path1 = PathBuf::from("/a");
        let path2 = PathBuf::from("/b");
        let worktrees = vec![live("/a", Some("feat")), live("/b", Some("feat"))];
        match resolve_cross_worktree_branch_delete(&worktrees, "feat", &path1, true) {
            CrossWorktreeBranchDeleteAction::Refuse { other_path } => {
                assert_eq!(other_path, path2);
            }
            other => panic!("expected refuse, got {other:?}"),
        }
    }

    #[test]
    fn resolve_cross_worktree_skips_auto_delete() {
        let path1 = PathBuf::from("/a");
        let path2 = PathBuf::from("/b");
        let worktrees = vec![live("/a", Some("feat")), live("/b", Some("feat"))];
        match resolve_cross_worktree_branch_delete(&worktrees, "feat", &path1, false) {
            CrossWorktreeBranchDeleteAction::Skip { other_path } => {
                assert_eq!(other_path, path2);
            }
            other => panic!("expected skip, got {other:?}"),
        }
    }

    #[test]
    fn count_cross_worktree_branch_conflicts_detects_blocked_candidates() {
        let worktrees = vec![
            live("/temp/feat", Some("feat")),
            live("/review", Some("feat")),
        ];
        let candidates = vec![(
            CleanupCandidate {
                branch: "feat".to_string(),
                path: PathBuf::from("/temp/feat"),
                live: worktrees[0].clone(),
            },
            false,
        )];
        assert_eq!(
            count_cross_worktree_branch_conflicts(&worktrees, &candidates),
            1
        );
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo(dir: &Path) -> git2::Repository {
        git(dir, &["init", "--initial-branch=main"]);
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        std::fs::write(dir.join("base.txt"), "base").unwrap();
        git(dir, &["add", "base.txt"]);
        git(dir, &["commit", "-m", "base"]);
        git2::Repository::open(dir).unwrap()
    }

    #[test]
    fn capture_branch_tip_for_deletion_returns_none_when_branch_missing() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo(dir.path());
        assert!(capture_branch_tip_for_deletion(&repo, "missing-branch").is_none());
    }

    #[test]
    fn delete_branch_after_removal_deletes_when_tip_matches() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo(dir.path());
        git(dir.path(), &["checkout", "-b", "feature-a"]);
        std::fs::write(dir.path().join("feature.txt"), "feature").unwrap();
        git(dir.path(), &["add", "feature.txt"]);
        git(dir.path(), &["commit", "-m", "feature"]);
        let tip = local_branch_tip(&repo, "feature-a").unwrap();

        let (deleted, stored_tip) = delete_branch_after_removal(&repo, "feature-a", tip, false);
        assert!(deleted);
        assert_eq!(stored_tip, Some(tip.to_string()));
        assert!(repo.find_branch("feature-a", BranchType::Local).is_err());
    }

    #[test]
    fn delete_branch_after_removal_skips_mismatch_without_force() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo(dir.path());
        git(dir.path(), &["checkout", "-b", "feature-a"]);
        std::fs::write(dir.path().join("feature.txt"), "feature").unwrap();
        git(dir.path(), &["add", "feature.txt"]);
        git(dir.path(), &["commit", "-m", "feature"]);
        let stale_tip = local_branch_tip(&repo, "feature-a").unwrap();
        std::fs::write(dir.path().join("feature-2.txt"), "feature-2").unwrap();
        git(dir.path(), &["add", "feature-2.txt"]);
        git(dir.path(), &["commit", "-m", "feature-2"]);

        let (deleted, stored_tip) =
            delete_branch_after_removal(&repo, "feature-a", stale_tip, false);
        assert!(!deleted);
        assert_eq!(stored_tip, None);
        assert!(repo.find_branch("feature-a", BranchType::Local).is_ok());
    }

    #[test]
    fn delete_branch_after_removal_force_deletes_on_tip_mismatch() {
        let dir = TempDir::new().unwrap();
        let repo = init_repo(dir.path());
        git(dir.path(), &["checkout", "-b", "feature-a"]);
        std::fs::write(dir.path().join("feature.txt"), "feature").unwrap();
        git(dir.path(), &["add", "feature.txt"]);
        git(dir.path(), &["commit", "-m", "feature"]);
        let stale_tip = local_branch_tip(&repo, "feature-a").unwrap();
        std::fs::write(dir.path().join("feature-2.txt"), "feature-2").unwrap();
        git(dir.path(), &["add", "feature-2.txt"]);
        git(dir.path(), &["commit", "-m", "feature-2"]);
        git(dir.path(), &["checkout", "main"]);

        let (deleted, stored_tip) =
            delete_branch_after_removal(&repo, "feature-a", stale_tip, true);
        assert!(deleted);
        assert_eq!(stored_tip, Some(stale_tip.to_string()));
        assert!(repo.find_branch("feature-a", BranchType::Local).is_err());
    }
}
