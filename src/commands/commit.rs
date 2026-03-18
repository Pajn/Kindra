use crate::commands::{find_upstream, resolve_rebase_autostash};
use crate::rebase_utils::{
    RebaseState, check_worktrees, checkout_branch, run_rebase_loop, save_state, state_path,
};
use crate::stack::{StackBranch, collect_descendants, get_stack_branches_from_merge_base};
use anyhow::{Context, Result, anyhow};
use git2::{BranchType, Oid, Repository};
use std::collections::HashMap;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn commit(args: &[String]) -> Result<()> {
    let repo = crate::open_repo()?;

    let path = state_path(&repo);
    if path.exists() {
        return Err(anyhow!(
            "A move or commit operation is already in progress. Use 'gits continue' or 'gits abort'."
        ));
    }

    let head = repo.head()?;
    let current_branch_name = if !repo.head_detached()? {
        head.shorthand().map(|s| s.to_string())
    } else {
        None
    }
    .ok_or_else(|| anyhow!("You must be on a branch to use 'commit'"))?;

    let upstream_name = find_upstream(&repo)?;
    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let upstream_id = upstream_obj.id();
    let head_id = head.peel_to_commit()?.id();
    let parsed = parse_commit_args(args)?;
    let autostash = resolve_rebase_autostash(&repo, parsed.autostash)?;
    let on_flag = parsed.on_target.is_some();

    let current_stack = build_stack_context(&repo, head_id, upstream_id, &upstream_name)
        .with_context(|| {
            format!(
                "Failed to discover stack context for current branch '{}'.",
                current_branch_name
            )
        })?;

    let target_branch = match parsed.on_target {
        None => current_branch_name.clone(),
        Some(Some(branch_name)) => branch_name,
        Some(None) => select_target_branch(
            &repo,
            &current_branch_name,
            head_id,
            &current_stack.stack_branches,
        )?,
    };

    repo.find_branch(&target_branch, BranchType::Local)
        .with_context(|| format!("Target branch '{}' not found.", target_branch))?;
    let target_old_head_id = repo.revparse_single(&target_branch)?.id();
    let target_in_current_context = target_branch == upstream_name
        || current_stack
            .stack_branches
            .iter()
            .any(|b| b.name == target_branch);

    let target_stack = build_stack_context(&repo, target_old_head_id, upstream_id, &upstream_name)?;
    let target_sub_stack = collect_target_sub_stack(
        &repo,
        &target_branch,
        target_old_head_id,
        &upstream_name,
        &target_stack.stack_branches,
    )?;
    let target_has_dependents =
        has_dependents_to_rebase(&target_branch, &upstream_name, &target_sub_stack);

    let should_rebase = if !target_in_current_context && on_flag && target_has_dependents {
        crate::commands::prompt_confirm(&format!(
            "Branch '{}' has dependent branches in another stack. Rebase that stack as well?",
            target_branch
        ))?
    } else {
        true
    };

    let switching_branches = target_branch != current_branch_name;
    let mut sub_stack = target_sub_stack;
    crate::stack::sort_branches_topologically(&repo, &mut sub_stack)?;

    let remaining_branches: Vec<String> = sub_stack
        .iter()
        .filter(|sb| sb.name != target_branch)
        .map(|sb| sb.name.clone())
        .collect();

    let will_rebase = should_rebase && target_has_dependents && !remaining_branches.is_empty();

    // The check_worktrees call must run before the code path that performs the commit and
    // mutates target_branch so failures don't leave state unpersisted.
    if will_rebase {
        check_worktrees(&remaining_branches, parsed.force)?;
    }

    if switching_branches || will_rebase {
        let stash_ref = if switching_branches {
            stash_non_staged_changes()?
        } else {
            None
        };

        let (parent_id_map, parent_name_map) = if will_rebase {
            crate::stack::build_parent_maps(
                &repo,
                &sub_stack,
                &target_stack.stack_branches,
                target_stack.merge_base,
                target_old_head_id,
                &target_branch,
            )?
        } else {
            (HashMap::new(), HashMap::new())
        };

        let state = RebaseState {
            operation: crate::rebase_utils::Operation::Commit,
            original_branch: target_branch.clone(),
            target_branch: target_branch.clone(),
            caller_branch: if switching_branches {
                Some(current_branch_name)
            } else {
                None
            },
            remaining_branches: if will_rebase {
                remaining_branches
            } else {
                Vec::new()
            },
            in_progress_branch: None,
            parent_id_map,
            parent_name_map,
            new_base_map: HashMap::new(),
            stash_ref,
            unstage_on_restore: switching_branches,
            autostash,
        };

        save_state(&repo, &state)?;

        if switching_branches && let Err(err) = checkout_branch(&target_branch) {
            return Err(err.context(
                "Failed to checkout target branch. Use 'gits abort' to restore original state.",
            ));
        }

        // Run the actual git commit
        let status = Command::new("git")
            .arg("commit")
            .args(&parsed.git_commit_args)
            .status()?;
        if !status.success() {
            return Err(anyhow!(
                "git commit failed. Resolve and run 'gits continue', or run 'gits abort'."
            ));
        }

        // Refresh repo state after commit
        let repo = crate::open_repo()?;
        let _new_target_head_id = repo.revparse_single(&target_branch)?.id();

        run_rebase_loop(&repo, state)
    } else {
        // Run the actual git commit
        let status = Command::new("git")
            .arg("commit")
            .args(&parsed.git_commit_args)
            .status()?;
        if !status.success() {
            return Err(anyhow!("git commit failed"));
        }
        Ok(())
    }
}

struct StackContext {
    merge_base: Oid,
    stack_branches: Vec<StackBranch>,
}

#[derive(Default)]
struct ParsedCommitArgs {
    on_target: Option<Option<String>>,
    force: bool,
    autostash: Option<bool>,
    git_commit_args: Vec<String>,
}

fn parse_commit_args(args: &[String]) -> Result<ParsedCommitArgs> {
    let mut parsed = ParsedCommitArgs::default();
    let mut idx = 0;

    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--" {
            parsed.git_commit_args.extend(args[idx..].iter().cloned());
            break;
        }

        if arg == "--force" {
            parsed.force = true;
            idx += 1;
            continue;
        }

        if arg == "--autostash" {
            parsed.autostash = Some(true);
            idx += 1;
            continue;
        }

        if arg == "--no-autostash" {
            parsed.autostash = Some(false);
            idx += 1;
            continue;
        }

        if arg == "--on" {
            if parsed.on_target.is_some() {
                return Err(anyhow!("--on can only be specified once."));
            }
            if idx + 1 == args.len() {
                parsed.on_target = Some(None);
                idx += 1;
                continue;
            }
            if args[idx + 1].starts_with('-') {
                return Err(anyhow!(
                    "When using '--on', provide a branch name or use '--on=' for interactive selection."
                ));
            }
            parsed.on_target = Some(Some(args[idx + 1].clone()));
            idx += 2;
            continue;
        }

        if let Some(value) = arg.strip_prefix("--on=") {
            if parsed.on_target.is_some() {
                return Err(anyhow!("--on can only be specified once."));
            }
            if value.is_empty() {
                parsed.on_target = Some(None);
            } else {
                parsed.on_target = Some(Some(value.to_string()));
            }
            idx += 1;
            continue;
        }

        parsed.git_commit_args.push(arg.clone());
        idx += 1;
    }

    Ok(parsed)
}

fn build_stack_context(
    repo: &Repository,
    head_id: Oid,
    upstream_id: Oid,
    upstream_name: &str,
) -> Result<StackContext> {
    let merge_base = repo.merge_base(upstream_id, head_id)?;
    let stack_branches =
        get_stack_branches_from_merge_base(repo, merge_base, head_id, upstream_id, upstream_name)?;
    Ok(StackContext {
        merge_base,
        stack_branches,
    })
}

fn select_target_branch(
    repo: &Repository,
    current_branch_name: &str,
    current_head_id: Oid,
    stack_branches: &[StackBranch],
) -> Result<String> {
    let mut options = stack_branches.to_vec();
    if !options.iter().any(|b| b.name == current_branch_name) {
        options.push(StackBranch {
            name: current_branch_name.to_string(),
            id: current_head_id,
        });
    }

    if options.is_empty() {
        return Err(anyhow!(
            "No branches found in the current stack to commit onto."
        ));
    }

    crate::stack::sort_branches_topologically(repo, &mut options)?;
    let display: Vec<String> = options
        .iter()
        .map(|b| {
            if b.name == current_branch_name {
                format!("* {}", b.name)
            } else {
                format!("  {}", b.name)
            }
        })
        .collect();
    let selected_display =
        crate::commands::prompt_select("Select branch to commit onto:", display)?;
    options
        .iter()
        .find(|b| {
            let rendered = if b.name == current_branch_name {
                format!("* {}", b.name)
            } else {
                format!("  {}", b.name)
            };
            rendered == selected_display
        })
        .map(|b| b.name.clone())
        .ok_or_else(|| anyhow!("Failed to resolve selected branch '{}'.", selected_display))
}

fn collect_target_sub_stack(
    repo: &Repository,
    target_branch: &str,
    target_head_id: Oid,
    upstream_name: &str,
    all_branches_in_stack: &[StackBranch],
) -> Result<Vec<StackBranch>> {
    let mut sub_stack = Vec::new();
    if target_branch == upstream_name {
        crate::stack::collect_descendants_of_id(
            repo,
            target_head_id,
            all_branches_in_stack,
            &mut sub_stack,
        )?;
    } else if all_branches_in_stack
        .iter()
        .any(|b| b.name == target_branch)
    {
        collect_descendants(repo, target_branch, all_branches_in_stack, &mut sub_stack)?;
    }
    Ok(sub_stack)
}

fn has_dependents_to_rebase(
    target_branch: &str,
    upstream_name: &str,
    sub_stack: &[StackBranch],
) -> bool {
    if target_branch == upstream_name {
        !sub_stack.is_empty()
    } else {
        sub_stack.iter().any(|b| b.name != target_branch)
    }
}

fn stash_head_ref() -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("--verify")
        .arg("-q")
        .arg("refs/stash")
        .output()?;
    if output.status.success() {
        let ref_name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if ref_name.is_empty() {
            Ok(None)
        } else {
            Ok(Some(ref_name))
        }
    } else {
        Ok(None)
    }
}

fn stash_non_staged_changes() -> Result<Option<String>> {
    let before = stash_head_ref()?;
    let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let message = format!("gits-commit-on-{}-{}", std::process::id(), ts);
    let status = Command::new("git")
        .arg("stash")
        .arg("push")
        .arg("--keep-index")
        .arg("--include-untracked")
        .arg("-m")
        .arg(&message)
        .status()?;
    if !status.success() {
        return Err(anyhow!("Failed to stash non-staged files."));
    }
    let after = stash_head_ref()?;
    if after != before {
        Ok(Some(message))
    } else {
        Ok(None)
    }
}
