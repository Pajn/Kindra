use super::CheckoutSubcommand;
use super::find_upstream;
use crate::gh;
use crate::stack::{
    discover_pr_connected_stack, get_immediate_successors, get_stack_tips, visualize_stack,
};
use crate::worktree::git::repo_root;
use anyhow::{Context, Result, anyhow};
use git2::{BranchType, Repository};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;

pub fn checkout(
    subcommand: &Option<CheckoutSubcommand>,
    branch: &Option<String>,
    all: bool,
) -> Result<()> {
    let repo = crate::open_repo()?;

    if let Some(branch) = branch {
        let _lock = crate::state_io::RepoLock::acquire(&repo)?;
        ensure_checkout_available(&repo)?;
        return crate::overrides::with_planned(&repo, false, || {
            checkout_branch_with_pr_hydration(&repo, branch)
        });
    }

    if all && subcommand.is_none() {
        let mut branch_names = Vec::new();
        let local_branches = repo.branches(Some(BranchType::Local))?;
        for res in local_branches {
            let (branch, _) = res?;
            if let Some(name) = branch.name()? {
                branch_names.push(name.to_string());
            }
        }
        branch_names.sort();

        if branch_names.is_empty() {
            println!("No local branches found.");
            return Ok(());
        }

        let selected_name = crate::commands::prompt_select(
            "Select branch to checkout:",
            branch_names,
            crate::commands::Fallback::Require("Run 'git checkout <branch>' instead."),
        )?;
        return perform_git_checkout(&selected_name);
    }

    let upstream_name = find_upstream(&repo)?.ok_or_else(|| {
        anyhow!("Could not find a base branch (init.defaultBranch, main, master, or trunk)")
    })?;
    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let upstream_id = upstream_obj.id();
    let head = repo.head()?;
    let head_id = head.peel_to_commit()?.id();

    let current_branch_name = if !repo.head_detached()? {
        head.shorthand().map(|s| s.to_string())
    } else {
        None
    };

    match subcommand {
        Some(CheckoutSubcommand::Up) => {
            let merge_base = repo.merge_base(upstream_id, head_id)?;
            let branches = crate::stack::get_stack_branches_from_merge_base(
                &repo,
                merge_base,
                head_id,
                upstream_id,
                &upstream_name,
            )?;
            let mut successors = get_immediate_successors(&repo, head_id, &branches)?;
            successors.sort();

            match successors.len() {
                0 => Err(anyhow!("Already at the top of the stack")),
                1 => perform_git_checkout(&successors[0]),
                _ => {
                    let selected = crate::commands::prompt_select(
                        "Multiple branches ahead. Select one:",
                        successors,
                        crate::commands::Fallback::Require("Run 'git checkout <branch>' instead."),
                    )?;
                    perform_git_checkout(&selected)
                }
            }
        }
        Some(CheckoutSubcommand::Down) => {
            let current_name = current_branch_name.ok_or_else(|| anyhow!("Not on a branch"))?;
            let parent_branches =
                find_first_parent_branches_via_git_log(&repo, &upstream_name, &current_name)?;

            match parent_branches.len() {
                0 => perform_git_checkout(&upstream_name),
                1 => perform_git_checkout(&parent_branches[0]),
                _ => {
                    let selected = crate::commands::prompt_select(
                        "Multiple parent branches found. Select one:",
                        parent_branches,
                        crate::commands::Fallback::Require("Run 'git checkout <branch>' instead."),
                    )?;
                    perform_git_checkout(&selected)
                }
            }
        }
        Some(CheckoutSubcommand::Top) => {
            let merge_base = repo.merge_base(upstream_id, head_id)?;
            let branches = crate::stack::get_stack_branches_from_merge_base(
                &repo,
                merge_base,
                head_id,
                upstream_id,
                &upstream_name,
            )?;
            let mut tips = get_stack_tips(&repo, &branches)?;
            tips.sort();
            match tips.len() {
                0 => Err(anyhow!("No branches in stack")),
                1 => perform_git_checkout(&tips[0]),
                _ => {
                    let selected = crate::commands::prompt_select(
                        "Multiple stack tips found. Select one:",
                        tips,
                        crate::commands::Fallback::Require("Run 'git checkout <branch>' instead."),
                    )?;
                    perform_git_checkout(&selected)
                }
            }
        }
        None => {
            let merge_base = repo.merge_base(upstream_id, head_id)?;
            let all_branches = crate::stack::get_stack_branches_from_merge_base(
                &repo,
                merge_base,
                head_id,
                upstream_id,
                &upstream_name,
            )?;

            let visualized = visualize_stack(&repo, &all_branches, current_branch_name.as_deref())?;

            if visualized.is_empty() {
                println!(
                    "No branches found in the current stack (excluding {}). Use --all to see everything.",
                    upstream_name
                );
                return Ok(());
            }

            let options: Vec<String> = visualized.iter().map(|v| v.display_name.clone()).collect();
            let selected_display = crate::commands::prompt_select(
                "Select branch to checkout:",
                options,
                crate::commands::Fallback::Require("Run 'git checkout <branch>' instead."),
            )?;

            let selected_name = visualized
                .iter()
                .find(|v| v.display_name == selected_display)
                .map(|v| v.name.clone())
                .ok_or_else(|| anyhow!("Failed to find selected branch '{}'", selected_display))?;

            perform_git_checkout(&selected_name)
        }
    }
}

#[derive(Serialize, Deserialize)]
struct HydrationStep {
    branch: String,
    remote_ref: Option<String>,
    tip: String,
    completed: bool,
}

#[derive(Serialize, Deserialize)]
struct HydrationState {
    branch: String,
    repository: String,
    steps: Vec<HydrationStep>,
}

pub(crate) fn hydration_state_path(repo: &Repository) -> PathBuf {
    repo.path().join("kindra_checkout_state.json")
}

pub(crate) fn hydration_in_progress(repo: &Repository) -> bool {
    hydration_state_path(repo).exists()
}

fn save_hydration(repo: &Repository, state: &HydrationState) -> Result<()> {
    crate::state_io::write_atomic(
        &hydration_state_path(repo),
        &serde_json::to_string_pretty(state)?,
    )
}

fn ensure_checkout_available(repo: &Repository) -> Result<()> {
    if hydration_in_progress(repo)
        || crate::rebase_utils::state_path(repo).exists()
        || crate::commands::run::run_state_exists(repo)
    {
        return Err(anyhow!(
            "A Kindra operation is in progress. Use 'kin continue' or 'kin abort'."
        ));
    }
    crate::commands::sync::ensure_no_native_git_operation(repo)
}

fn checkout_branch_with_pr_hydration(repo: &Repository, branch: &str) -> Result<()> {
    fetch_all_remotes(repo)?;
    gh::check_gh()?;

    let upstream_name = find_upstream(repo)?;
    let (repository, prs) = gh::checkout_pr_snapshot()?;
    let (branch, creation_order) =
        discover_pr_connected_stack(repo, branch, upstream_name.as_deref(), &prs)?;
    let mut steps = Vec::new();
    for name in creation_order {
        let local = repo.find_branch(&name, BranchType::Local).ok();
        let remote_ref = if local.is_some() {
            None
        } else {
            Some(resolve_remote_tracking_ref(repo, &name, &repository)?.ok_or_else(|| anyhow!(
                "No remote-tracking branch found for discovered stack branch '{}' in PR repository '{}'.", name, repository
            ))?)
        };
        let tip = if let Some(local) = &local {
            local.get().peel_to_commit()?.id()
        } else {
            repo.find_branch(remote_ref.as_ref().unwrap(), BranchType::Remote)?
                .get()
                .peel_to_commit()?
                .id()
        };
        steps.push(HydrationStep {
            branch: name,
            remote_ref,
            tip: tip.to_string(),
            completed: local.is_some(),
        });
    }
    let state = HydrationState {
        branch,
        repository,
        steps,
    };
    // Persist every source OID before creating any refs. A restart never fetches
    // again or silently switches to a different remote/commit halfway through.
    save_hydration(repo, &state)?;
    continue_hydration(repo)
}

pub(crate) fn continue_hydration(repo: &Repository) -> Result<()> {
    crate::commands::sync::ensure_no_native_git_operation(repo)?;
    let mut state: HydrationState =
        serde_json::from_str(&std::fs::read_to_string(hydration_state_path(repo))?)?;
    for i in 0..state.steps.len() {
        if state.steps[i].completed {
            continue;
        }
        ensure_local_branch_for_checkout(repo, &state.steps[i])
            .context("Checkout hydration stopped. Fix the error and run 'kin continue', or 'kin abort' to stop hydration")?;
        state.steps[i].completed = true;
        save_hydration(repo, &state)?;
    }
    let mut plan = crate::overrides::Plan::default();
    crate::overrides::prepare(repo, plan.checkout_rev(repo, &state.branch))?;
    git_checkout(&state.branch).context(
        "Checkout hydration stopped. Run 'kin continue' to retry checkout or 'kin abort'",
    )?;
    std::fs::remove_file(hydration_state_path(repo))?;
    Ok(())
}

pub(crate) fn abort_hydration(repo: &Repository) -> Result<()> {
    // Hydration only adds branches; retaining them avoids deleting edits made
    // during an interruption (including a crash between creation and checkpoint).
    std::fs::remove_file(hydration_state_path(repo))?;
    println!("Checkout hydration aborted. Already-created branches were retained.");
    Ok(())
}

fn fetch_all_remotes(repo: &Repository) -> Result<()> {
    let output = Command::new("git")
        .current_dir(repo_root(repo)?)
        .args(["fetch", "--all", "--prune"])
        .output()
        .context("Failed to run `git fetch --all --prune`")?;

    if !output.status.success() {
        return Err(anyhow!(
            "git fetch --all --prune failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(())
}

fn ensure_local_branch_for_checkout(repo: &Repository, step: &HydrationStep) -> Result<()> {
    let tip = git2::Oid::from_str(&step.tip)?;
    let mut branch = match repo.find_branch(&step.branch, BranchType::Local) {
        Ok(branch) => {
            // Creation may have succeeded just before a checkpoint failed.
            if branch.get().target() != Some(tip) {
                return Err(anyhow!(
                    "Branch '{}' changed since hydration was planned; refusing to overwrite it",
                    step.branch
                ));
            }
            branch
        }
        Err(err) if err.code() == git2::ErrorCode::NotFound => {
            repo.branch(&step.branch, &repo.find_commit(tip)?, false)?
        }
        Err(err) => return Err(err.into()),
    };
    branch.set_upstream(step.remote_ref.as_deref())?;
    Ok(())
}

fn resolve_remote_tracking_ref(
    repo: &Repository,
    branch: &str,
    repository: &str,
) -> Result<Option<String>> {
    let mut candidates = Vec::new();
    for name in repo.remotes()?.iter().flatten() {
        let remote = repo.find_remote(name)?;
        // libgit2 applies insteadOf to remote.url(); retain the configured
        // GitHub identity when a transport rewrite uses a local/SSH alias.
        let configured = repo
            .config()?
            .get_string(&format!("remote.{name}.url"))
            .ok();
        let identity = remote
            .url()
            .and_then(gh::repository_identity)
            .or_else(|| configured.as_deref().and_then(gh::repository_identity));
        if identity.as_deref() != Some(repository) {
            continue;
        }
        let tracking = format!("{name}/{branch}");
        if let Ok(reference) = repo.find_branch(&tracking, BranchType::Remote) {
            candidates.push((tracking, reference.get().peel_to_commit()?.id()));
        }
    }
    candidates.sort();
    if let Some((_, first)) = candidates.first()
        && candidates.iter().any(|(_, tip)| tip != first)
    {
        return Err(anyhow!(
            "Ambiguous remote-tracking branches for '{}' in PR repository '{}'",
            branch,
            repository
        ));
    }
    Ok(candidates.into_iter().next().map(|(name, _)| name))
}

fn perform_git_checkout(name: &str) -> Result<()> {
    let repo = crate::open_repo()?;
    let _lock = crate::state_io::RepoLock::acquire(&repo)?;
    ensure_checkout_available(&repo)?;
    crate::overrides::with_planned(&repo, false, || {
        let mut plan = crate::overrides::Plan::default();
        crate::overrides::prepare(&repo, plan.checkout_rev(&repo, name))?;
        git_checkout(name)
    })
}

fn git_checkout(name: &str) -> Result<()> {
    let output = Command::new("git")
        .arg("checkout")
        .arg(name)
        .output()
        .with_context(|| format!("Failed to run `git checkout {name}`"))?;

    if !output.status.success() {
        return Err(anyhow!(
            "git checkout failed for branch '{}': {}",
            name,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(())
}

fn find_first_parent_branches_via_git_log(
    repo: &git2::Repository,
    upstream_name: &str,
    current_branch: &str,
) -> Result<Vec<String>> {
    let repo_root = if let Some(workdir) = repo.workdir() {
        workdir.to_path_buf()
    } else {
        repo.path()
            .parent()
            .ok_or_else(|| anyhow!("Failed to resolve repository root path."))?
            .to_path_buf()
    };

    let output = Command::new("git")
        .args([
            "log",
            "--first-parent",
            "--decorate=full",
            "--format=%H%x00%D",
            "HEAD",
            &format!("^{upstream_name}"),
        ])
        .current_dir(repo_root)
        .output()?;

    if !output.status.success() {
        return Err(anyhow!(
            "Failed to inspect first-parent ancestry via git log"
        ));
    }

    let stdout = String::from_utf8(output.stdout)?;
    let mut lines = stdout.lines();
    let _ = lines.next();

    for line in lines {
        let mut parts = line.splitn(2, '\0');
        let _commit = parts.next();
        let decorations = parts.next().unwrap_or("");
        let mut names = Vec::new();

        for token in decorations.split(',') {
            let item = token.trim();
            if item.is_empty() {
                continue;
            }

            let maybe_ref = if let Some(rest) = item.strip_prefix("HEAD -> ") {
                rest.trim()
            } else {
                item
            };

            if let Some(local) = maybe_ref.strip_prefix("refs/heads/")
                && local != current_branch
                && local != upstream_name
            {
                names.push(local.to_string());
            }
        }

        if !names.is_empty() {
            names.sort();
            names.dedup();
            return Ok(names);
        }
    }

    Ok(Vec::new())
}
