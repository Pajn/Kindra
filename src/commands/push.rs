use crate::commands::find_upstream;
use crate::stack::get_stack_branches;
use anyhow::{Result, anyhow};
use git2::{BranchType, Repository};
use std::fmt;
use std::process::Command;

pub fn push() -> Result<()> {
    let repo = crate::open_repo()?;

    let remote_name = resolve_remote(&repo)?;

    let upstream_name = find_upstream(&repo)?;
    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let upstream_id = upstream_obj.id();
    let head_id = repo.head()?.peel_to_commit()?.id();

    let mut branches_to_push = Vec::new();
    let mut branches_without_upstream = Vec::new();

    let stack_branches = get_stack_branches(&repo, head_id, upstream_id, &upstream_name)?;
    for sb in stack_branches {
        let branch = repo.find_branch(&sb.name, BranchType::Local)?;
        match branch.upstream() {
            Ok(upstream) => {
                let upstream_name = upstream.name()?.unwrap_or("unknown").to_string();
                branches_to_push.push(BranchStatus {
                    name: sb.name,
                    upstream: Some(upstream_name),
                });
            }
            Err(_) => {
                branches_without_upstream.push(BranchStatus {
                    name: sb.name,
                    upstream: None,
                });
            }
        }
    }

    if branches_to_push.is_empty() && branches_without_upstream.is_empty() {
        println!("No branches in stack to push.");
        return Ok(());
    }

    if branches_without_upstream.is_empty() {
        perform_push(&repo, branches_to_push, &remote_name)?;
    } else {
        let mut all_branches = branches_to_push.clone();
        all_branches.extend(branches_without_upstream.clone());
        all_branches.sort_by(|a, b| a.name.cmp(&b.name));

        let options = all_branches
            .iter()
            .filter(|b| b.upstream.is_none())
            .cloned()
            .collect::<Vec<_>>();

        if options.is_empty() {
            perform_push(&repo, branches_to_push, &remote_name)?;
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

        let mut branches_with_upstream = Vec::new();
        for branch_status in selected {
            branches_with_upstream.push(branch_status.name.clone());
        }

        let mut branches_to_push_with_upstream = Vec::new();
        for name in &branches_with_upstream {
            branches_to_push_with_upstream.push(BranchStatus {
                name: (*name).clone(),
                upstream: Some(format!("{}/{}", remote_name, name)),
            });
        }

        branches_to_push.extend(branches_to_push_with_upstream);

        perform_push_with_upstream(&repo, &branches_with_upstream, &remote_name)?;

        let pushed_names: Vec<&String> = branches_with_upstream.iter().collect();
        let existing_upstream: Vec<BranchStatus> = branches_to_push
            .iter()
            .filter(|b| b.upstream.is_some() && !pushed_names.contains(&&b.name))
            .cloned()
            .collect();

        if !existing_upstream.is_empty() {
            perform_push(&repo, existing_upstream, &remote_name)?;
        }
    }

    Ok(())
}

#[derive(Clone, Debug)]
struct BranchStatus {
    name: String,
    upstream: Option<String>,
}

impl fmt::Display for BranchStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.upstream {
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

fn perform_push(_repo: &Repository, branches: Vec<BranchStatus>, remote: &str) -> Result<()> {
    if branches.is_empty() {
        println!("Nothing to push.");
        return Ok(());
    }

    let refs: Vec<String> = branches
        .into_iter()
        .filter_map(|b| b.upstream)
        .filter_map(|upstream| {
            let parts: Vec<&str> = upstream.splitn(2, '/').collect();
            if parts.len() == 2 {
                Some(parts[1].to_string())
            } else {
                None
            }
        })
        .collect();

    if refs.is_empty() {
        println!("No branches with upstream to push.");
        return Ok(());
    }

    println!("Pushing {} branches to {}...", refs.len(), remote);
    let mut cmd = Command::new("git");
    cmd.arg("push")
        .arg("--atomic")
        .arg("--force-with-lease")
        .arg(remote);

    for r in &refs {
        cmd.arg(format!("{}:{}", r, r));
    }

    let status = cmd.status()?;
    if !status.success() {
        return Err(anyhow!("Push failed for remote '{}'", remote));
    }

    Ok(())
}
