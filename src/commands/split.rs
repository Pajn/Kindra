use crate::commands::{CommitInfo, find_upstream};
use crate::stack::get_stack_tips;
use anyhow::{Context, Result, anyhow};
use git2::{BranchType, Repository};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use tempfile::NamedTempFile;

pub fn split() -> Result<()> {
    let repo =
        Repository::open(".").context("Failed to open git repository. Are you in a git repo?")?;

    let upstream_name = find_upstream(&repo)?;
    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let upstream_id = upstream_obj.id();
    let head_obj = repo.revparse_single("HEAD")?;
    let head_id = head_obj.id();

    let merge_base = repo.merge_base(upstream_id, head_id)?;

    let stack_branches =
        crate::stack::get_stack_branches_from_merge_base(&repo, merge_base, &upstream_name)?;
    let mut tips = get_stack_tips(&repo, &stack_branches)?;
    tips.sort();

    // If there are multiple tips, the user must choose one.
    // If there are no tips (meaning no branches on the stack), we default to HEAD.
    let (target_tip_name, target_tip_id) = match tips.len() {
        0 => ("HEAD".to_string(), head_id),
        1 => (tips[0].clone(), repo.revparse_single(&tips[0])?.id()),
        _ => {
            let selected = crate::commands::prompt_select(
                "Multiple stack tips found. Which path are you splitting?",
                tips,
            )?;
            let id = repo.revparse_single(&selected)?.id();
            (selected, id)
        }
    };

    // Now we only care about branches that are on the linear path to the target tip.
    let mut path_branches = Vec::new();
    for b in stack_branches {
        let is_on_path = (repo.graph_descendant_of(target_tip_id, b.id)? || target_tip_id == b.id)
            && (repo.graph_descendant_of(b.id, merge_base)? || b.id == merge_base);
        if is_on_path {
            path_branches.push(b);
        }
    }

    let mut revwalk = repo.revwalk()?;
    revwalk.push(target_tip_id)?;
    revwalk.hide(merge_base)?;
    revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;

    let mut commits = Vec::new();
    let mut commit_ids = HashSet::new();
    for id in revwalk {
        let id = id?;
        let commit = repo.find_commit(id)?;
        let id_str = id.to_string();
        commits.push(CommitInfo {
            id: id_str.clone(),
            summary: commit.summary().unwrap_or("").to_string(),
        });
        commit_ids.insert(id_str);
    }

    if commits.is_empty() {
        println!("No commits to manage between HEAD and {}", upstream_name);
        return Ok(());
    }

    // Map commits to branches (only local branches pointing into our path)
    let mut commit_to_branches: HashMap<String, Vec<String>> = HashMap::new();
    for branch in path_branches {
        let id_str = branch.id.to_string();
        if commit_ids.contains(&id_str) {
            commit_to_branches
                .entry(id_str)
                .or_default()
                .push(branch.name);
        }
    }

    // Generate buffer
    let mut buffer = String::new();
    for commit in &commits {
        buffer.push_str(&format!("{} {}\n", &commit.id[..7], commit.summary));
        if let Some(branch_names) = commit_to_branches.get(&commit.id) {
            for name in branch_names {
                buffer.push_str(&format!("branch {}\n", name));
            }
        }
    }

    buffer.push_str("\n# gits split\n");
    buffer.push_str("# Move 'branch <name>' rows to reassign branches to commits.\n");
    buffer.push_str("# Add new 'branch <name>' rows to create branches.\n");
    buffer.push_str("# Remove 'branch <name>' rows to delete branches.\n");
    buffer.push_str("# DO NOT edit commit lines (SHA + summary).\n");
    buffer.push_str(&format!("# Base branch: {}\n", upstream_name));
    buffer.push_str(&format!("# Path to tip: {}\n", target_tip_name));

    // Open editor
    let mut temp_file = NamedTempFile::new()?;
    temp_file.write_all(buffer.as_bytes())?;
    let temp_path = temp_file.path().to_path_buf();

    crate::editor::launch_editor(&temp_path)?;

    let edited_buffer = fs::read_to_string(&temp_path)?;

    // Parse and Validate
    let mut new_commits_short = Vec::new();
    let mut new_branch_map: Vec<(String, String)> = Vec::new(); // (branch_name, commit_id_short)
    let mut last_commit_id: Option<String> = None;

    for line in edited_buffer.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line.starts_with("branch ") {
            let branch_name = line.strip_prefix("branch ").unwrap().trim().to_string();
            if let Some(id) = &last_commit_id {
                new_branch_map.push((branch_name, id.clone()));
            } else {
                return Err(anyhow!(
                    "Branch '{}' must follow a commit line",
                    branch_name
                ));
            }
        } else {
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.is_empty() {
                continue;
            }
            let id = parts[0].to_string();
            new_commits_short.push(id.clone());
            last_commit_id = Some(id);
        }
    }

    // Validate commits (order and content must match exactly)
    if new_commits_short.len() != commits.len() {
        return Err(anyhow!(
            "Commit list was modified (count changed). gits split only supports branch management."
        ));
    }

    for (original, new_short) in commits.iter().zip(new_commits_short.iter()) {
        if !original.id.starts_with(new_short) {
            return Err(anyhow!(
                "Commit '{}' was modified or moved. gits split only supports branch management.",
                new_short
            ));
        }
    }

    // Apply changes
    let head_detached = repo.head_detached()?;
    let current_branch = if !head_detached {
        repo.head()?.shorthand().map(|s| s.to_string())
    } else {
        None
    };

    let existing_managed_branches: HashMap<String, String> = commit_to_branches
        .into_iter()
        .flat_map(|(id, names)| names.into_iter().map(move |name| (name, id.clone())))
        .collect();

    let mut next_branches: HashMap<String, String> = HashMap::new();
    for (name, id_short) in new_branch_map {
        // Map short ID back to full ID
        let full_id = commits
            .iter()
            .find(|c| c.id.starts_with(&id_short))
            .map(|c| c.id.clone())
            .ok_or_else(|| anyhow!("Could not resolve commit {}", id_short))?;
        next_branches.insert(name, full_id);
    }

    // Delete branches
    for name in existing_managed_branches.keys() {
        if !next_branches.contains_key(name) {
            if Some(name) == current_branch.as_ref() {
                println!(
                    "Cannot delete current branch: {}. Detaching HEAD first.",
                    name
                );
                let head_commit = repo.head()?.peel_to_commit()?;
                repo.set_head_detached(head_commit.id())?;
            }
            let mut branch = repo.find_branch(name, BranchType::Local)?;
            branch.delete()?;
            println!("Deleted branch: {}", name);
        }
    }

    // Create or move branches
    for (name, id) in next_branches {
        let commit_obj = repo.revparse_single(&id)?;
        let commit = commit_obj
            .as_commit()
            .ok_or_else(|| anyhow!("{} is not a commit", id))?;

        match repo.find_branch(&name, BranchType::Local) {
            Ok(existing) => {
                let target = existing.get().target();
                if target.map(|t| t.to_string()) == Some(id.clone()) {
                    continue;
                }

                if Some(&name) == current_branch.as_ref() {
                    println!("Detaching HEAD to move current branch: {}", name);
                    let head_commit = repo.head()?.peel_to_commit()?;
                    repo.set_head_detached(head_commit.id())?;
                }

                repo.branch(&name, commit, true)?;
                println!("Moved branch: {} -> {}", name, &id[..7]);
            }
            Err(_) => {
                repo.branch(&name, commit, false)?;
                println!("Created branch: {} -> {}", name, &id[..7]);
            }
        }
    }

    Ok(())
}
