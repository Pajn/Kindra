use crate::commands::find_upstream;
use crate::stack::get_stack_branches;
use anyhow::{Result, anyhow};
use git2::{BranchType, Repository};
use std::fmt;
use std::process::Command;

pub fn push() -> Result<()> {
    let repo = crate::open_repo()?;

    let upstream_name = find_upstream(&repo)?;
    let current_branch_name = repo.head()?.shorthand().map(|name| name.to_string());
    if current_branch_name.as_deref() == Some(&upstream_name) {
        return push_upstream_branch(&repo, &upstream_name);
    }

    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let upstream_id = upstream_obj.id();
    let head_id = repo.head()?.peel_to_commit()?.id();

    let mut branches_to_push = Vec::new();
    let mut branches_without_upstream = Vec::new();

    let stack_branches = get_stack_branches(&repo, head_id, upstream_id, &upstream_name)?;
    for sb in stack_branches {
        let branch = repo.find_branch(&sb.name, BranchType::Local)?;
        match tracked_push_target(&repo, &branch, sb.name.clone())? {
            Some(target) => {
                branches_to_push.push(target);
            }
            None => {
                branches_without_upstream.push(BranchStatus::without_upstream(sb.name));
            }
        }
    }

    if branches_to_push.is_empty() && branches_without_upstream.is_empty() {
        println!("No branches in stack to push.");
        return Ok(());
    }

    if branches_without_upstream.is_empty() {
        perform_push(branches_to_push)?;
    } else {
        let mut all_branches = branches_to_push.clone();
        all_branches.extend(branches_without_upstream.clone());
        all_branches.sort_by(|a, b| a.name.cmp(&b.name));

        let options = all_branches
            .iter()
            .filter(|b| b.tracked_ref.is_none())
            .cloned()
            .collect::<Vec<_>>();

        if options.is_empty() {
            perform_push(branches_to_push)?;
            return Ok(());
        }

        let selected = crate::commands::prompt_multi_select(
            "Select branches to set upstream and push (Space to toggle, Enter to confirm):",
            options,
        )?;

        if selected.is_empty() && branches_to_push.is_empty() {
            println!("No branches selected to push.");
            return Ok(());
        }

        let remote_name = resolve_remote(&repo)?;
        let mut branches_with_upstream = Vec::new();
        for branch_status in selected {
            branches_with_upstream.push(branch_status.name.clone());
        }

        let mut branches_to_push_with_upstream = Vec::new();
        for name in &branches_with_upstream {
            branches_to_push_with_upstream.push(BranchStatus::with_upstream(
                name.clone(),
                &remote_name,
                name,
            ));
        }

        branches_to_push.extend(branches_to_push_with_upstream);

        perform_push_with_upstream(&repo, &branches_with_upstream, &remote_name)?;

        let pushed_names: Vec<&String> = branches_with_upstream.iter().collect();
        let existing_upstream: Vec<BranchStatus> = branches_to_push
            .iter()
            .filter(|b| b.tracked_ref.is_some() && !pushed_names.contains(&&b.name))
            .cloned()
            .collect();

        if !existing_upstream.is_empty() {
            perform_push(existing_upstream)?;
        }
    }

    Ok(())
}

fn push_upstream_branch(repo: &Repository, upstream_name: &str) -> Result<()> {
    let branch = repo.find_branch(upstream_name, BranchType::Local)?;
    if let Some(target) = tracked_push_target(repo, &branch, upstream_name.to_string())? {
        perform_push(vec![target])
    } else {
        let remote_name = resolve_remote(repo)?;
        perform_push_with_upstream(repo, &[upstream_name.to_string()], remote_name.as_str())
    }
}

#[derive(Clone, Debug)]
struct BranchStatus {
    name: String,
    tracked_remote: Option<String>,
    tracked_ref: Option<String>,
    display_upstream: Option<String>,
}

impl BranchStatus {
    fn with_upstream(name: String, remote: &str, remote_ref: &str) -> Self {
        Self {
            name,
            tracked_remote: Some(remote.to_string()),
            tracked_ref: Some(remote_ref.to_string()),
            display_upstream: Some(format!("{}/{}", remote, remote_ref)),
        }
    }

    fn without_upstream(name: String) -> Self {
        Self {
            name,
            tracked_remote: None,
            tracked_ref: None,
            display_upstream: None,
        }
    }
}

impl fmt::Display for BranchStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.display_upstream {
            Some(u) => write!(f, "{} -> {}", self.name, u),
            None => write!(f, "{} (no upstream)", self.name),
        }
    }
}

fn resolve_remote(repo: &Repository) -> Result<String> {
    let remotes = repo.remotes()?;
    let remote_list: Vec<String> = remotes.iter().flatten().map(|s| s.to_string()).collect();

    if remote_list.contains(&"origin".to_string()) {
        Ok("origin".to_string())
    } else if remote_list.len() == 1 {
        Ok(remote_list[0].clone())
    } else if remote_list.is_empty() {
        Err(anyhow!("No remotes configured."))
    } else {
        Err(anyhow!(
            "'origin' remote not found and multiple remotes exist. Please specify a remote or rename one to 'origin'."
        ))
    }
}

fn perform_push_with_upstream(_repo: &Repository, branches: &[String], remote: &str) -> Result<()> {
    if branches.is_empty() {
        return Ok(());
    }

    println!(
        "Pushing {} branches with upstream to {}...",
        branches.len(),
        remote
    );
    let mut cmd = Command::new("git");
    cmd.arg("push")
        .arg("--atomic")
        .arg("--force-with-lease")
        .arg("-u")
        .arg(remote);

    for branch in branches {
        cmd.arg(format!("{}:{}", branch, branch));
    }

    let status = cmd.status()?;
    if !status.success() {
        return Err(anyhow!("Push failed for remote '{}'", remote));
    }

    Ok(())
}

fn perform_push(branches: Vec<BranchStatus>) -> Result<()> {
    if branches.is_empty() {
        println!("Nothing to push.");
        return Ok(());
    }

    let mut branches_by_remote: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for branch in branches {
        let (Some(remote), Some(remote_ref)) = (branch.tracked_remote, branch.tracked_ref) else {
            continue;
        };

        if let Some((_, refs)) = branches_by_remote
            .iter_mut()
            .find(|(existing_remote, _)| *existing_remote == remote)
        {
            refs.push((branch.name, remote_ref));
        } else {
            branches_by_remote.push((remote, vec![(branch.name, remote_ref)]));
        }
    }

    if branches_by_remote.is_empty() {
        println!("No branches with upstream to push.");
        return Ok(());
    }

    for (remote, refs) in branches_by_remote {
        println!("Pushing {} branches to {}...", refs.len(), remote);
        let mut cmd = Command::new("git");
        cmd.arg("push")
            .arg("--atomic")
            .arg("--force-with-lease")
            .arg(&remote);

        for (local_name, remote_ref) in &refs {
            cmd.arg(format!("{}:{}", local_name, remote_ref));
        }

        let status = cmd.status()?;
        if !status.success() {
            return Err(anyhow!("Push failed for remote '{}'", remote));
        }
    }

    Ok(())
}

fn tracked_push_target(
    repo: &Repository,
    branch: &git2::Branch<'_>,
    local_name: String,
) -> Result<Option<BranchStatus>> {
    let Ok(upstream_branch) = branch.upstream() else {
        return Ok(None);
    };
    let Some(upstream_ref) = upstream_branch.get().name() else {
        return Ok(None);
    };
    let display_upstream = upstream_branch.name()?.map(str::to_string);
    let Some(local_ref) = branch.get().name() else {
        return Ok(None);
    };
    let remote_name = repo
        .branch_upstream_remote(local_ref)
        .ok()
        .and_then(|buf| buf.as_str().map(|value| value.to_string()));
    let Some(remote_name) = remote_name else {
        return Ok(None);
    };
    let remote_ref = upstream_ref
        .strip_prefix(&format!("refs/remotes/{remote_name}/"))
        .or_else(|| upstream_ref.strip_prefix("refs/heads/"))
        .map(str::to_string)
        .unwrap_or_else(|| upstream_ref.to_string());

    Ok(Some(BranchStatus {
        name: local_name,
        tracked_remote: Some(remote_name),
        tracked_ref: Some(remote_ref),
        display_upstream: display_upstream.or_else(|| Some(upstream_ref.to_string())),
    }))
}
