use crate::commands::find_upstream;
use crate::rebase_utils::state_path;
use crate::stack::{find_restack_boundary, get_stack_branches_from_merge_base, get_stack_tips};
use anyhow::{Result, anyhow};
use git2::BranchType;
use std::io::IsTerminal;
use std::process::Command;

pub fn restack() -> Result<()> {
    let repo = crate::open_repo()?;

    let path = state_path(&repo);
    if path.exists() {
        return Err(anyhow!(
            "A move or commit operation is already in progress. Use 'gits continue' or 'gits abort'."
        ));
    }
    ensure_no_native_git_operation(&repo)?;

    let head = repo.head()?;
    let head_id = head.peel_to_commit()?.id();
    let current_branch_name = if !repo.head_detached()? {
        head.shorthand().map(|s| s.to_string())
    } else {
        None
    };

    let upstream_name = find_upstream(&repo)?;
    if current_branch_name.as_deref() == Some(&upstream_name) {
        return Err(anyhow!(
            "Branch '{}' is the upstream branch. Switch to a stack branch before running 'restack'.",
            upstream_name
        ));
    }
    let rebase_onto_name = resolve_restack_onto(&repo, &upstream_name)?;
    fetch_restack_remote(&rebase_onto_name)?;

    let upstream_obj = repo.revparse_single(&rebase_onto_name)?;
    let upstream_id = upstream_obj.id();
    let merge_base = repo.merge_base(upstream_id, head_id)?;
    let stack_branches = get_stack_branches_from_merge_base(
        &repo,
        merge_base,
        head_id,
        upstream_id,
        &rebase_onto_name,
    )?;

    if stack_branches.is_empty() {
        println!("No branches found in the current stack.");
        return Ok(());
    }
    let mut tips = get_stack_tips(&repo, &stack_branches)?;
    tips.sort();
    let top_branch = match tips.len() {
        0 => {
            println!("No branches in stack.");
            return Ok(());
        }
        1 => tips[0].clone(),
        _ => {
            if !std::io::stdin().is_terminal() {
                return Err(anyhow!(
                    "Multiple stack tips found. Run 'gits restack' interactively to choose one, or checkout the desired tip branch and rerun."
                ));
            }
            crate::commands::prompt_select("Multiple stack tips found. Select one:", tips)?
        }
    };

    if current_branch_name.as_deref() != Some(top_branch.as_str()) {
        checkout_branch(&top_branch)?;
    }

    let repo = crate::open_repo()?;
    let boundary = find_restack_boundary(&repo, &top_branch, &rebase_onto_name)?;
    let Some(boundary) = boundary else {
        println!(
            "All commits in this stack appear to be integrated into {}.",
            rebase_onto_name
        );
        return Ok(());
    };

    ensure_git_supports_update_refs()?;

    let status = Command::new("git")
        .arg("rebase")
        .arg("--update-refs")
        .arg("--onto")
        .arg(&rebase_onto_name)
        .arg(boundary.old_base.to_string())
        .arg(&top_branch)
        .status()?;

    if !status.success() {
        return Err(anyhow!(
            "git rebase failed. Resolve conflicts and run 'git rebase --continue' or 'git rebase --abort'."
        ));
    }

    Ok(())
}

fn ensure_no_native_git_operation(repo: &git2::Repository) -> Result<()> {
    let git_dir = repo.path();
    let rebase_in_progress =
        git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists();
    let merge_in_progress = git_dir.join("MERGE_HEAD").exists();
    let cherry_pick_in_progress = git_dir.join("CHERRY_PICK_HEAD").exists();

    if rebase_in_progress || merge_in_progress || cherry_pick_in_progress {
        return Err(anyhow!(
            "A native git operation is in progress. Resolve it first with 'git rebase --continue'/'git rebase --abort', 'git merge --abort', or 'git cherry-pick --continue'/'git cherry-pick --abort'. If this came from a gits-managed rebase, use 'gits continue' or 'gits abort'."
        ));
    }

    Ok(())
}

fn resolve_restack_onto(repo: &git2::Repository, upstream_name: &str) -> Result<String> {
    if let Ok(branch) = repo.find_branch(upstream_name, BranchType::Local)
        && let Ok(upstream_branch) = branch.upstream()
        && let Some(upstream_ref) = upstream_branch.name()?
    {
        return Ok(upstream_ref.to_string());
    }

    let remotes = repo.remotes()?;
    let remote_names: Vec<String> = remotes.iter().flatten().map(|s| s.to_string()).collect();
    if let Some((prefix, _)) = upstream_name.split_once('/')
        && remote_names.iter().any(|remote| remote == prefix)
    {
        return Ok(upstream_name.to_string());
    }

    let origin_candidate = format!("origin/{upstream_name}");
    if repo.revparse_single(&origin_candidate).is_ok() {
        return Ok(origin_candidate);
    }

    if remote_names.len() == 1 {
        let only_remote_candidate = format!("{}/{}", remote_names[0], upstream_name);
        if repo.revparse_single(&only_remote_candidate).is_ok() {
            return Ok(only_remote_candidate);
        }
    }

    Ok(upstream_name.to_string())
}

fn fetch_restack_remote(rebase_onto_name: &str) -> Result<()> {
    let Some((remote_name, _)) = rebase_onto_name.split_once('/') else {
        return Ok(());
    };

    let status = Command::new("git").arg("fetch").arg(remote_name).status()?;
    if !status.success() {
        return Err(anyhow!(
            "git fetch failed for remote '{}' while preparing restack.",
            remote_name
        ));
    }

    Ok(())
}

fn checkout_branch(name: &str) -> Result<()> {
    let status = Command::new("git").arg("checkout").arg(name).status()?;
    if !status.success() {
        return Err(anyhow!("git checkout failed for branch '{}'", name));
    }
    Ok(())
}

fn ensure_git_supports_update_refs() -> Result<()> {
    let output = Command::new("git").arg("--version").output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "restack requires Git >= 2.38.0 because it uses '--update-refs', but 'git --version' failed."
        ));
    }

    let version_output = String::from_utf8_lossy(&output.stdout);
    let version = parse_git_semver(&version_output).ok_or_else(|| {
        anyhow!(
            "restack requires Git >= 2.38.0 because it uses '--update-refs', but could not parse `git --version` output: {}",
            version_output.trim()
        )
    })?;

    if version < (2, 38, 0) {
        return Err(anyhow!(
            "restack requires Git >= 2.38.0 because '--update-refs' is used during rebase. Detected Git {}.{}.{}.",
            version.0,
            version.1,
            version.2
        ));
    }

    Ok(())
}

fn parse_git_semver(version_output: &str) -> Option<(u64, u64, u64)> {
    let version_token = version_output
        .split_whitespace()
        .find(|part| part.as_bytes().first().is_some_and(u8::is_ascii_digit))?;

    let numbers = version_token
        .split('.')
        .filter_map(|segment| {
            let digits: String = segment
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect();
            (!digits.is_empty())
                .then_some(digits)
                .and_then(|d| d.parse::<u64>().ok())
        })
        .collect::<Vec<u64>>();

    if numbers.len() < 3 {
        return None;
    }

    Some((numbers[0], numbers[1], numbers[2]))
}

#[cfg(test)]
mod tests {
    use super::parse_git_semver;

    #[test]
    fn parse_git_semver_ignores_non_numeric_dot_segments() {
        let parsed = parse_git_semver("git version 2.44.0.windows.1");
        assert_eq!(parsed, Some((2, 44, 0)));
    }

    #[test]
    fn parse_git_semver_requires_three_components() {
        let parsed = parse_git_semver("git version 2.44");
        assert_eq!(parsed, None);
    }
}
