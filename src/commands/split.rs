use crate::commands::{CommitInfo, find_upstream};
use crate::stack::{collect_path_branches, get_stack_tips};
use anyhow::{Context, Result, anyhow};
use git2::{BranchType, ErrorCode, Oid, Repository};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use tempfile::NamedTempFile;

pub fn split() -> Result<()> {
    let repo = crate::open_repo()?;
    let _lock = crate::state_io::RepoLock::acquire(&repo)?;

    if crate::rebase_utils::passively_reconcile_rebase_state(&repo)?
        || crate::commands::run::run_state_exists(&repo)
    {
        return Err(anyhow!(
            "A Kindra operation is already in progress. Use 'kin continue' or 'kin abort'."
        ));
    }
    crate::commands::sync::ensure_no_native_git_operation(&repo)?;

    let upstream_name = find_upstream(&repo)?.ok_or_else(|| {
        anyhow!("Could not find a base branch (init.defaultBranch, main, master, or trunk)")
    })?;
    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let upstream_id = upstream_obj.id();
    let head_obj = repo.revparse_single("HEAD")?;
    let head_id = head_obj.id();

    let merge_base = repo.merge_base(upstream_id, head_id)?;

    let stack_branches = crate::stack::get_stack_branches_from_merge_base(
        &repo,
        merge_base,
        head_id,
        upstream_id,
        &upstream_name,
    )?;
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
    let path_branches = collect_path_branches(&repo, target_tip_id, merge_base, &stack_branches)?;

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
    for branch in &path_branches {
        let id_str = branch.id.to_string();
        if commit_ids.contains(&id_str) {
            commit_to_branches
                .entry(id_str)
                .or_default()
                .push(branch.name.clone());
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

    buffer.push_str("\n# kin split\n");
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
            if branch_name.is_empty() || !git2::Branch::name_is_valid(&branch_name)? {
                return Err(anyhow!(
                    "Invalid branch name '{}' in split editor buffer",
                    branch_name
                ));
            }
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
            "Commit list was modified (count changed). kin split only supports branch management."
        ));
    }

    for (original, new_short) in commits.iter().zip(new_commits_short.iter()) {
        if !original.id.starts_with(new_short) {
            return Err(anyhow!(
                "Commit '{}' was modified or moved. kin split only supports branch management.",
                new_short
            ));
        }
    }

    let mut next_branches: HashMap<String, String> = HashMap::new();
    let mut new_branch_map_full: Vec<(String, String)> = Vec::new();
    for (name, id_short) in &new_branch_map {
        let name = name.clone();
        let id_short = id_short.clone();
        // Map short ID back to full ID
        let matches: Vec<_> = commits
            .iter()
            .filter(|c| c.id.starts_with(&id_short))
            .collect();

        if matches.is_empty() {
            return Err(anyhow!(
                "Could not resolve commit {} for branch {}",
                id_short,
                name
            ));
        } else if matches.len() > 1 {
            let candidates: Vec<_> = matches.iter().map(|c| &c.id[..7]).collect();
            return Err(anyhow!(
                "Ambiguous commit prefix {} for branch {}. Candidates: {:?}",
                id_short,
                name,
                candidates
            ));
        }

        if next_branches.contains_key(&name) {
            return Err(anyhow!("Duplicate branch row for branch {}", name));
        }

        let full_id = matches[0].id.clone();
        next_branches.insert(name.clone(), full_id.clone());
        new_branch_map_full.push((name, full_id));
    }

    // Apply changes
    apply_split(
        &repo,
        next_branches,
        new_branch_map_full,
        path_branches.iter().map(|b| b.name.clone()).collect(),
        commit_ids,
    )?;

    Ok(())
}

fn apply_split(
    repo: &Repository,
    next_branches: HashMap<String, String>,
    new_branch_map: Vec<(String, String)>,
    initial_branches: Vec<String>,
    allowed_ids: HashSet<String>,
) -> Result<()> {
    let initial_names: HashSet<String> = initial_branches.into_iter().collect();

    // 1. Pre-flight validation and commit resolution.
    // This phase performs no ref mutations: it only resolves commits and decides
    // which branches to create, move, skip, or delete.
    let mut resolved_commits = Vec::new(); // (branch_name, commit_to_set, should_overwrite)
    let mut skip_branches = HashSet::new();

    for (name, id) in &next_branches {
        let commit_obj = repo.revparse_single(id).context(format!(
            "Failed to resolve target commit {} for branch {}",
            id, name
        ))?;
        let _ = commit_obj
            .as_commit()
            .ok_or_else(|| anyhow!("Target {} for branch {} is not a commit", id, name))?;

        match repo.find_branch(name, BranchType::Local) {
            Ok(existing) => {
                let target = existing.get().target();
                let target_str = target.map(|t| t.to_string());
                if target_str.as_deref() == Some(id) {
                    skip_branches.insert(name.clone());
                    continue;
                }

                // Guard: Only allow moving an existing branch if it was part of the original
                // stack (by name or by pointing to one of the commits in the stack).
                let is_safe = initial_names.contains(name)
                    || target_str.as_ref().is_some_and(|t| allowed_ids.contains(t));

                if !is_safe {
                    let confirm_msg = format!(
                        "Branch '{}' already exists and is NOT part of the stack. Do you want to overwrite it?",
                        name
                    );
                    if !crate::commands::prompt_confirm(&confirm_msg)? {
                        println!("Skipping branch '{}'", name);
                        skip_branches.insert(name.clone());
                        continue;
                    }
                }
                resolved_commits.push((name.clone(), id.clone(), true));
            }
            Err(e) if e.code() == ErrorCode::NotFound => {
                resolved_commits.push((name.clone(), id.clone(), false));
            }
            Err(e) => {
                return Err(anyhow!(e)
                    .context(format!("Failed to find branch {} during application", name)));
            }
        }
    }

    let delete_names: Vec<String> = initial_names
        .iter()
        .filter(|name| !next_branches.contains_key(*name))
        .cloned()
        .collect();

    // 2. Snapshot every ref we might touch, plus HEAD, so a mid-apply failure can
    // be rolled back to the pre-split state rather than left half-applied.
    let mut touched: Vec<String> = resolved_commits
        .iter()
        .filter(|(name, _, _)| !skip_branches.contains(name))
        .map(|(name, _, _)| name.clone())
        .collect();
    touched.extend(delete_names.iter().cloned());
    let snapshot = snapshot_branches(repo, &touched)?;
    let head_snapshot = HeadSnapshot::capture(repo)?;

    // 3. Perform the ref mutations, rolling back on any error.
    let result = apply_split_mutations(
        repo,
        &resolved_commits,
        &delete_names,
        &skip_branches,
        &new_branch_map,
    );

    if let Err(err) = result {
        eprintln!("kin split failed partway through: {err:#}");
        eprintln!("Rolling back branch changes to the pre-split state...");
        if let Err(rollback_err) = restore_branches(repo, &snapshot) {
            eprintln!(
                "Warning: rollback of branch refs was incomplete: {rollback_err:#}. Use 'git reflog' to recover."
            );
        }
        if let Err(rollback_err) = head_snapshot.restore(repo) {
            eprintln!("Warning: rollback of HEAD was incomplete: {rollback_err:#}");
        }
        return Err(err.context("kin split was aborted and rolled back"));
    }

    Ok(())
}

/// Apply the resolved branch creations/moves and deletions.
///
/// Creations and moves run first because they are non-destructive: the original
/// branch tips still exist until the delete phase, so a failure here leaves the
/// stack recoverable. Deletions run last and echo the old tip SHA so a mistaken
/// delete can be undone from the printed value or the reflog.
fn apply_split_mutations(
    repo: &Repository,
    resolved_commits: &[(String, String, bool)],
    delete_names: &[String],
    skip_branches: &HashSet<String>,
    new_branch_map: &[(String, String)],
) -> Result<()> {
    let current_branch = current_branch_name(repo)?;

    for (name, id, force) in resolved_commits {
        if skip_branches.contains(name) {
            continue;
        }

        let commit_obj = repo.revparse_single(id)?;
        let commit = commit_obj
            .as_commit()
            .ok_or_else(|| anyhow!("Target {} for branch {} is not a commit", id, name))?;

        if Some(name) == current_branch.as_ref() {
            println!("Detaching HEAD to move current branch: {}", name);
            detach_head(repo)?;
        }

        repo.branch(name, commit, *force)?;
        if *force {
            println!("Moved branch: {} -> {}", name, &id[..7]);
        } else {
            println!("Created branch: {} -> {}", name, &id[..7]);
        }
    }

    for name in delete_names {
        match repo.find_branch(name, BranchType::Local) {
            Ok(mut branch) => {
                let old_tip = branch.get().target().map(|t| t.to_string());
                if Some(name) == current_branch.as_ref() && !repo.head_detached()? {
                    println!(
                        "Cannot delete current branch: {}. Detaching HEAD first.",
                        name
                    );
                    detach_head(repo)?;
                }
                branch.delete()?;
                match old_tip {
                    Some(old) => println!("Deleted branch: {} (was {})", name, &old[..7]),
                    None => println!("Deleted branch: {}", name),
                }
            }
            Err(e) if e.code() == ErrorCode::NotFound => {
                // Branch already gone, skip.
            }
            Err(e) => {
                return Err(
                    anyhow!(e).context(format!("Failed to find branch {} for deletion", name))
                );
            }
        }
    }

    if repo.head_detached()? {
        let head_commit = repo.head()?.peel_to_commit()?;
        let head_id_str = head_commit.id().to_string();
        for (name, commit_id) in new_branch_map {
            if commit_id != &head_id_str {
                continue;
            }
            // A skipped overwrite branch may still target its old commit even
            // though its desired commit equals HEAD. Only reattach to a branch
            // that actually points at HEAD now.
            let points_at_head = repo
                .find_branch(name, BranchType::Local)
                .ok()
                .and_then(|branch| branch.get().target())
                .is_some_and(|target| target == head_commit.id());
            if points_at_head {
                repo.set_head(&format!("refs/heads/{}", name))?;
                break;
            }
        }
    }

    Ok(())
}

fn current_branch_name(repo: &Repository) -> Result<Option<String>> {
    if repo.head_detached()? {
        Ok(None)
    } else {
        Ok(repo.head()?.shorthand().map(|s| s.to_string()))
    }
}

fn detach_head(repo: &Repository) -> Result<()> {
    let head_commit = repo.head()?.peel_to_commit()?;
    repo.set_head_detached(head_commit.id())?;
    Ok(())
}

/// Record the current tip of each named branch (`None` if it does not exist yet)
/// so it can be restored on rollback.
fn snapshot_branches(repo: &Repository, names: &[String]) -> Result<Vec<(String, Option<Oid>)>> {
    let mut seen = HashSet::new();
    let mut snapshot = Vec::new();
    for name in names {
        if !seen.insert(name.clone()) {
            continue;
        }
        let target = match repo.find_branch(name, BranchType::Local) {
            Ok(branch) => branch.get().target(),
            Err(e) if e.code() == ErrorCode::NotFound => None,
            Err(e) => {
                return Err(anyhow!(e).context(format!("Failed to snapshot branch {}", name)));
            }
        };
        snapshot.push((name.clone(), target));
    }
    Ok(snapshot)
}

/// Best-effort restore of branches to their snapshotted tips. Continues past
/// individual failures and returns the first error encountered, if any.
fn restore_branches(repo: &Repository, snapshot: &[(String, Option<Oid>)]) -> Result<()> {
    let mut first_err: Option<anyhow::Error> = None;
    for (name, target) in snapshot {
        let outcome = match target {
            Some(oid) => repo
                .reference(
                    &format!("refs/heads/{name}"),
                    *oid,
                    true,
                    "kin split rollback",
                )
                .map(|_| ())
                .map_err(anyhow::Error::from),
            None => match repo.find_branch(name, BranchType::Local) {
                Ok(mut branch) => branch.delete().map_err(anyhow::Error::from),
                Err(e) if e.code() == ErrorCode::NotFound => Ok(()),
                Err(e) => Err(anyhow::Error::from(e)),
            },
        };
        if let Err(e) = outcome
            && first_err.is_none()
        {
            first_err = Some(e.context(format!("failed to restore branch {name}")));
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Snapshot of HEAD taken before mutations so it can be restored on rollback.
enum HeadSnapshot {
    Branch(String),
    Detached(Oid),
}

impl HeadSnapshot {
    fn capture(repo: &Repository) -> Result<Self> {
        if repo.head_detached()? {
            let oid = repo.head()?.peel_to_commit()?.id();
            Ok(HeadSnapshot::Detached(oid))
        } else {
            let name = repo
                .head()?
                .shorthand()
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow!("Could not determine current branch for HEAD snapshot"))?;
            Ok(HeadSnapshot::Branch(name))
        }
    }

    fn restore(&self, repo: &Repository) -> Result<()> {
        match self {
            HeadSnapshot::Branch(name) => {
                repo.set_head(&format!("refs/heads/{name}"))?;
            }
            HeadSnapshot::Detached(oid) => {
                repo.set_head_detached(*oid)?;
            }
        }
        Ok(())
    }
}
