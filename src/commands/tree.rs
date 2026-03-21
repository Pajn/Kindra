use crate::commands::find_upstream;
use crate::gh;
use crate::stack::{StackBranch, find_parent_in_stack, get_stack_branches};
use anyhow::{Context, Result};
use clap::Args;
use crossterm::style::{Color, Stylize};
use git2::BranchType;
use std::collections::HashMap;

/// CLI arguments for the tree command
#[derive(Args, Debug)]
pub struct TreeArgs {
    /// Show commits unique to each branch
    #[arg(short, long)]
    pub commits: bool,

    /// Show remote sync status (ahead/behind)
    #[arg(short, long)]
    pub remote: bool,

    /// Show PR number and state
    #[arg(short, long)]
    pub pr: bool,

    /// Show all status information (commits, remote, PR)
    #[arg(short, long)]
    pub verbose: bool,

    /// Custom upstream branch
    #[arg(long)]
    pub upstream: Option<String>,
}

/// Represents a commit on a branch
#[derive(Clone, Debug)]
pub struct BranchCommit {
    pub hash: String,
    pub subject: String,
}

/// Represents sync status with remote
#[derive(Clone, Debug)]
pub struct RemoteSyncStatus {
    pub ahead: u32,
    pub behind: u32,
}

/// Represents sync status with upstream (needs rebase)
#[derive(Clone, Debug, PartialEq)]
pub enum SyncStatus {
    InSync,
    NeedsRebase,
    Merged,
}

/// Represents PR status
#[derive(Clone, Debug)]
pub struct PrStatus {
    pub number: u64,
    pub is_draft: bool,
}

/// TreeBranch represents a branch in the tree with all its metadata
#[derive(Clone, Debug)]
pub struct TreeBranch {
    pub branch: StackBranch,
    pub parent_name: Option<String>,
    pub children: Vec<String>,
    pub depth: usize,
    pub is_last_child: bool,
    #[allow(dead_code)]
    pub is_current: bool, // May be used for future highlighting
    pub commits: Vec<BranchCommit>,
    pub remote_sync: Option<RemoteSyncStatus>,
    pub pr_status: Option<PrStatus>,
    pub sync_status: Option<SyncStatus>,
}

/// Options for rendering the tree
#[derive(Clone, Debug)]
struct RenderOptions {
    show_commits: bool,
    show_remote: bool,
    show_pr: bool,
}

/// Render the tree with all configured options
pub fn tree(args: &TreeArgs) -> Result<()> {
    let repo = crate::open_repo()?;

    // Find upstream
    let upstream_name = if let Some(ref upstream) = args.upstream {
        upstream.clone()
    } else {
        find_upstream(&repo)?.ok_or_else(|| {
            anyhow::anyhow!(
                "Could not find a base branch (init.defaultBranch, main, master, or trunk)"
            )
        })?
    };

    // Get current branch
    let current_branch_name = if !repo.head_detached()? {
        repo.head()
            .ok()
            .and_then(|h| h.shorthand().map(|s| s.to_string()))
    } else {
        None
    };

    // Get upstream commit ID
    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let upstream_id = upstream_obj.id();

    // Get HEAD commit ID
    // Get stack branches
    let head_id = repo
        .head()
        .context("Failed to get HEAD")?
        .peel_to_commit()?
        .id();
    let stack_branches = get_stack_branches(&repo, head_id, upstream_id, &upstream_name)?;

    if stack_branches.is_empty() {
        println!("{} (empty stack)", upstream_name);
        return Ok(());
    }

    // Build tree structure
    let mut tree = build_tree_structure(&repo, &stack_branches, upstream_id)?;

    // Populate additional info based on flags
    let show_all = args.verbose;
    let show_commits = args.commits || show_all;
    let show_remote = args.remote || show_all;
    let show_pr = args.pr || show_all;

    let options = RenderOptions {
        show_commits,
        show_remote,
        show_pr,
    };

    if show_commits || show_remote || show_pr {
        populate_branch_details(
            &repo,
            &upstream_name,
            &mut tree,
            show_commits,
            show_remote,
            show_pr,
        )?;
    }

    // Render the tree
    render_tree(&mut tree, &upstream_name, &current_branch_name, &options);

    Ok(())
}

/// Build the tree structure from stack branches
fn build_tree_structure(
    repo: &git2::Repository,
    stack_branches: &[StackBranch],
    upstream_id: git2::Oid,
) -> Result<HashMap<String, TreeBranch>> {
    let mut tree: HashMap<String, TreeBranch> = HashMap::new();
    let mut children_map: HashMap<Option<String>, Vec<String>> = HashMap::new();

    // First, find parent for each branch
    for sb in stack_branches {
        let parent_id = find_branch_parent(repo, sb, stack_branches, upstream_id)?;
        let parent_name = if parent_id == git2::Oid::zero() {
            None
        } else {
            stack_branches
                .iter()
                .find(|b| b.id == parent_id)
                .map(|b| b.name.clone())
        };

        tree.insert(
            sb.name.clone(),
            TreeBranch {
                branch: sb.clone(),
                parent_name: parent_name.clone(),
                children: Vec::new(),
                depth: 0,
                is_last_child: false,
                is_current: false,
                commits: Vec::new(),
                remote_sync: None,
                pr_status: None,
                sync_status: None,
            },
        );

        children_map
            .entry(parent_name)
            .or_default()
            .push(sb.name.clone());
    }

    // Calculate depth for each branch (by walking parent chain)
    let mut depth_map: HashMap<String, usize> = HashMap::new();
    for (name, tb) in &tree {
        let mut depth = 0;
        let mut current = tb.parent_name.clone();
        while let Some(parent) = current {
            depth += 1;
            if let Some(parent_tb) = tree.get(&parent) {
                current = parent_tb.parent_name.clone();
            } else {
                break;
            }
        }
        depth_map.insert(name.clone(), depth);
    }

    // Set depth and children for each branch
    for (name, tb) in tree.iter_mut() {
        // Get depth from depth_map
        if let Some(depth) = depth_map.get(name) {
            tb.depth = *depth;
        }

        // Set children
        if let Some(children) = children_map.get(&Some(name.clone())) {
            tb.children = children.clone();
        }
    }

    // Mark last children
    for (parent_name, children) in children_map.iter() {
        if let Some(_parent) = parent_name
            && let Some(child_name) = children.last()
            && let Some(tb) = tree.get_mut(child_name)
        {
            tb.is_last_child = true;
        }
    }

    Ok(tree)
}

/// Find the parent of a branch within the stack
fn find_branch_parent(
    repo: &git2::Repository,
    branch: &StackBranch,
    all_branches: &[StackBranch],
    upstream_id: git2::Oid,
) -> Result<git2::Oid> {
    let parent_id = find_parent_in_stack(repo, &branch.name, all_branches, upstream_id)?;
    if parent_id == upstream_id {
        Ok(git2::Oid::zero())
    } else {
        Ok(parent_id)
    }
}

/// Populate branch details (commits, remote, PR)
fn populate_branch_details(
    repo: &git2::Repository,
    upstream_name: &str,
    tree: &mut HashMap<String, TreeBranch>,
    show_commits: bool,
    show_remote: bool,
    show_pr: bool,
) -> Result<()> {
    for tb in tree.values_mut() {
        let branch_name = &tb.branch.name;

        // Get commits
        if show_commits {
            tb.commits = get_branch_commits_list(repo, branch_name, upstream_name)?;
        }

        // Get remote sync status
        if show_remote {
            tb.remote_sync = get_remote_sync_status(repo, branch_name)?;
        }

        // Get PR status
        if show_pr && let Ok(Some(pr)) = gh::find_open_pr(branch_name) {
            tb.pr_status = Some(PrStatus {
                number: pr.number,
                is_draft: pr.is_draft,
            });
        }

        // Calculate sync status (needs rebase if not in sync with parent)
        tb.sync_status = Some(calculate_sync_status(repo, tb, upstream_name)?);
    }

    Ok(())
}

/// Get list of commits on a branch
fn get_branch_commits_list(
    repo: &git2::Repository,
    branch_name: &str,
    upstream_name: &str,
) -> Result<Vec<BranchCommit>> {
    let branch_id = repo.revparse_single(branch_name)?.peel_to_commit()?.id();
    let upstream_id = repo.revparse_single(upstream_name)?.peel_to_commit()?.id();

    let merge_base = repo.merge_base(upstream_id, branch_id)?;

    let mut revwalk = repo.revwalk()?;
    revwalk.push(branch_id)?;
    revwalk.hide(merge_base)?;
    revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;

    let mut commits = Vec::new();
    for oid in revwalk {
        let commit = repo.find_commit(oid?)?;
        commits.push(BranchCommit {
            hash: commit.id().to_string()[..7].to_string(),
            subject: commit.summary().unwrap_or("").to_string(),
        });
    }

    Ok(commits)
}

/// Get remote sync status (ahead/behind)
fn get_remote_sync_status(
    repo: &git2::Repository,
    branch_name: &str,
) -> Result<Option<RemoteSyncStatus>> {
    let branch = repo.find_branch(branch_name, BranchType::Local)?;

    // Try to get upstream branch
    let upstream = match branch.upstream() {
        Ok(up) => up,
        Err(_) => return Ok(None),
    };

    let local_tip = branch
        .get()
        .target()
        .ok_or_else(|| anyhow::anyhow!("Local branch tip not found"))?;
    let remote_tip = upstream
        .get()
        .target()
        .ok_or_else(|| anyhow::anyhow!("Remote branch tip not found"))?;

    let (ahead, behind) = repo.graph_ahead_behind(local_tip, remote_tip)?;
    let ahead = u32::try_from(ahead).context("Local branch ahead count exceeds u32")?;
    let behind = u32::try_from(behind).context("Local branch behind count exceeds u32")?;

    Ok(Some(RemoteSyncStatus { ahead, behind }))
}

/// Calculate sync status with parent branch
fn calculate_sync_status(
    repo: &git2::Repository,
    tb: &TreeBranch,
    upstream_name: &str,
) -> Result<SyncStatus> {
    // Check if branch is merged into upstream
    let upstream_id = repo.revparse_single(upstream_name)?.id();
    let branch_id = tb.branch.id;

    // If branch is ancestor of upstream, it's merged
    if repo.graph_descendant_of(upstream_id, branch_id)? || upstream_id == branch_id {
        return Ok(SyncStatus::Merged);
    }

    // Check if branch needs rebase by comparing with parent
    if let Some(ref parent_name) = tb.parent_name {
        let parent_id = repo.revparse_single(parent_name)?.id();
        let merge_base = repo.merge_base(branch_id, parent_id)?;

        // If merge_base == parent_id, branch is in sync with parent
        if merge_base == parent_id {
            return Ok(SyncStatus::InSync);
        }
    } else if let Ok(branch) = repo.find_branch(&tb.branch.name, BranchType::Local)
        && let Ok(upstream) = branch.upstream()
        && let Some(branch_upstream_id) = upstream.get().target()
    {
        let merge_base = repo.merge_base(branch_id, branch_upstream_id)?;
        if merge_base == branch_upstream_id {
            return Ok(SyncStatus::InSync);
        }
    }

    Ok(SyncStatus::NeedsRebase)
}

/// Render the tree to stdout
fn render_tree(
    tree: &mut HashMap<String, TreeBranch>,
    upstream_name: &str,
    current_branch_name: &Option<String>,
    options: &RenderOptions,
) {
    // Find root branches (branches whose parent is not in the stack)
    let mut root_names: Vec<String> = Vec::new();
    for tb in tree.values() {
        if tb.parent_name.is_none() || !tree.contains_key(tb.parent_name.as_ref().unwrap()) {
            root_names.push(tb.branch.name.clone());
        }
    }
    root_names.sort();

    // Sort children at each level
    for tb in tree.values_mut() {
        tb.children.sort();
    }

    let upstream_display = if current_branch_name.as_deref() == Some(upstream_name) {
        upstream_name.bold().to_string()
    } else {
        upstream_name.to_string()
    };
    println!("{upstream_display}");

    // Render each root and its descendants under the upstream branch
    for (idx, root_name) in root_names.iter().enumerate() {
        let is_last_root = idx == root_names.len() - 1;
        render_branch_recursive(
            tree,
            root_name,
            "",
            is_last_root,
            current_branch_name,
            options,
        );
    }
}

fn render_branch_recursive(
    tree: &HashMap<String, TreeBranch>,
    branch_name: &str,
    prefix: &str,
    is_last: bool,
    current_branch_name: &Option<String>,
    options: &RenderOptions,
) {
    let tb = match tree.get(branch_name) {
        Some(tb) => tb,
        None => return,
    };

    // Build the connector
    let connector = if is_last { "└─ " } else { "├─ " };

    // Build status indicators
    let mut status_parts: Vec<String> = Vec::new();

    // PR status
    if options.show_pr
        && let Some(ref pr) = tb.pr_status
    {
        let pr_str = if pr.is_draft {
            format!("#{} DRAFT", pr.number)
        } else {
            format!("#{} OPEN", pr.number)
        };
        status_parts.push(
            pr_str
                .with(if pr.is_draft {
                    Color::Yellow
                } else {
                    Color::Green
                })
                .to_string(),
        );
    }

    // Remote status
    if options.show_remote
        && let Some(ref remote) = tb.remote_sync
    {
        let remote_str = format!("[↑{} ↓{}]", remote.ahead, remote.behind);
        let color = if remote.ahead > 0 && remote.behind > 0 {
            Color::Magenta
        } else if remote.ahead > 0 {
            Color::Cyan
        } else if remote.behind > 0 {
            Color::Magenta
        } else {
            Color::Green
        };
        status_parts.push(remote_str.with(color).to_string());
    }

    // Sync status
    if (options.show_pr || options.show_remote)
        && let Some(ref sync) = tb.sync_status
    {
        let sync_str = match sync {
            SyncStatus::InSync => "[In Sync]",
            SyncStatus::NeedsRebase => "[Needs Rebase]",
            SyncStatus::Merged => "[Merged]",
        };
        let color = match sync {
            SyncStatus::InSync => Color::Green,
            SyncStatus::NeedsRebase => Color::Red,
            SyncStatus::Merged => Color::DarkGrey,
        };
        status_parts.push(sync_str.with(color).to_string());
    }

    // Commits
    if options.show_commits {
        let commit_strs: Vec<String> = tb
            .commits
            .iter()
            .map(|c| format!("{} \"{}\"", c.hash, c.subject))
            .collect();
        if !commit_strs.is_empty() {
            status_parts.push(commit_strs.join(", "));
        }
    }

    // Build the line
    let status_str = if status_parts.is_empty() {
        String::new()
    } else {
        format!(" {}", status_parts.join(" "))
    };

    // Determine branch name styling
    let is_current = current_branch_name.as_ref() == Some(&tb.branch.name);
    let branch_display = if is_current {
        tb.branch.name.clone().bold().to_string()
    } else {
        tb.branch.name.clone()
    };

    println!("{}{}{}{}", prefix, connector, branch_display, status_str);

    // Build child prefix
    let child_prefix = if is_last { "    " } else { "│   " };

    // Render children
    for (idx, child_name) in tb.children.iter().enumerate() {
        let is_last_child = idx == tb.children.len() - 1;
        render_branch_recursive(
            tree,
            child_name,
            &format!("{}{}", prefix, child_prefix),
            is_last_child,
            current_branch_name,
            options,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_branch_commit_display() {
        let commit = BranchCommit {
            hash: "abc1234".to_string(),
            subject: "Add login".to_string(),
        };
        assert_eq!(
            format!("{} \"{}\"", commit.hash, commit.subject),
            "abc1234 \"Add login\""
        );
    }

    #[test]
    fn test_remote_sync_status_display() {
        let status = RemoteSyncStatus {
            ahead: 2,
            behind: 0,
        };
        assert_eq!(format!("[↑{} ↓{}]", status.ahead, status.behind), "[↑2 ↓0]");
    }

    #[test]
    fn test_sync_status_variants() {
        assert_eq!(format!("{:?}", SyncStatus::InSync), "InSync");
        assert_eq!(format!("{:?}", SyncStatus::NeedsRebase), "NeedsRebase");
        assert_eq!(format!("{:?}", SyncStatus::Merged), "Merged");
    }
}
