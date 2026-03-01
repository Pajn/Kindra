use crate::commands::find_upstream;
use crate::rebase_utils::{RebaseState, run_rebase_loop, save_state, state_path};
use crate::stack::{collect_descendants, find_parent_in_stack, get_stack_branches_from_merge_base};
use anyhow::{Context, Result, anyhow};
use git2::Repository;
use std::collections::HashMap;
use std::process::Command;

pub fn commit(args: &[String]) -> Result<()> {
    let repo = Repository::open(".").context("Failed to open git repository.")?;

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
    let merge_base = repo.merge_base(upstream_id, head_id)?;

    // Capture stack state BEFORE commit
    let all_branches_in_stack =
        get_stack_branches_from_merge_base(&repo, merge_base, &upstream_name)?;

    // Run the actual git commit
    let is_non_interactive = args
        .iter()
        .any(|arg| arg == "-m" || arg == "--message" || arg == "-F" || arg == "--file");

    if is_non_interactive {
        let output = Command::new("git").arg("commit").args(args).output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let error_msg = if !stderr.is_empty() {
                stderr.to_string()
            } else if !stdout.is_empty() {
                stdout.to_string()
            } else {
                "unknown error".to_string()
            };
            return Err(anyhow!("git commit failed: {}", error_msg.trim()));
        }
    } else {
        let status = Command::new("git").arg("commit").args(args).status()?;
        if !status.success() {
            return Err(anyhow!("git commit failed with status {}", status));
        }
    }

    // Refresh repo state after commit

    let repo = Repository::open(".").context("Failed to reopen git repository.")?;
    let new_head = repo.head()?;
    let new_head_id = new_head.peel_to_commit()?.id();

    if head_id == new_head_id {
        return Ok(());
    }

    let mut sub_stack = Vec::new();
    if current_branch_name == upstream_name {
        // If we are on main, we want to rebase everything that was on top of the old main.
        // head_id is the commit we just committed ON TOP OF (the old main).
        crate::stack::collect_descendants_of_id(
            &repo,
            head_id,
            &all_branches_in_stack,
            &mut sub_stack,
        )?;
    } else {
        collect_descendants(
            &repo,
            &current_branch_name,
            &all_branches_in_stack,
            &mut sub_stack,
        )?;
    }

    if sub_stack.is_empty() || (sub_stack.len() == 1 && sub_stack[0].name == current_branch_name) {
        // No descendants to rebase
        return Ok(());
    }

    // Sort sub_stack by topology
    crate::stack::sort_branches_topologically(&repo, &mut sub_stack)?;

    let mut parent_id_map = HashMap::new();
    let mut parent_name_map = HashMap::new();
    for sb in &sub_stack {
        let parent_id = find_parent_in_stack(&repo, &sb.name, &all_branches_in_stack, merge_base)?;
        parent_id_map.insert(sb.name.clone(), parent_id.to_string());

        // If the parent is also in our sub_stack, store its name
        if let Some(parent_branch) = sub_stack
            .iter()
            .find(|p| p.id == parent_id && p.name != sb.name)
        {
            parent_name_map.insert(sb.name.clone(), parent_branch.name.clone());
        } else if parent_id == head_id {
            // If the parent is the branch we just committed on (root of move)
            parent_name_map.insert(sb.name.clone(), current_branch_name.clone());
        }
    }

    let remaining_branches: Vec<String> = sub_stack
        .iter()
        .filter(|sb| sb.name != current_branch_name)
        .map(|sb| sb.name.clone())
        .collect();

    let state = RebaseState {
        operation: crate::rebase_utils::Operation::Commit,
        original_branch: current_branch_name.clone(),
        target_branch: current_branch_name,
        remaining_branches,
        in_progress_branch: None,
        parent_id_map,
        parent_name_map,
    };

    save_state(&repo, &state)?;
    run_rebase_loop(&repo, state)
}
