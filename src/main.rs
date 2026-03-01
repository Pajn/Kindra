use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use git2::{BranchType, Repository};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::process::Command;
use tempfile::NamedTempFile;

#[derive(Parser)]
#[command(name = "gits")]
#[command(about = "A wrapper around git to aid certain workflows", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Opens $EDITOR to manage branches in a stack of commits
    Split,
}

struct CommitInfo {
    id: String,
    summary: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Split => split()?,
    }

    Ok(())
}

fn split() -> Result<()> {
    let repo = Repository::open(".")
        .context("Failed to open git repository. Are you in a git repo?")?;

    let upstream_name = find_upstream(&repo)?;
    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let head_obj = repo.revparse_single("HEAD")?;

    let merge_base = repo.merge_base(upstream_obj.id(), head_obj.id())?;

    let mut revwalk = repo.revwalk()?;
    revwalk.push(head_obj.id())?;
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

    // Map commits to branches (only local branches pointing into our stack)
    let mut commit_to_branches: HashMap<String, Vec<String>> = HashMap::new();
    let branches = repo.branches(Some(BranchType::Local))?;
    for branch_res in branches {
        let (branch, _) = branch_res?;
        if let Some(name) = branch.name()? {
            // Don't manage the upstream branch itself
            if name == upstream_name {
                continue;
            }
            if let Some(target) = branch.get().target() {
                let target_str = target.to_string();
                if commit_ids.contains(&target_str) {
                    commit_to_branches.entry(target_str).or_default().push(name.to_string());
                }
            }
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

    // Open editor
    let mut temp_file = NamedTempFile::new()?;
    temp_file.write_all(buffer.as_bytes())?;
    let temp_path = temp_file.path().to_path_buf();

    let editor = env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let status = Command::new(&editor)
        .arg(&temp_path)
        .status()
        .context(format!("Failed to launch editor: {}", editor))?;

    if !status.success() {
        return Err(anyhow!("Editor exited with non-zero status"));
    }

    let mut edited_buffer = String::new();
    fs::File::open(&temp_path)?.read_to_string(&mut edited_buffer)?;

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
                return Err(anyhow!("Branch '{}' must follow a commit line", branch_name));
            }
        } else {
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.is_empty() { continue; }
            let id = parts[0].to_string();
            new_commits_short.push(id.clone());
            last_commit_id = Some(id);
        }
    }

    // Validate commits (order and content must match exactly)
    if new_commits_short.len() != commits.len() {
        return Err(anyhow!("Commit list was modified (count changed). gits split only supports branch management."));
    }

    for (original, new_short) in commits.iter().zip(new_commits_short.iter()) {
        if !original.id.starts_with(new_short) {
            return Err(anyhow!("Commit '{}' was modified or moved. gits split only supports branch management.", new_short));
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
        let full_id = commits.iter()
            .find(|c| c.id.starts_with(&id_short))
            .map(|c| c.id.clone())
            .ok_or_else(|| anyhow!("Could not resolve commit {}", id_short))?;
        next_branches.insert(name, full_id);
    }

    // Delete branches
    for name in existing_managed_branches.keys() {
        if !next_branches.contains_key(name) {
            if Some(name) == current_branch.as_ref() {
                println!("Cannot delete current branch: {}. Detaching HEAD first.", name);
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
        let commit = commit_obj.as_commit().ok_or_else(|| anyhow!("{} is not a commit", id))?;

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

fn find_upstream(repo: &Repository) -> Result<String> {
    let candidates = ["main", "master", "origin/main", "origin/master"];
    for name in candidates {
        if let Ok(_) = repo.revparse_single(name) {
            // Check if it's a local branch first
            if repo.find_branch(name, BranchType::Local).is_ok() {
                return Ok(name.to_string());
            }
        }
    }
    // Fallback to the first candidate that exists at all
    for name in candidates {
        if let Ok(_) = repo.revparse_single(name) {
            return Ok(name.to_string());
        }
    }
    Err(anyhow!("Could not find a base branch (main or master)"))
}
