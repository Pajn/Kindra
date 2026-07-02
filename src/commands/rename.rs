use crate::commands::find_upstream;
use crate::rebase_utils::passively_reconcile_rebase_state;
use crate::worktree::metadata::WorktreeMetadata;
use anyhow::{Context, Result, anyhow};
use clap::Args;
use git2::{BranchType, Repository};
use std::process::Command;

#[derive(Args)]
pub struct RenameArgs {
    /// The new name for the current branch, or the existing branch to rename
    /// when NEW_NAME is also given.
    #[arg(value_name = "BRANCH", add = crate::commands::local_branch_completer())]
    pub first: String,
    /// New name for BRANCH. When present, BRANCH is treated as the existing
    /// branch to rename (mirrors `git branch -m <old> <new>`).
    #[arg(value_name = "NEW_NAME")]
    pub second: Option<String>,
}

pub fn rename(args: &RenameArgs) -> Result<()> {
    let repo = crate::open_repo()?;
    // Mirror `git branch -m [<old>] <new>`: one arg renames the current branch,
    // two args rename the named branch.
    let (old_name, new_name) = match &args.second {
        Some(new) => (args.first.clone(), new.clone()),
        None => (current_branch_name(&repo)?, args.first.clone()),
    };
    rename_branch(&repo, &old_name, &new_name)
}

fn current_branch_name(repo: &Repository) -> Result<String> {
    if repo.head_detached()? {
        return Err(anyhow!(
            "You are not on a branch (detached HEAD). Specify the branch to rename: kin rename <old> <new>"
        ));
    }
    repo.head()?
        .shorthand()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("Could not determine the current branch."))
}

fn rename_branch(repo: &Repository, old_name: &str, new_name: &str) -> Result<()> {
    let _lock = crate::state_io::RepoLock::acquire(repo)?;
    if passively_reconcile_rebase_state(repo)? || crate::commands::run::run_state_exists(repo) {
        return Err(anyhow!(
            "A Kindra operation is already in progress. Use 'kin continue' or 'kin abort'."
        ));
    }

    // Verify the branch exists before the same-name short-circuit, so
    // `kin rename ghost ghost` reports "not found" instead of a false success.
    repo.find_branch(old_name, BranchType::Local)
        .map_err(|_| anyhow!("Branch '{}' not found.", old_name))?;

    if old_name == new_name {
        println!("Branch '{}' already has that name.", old_name);
        return Ok(());
    }

    // The stack is derived relative to the upstream branch, and .git/kindra.toml
    // may pin it by name, so renaming it would leave the base dangling.
    if let Some(upstream) = find_upstream(repo)? {
        if old_name == upstream {
            return Err(anyhow!(
                "Branch '{}' is the upstream branch and cannot be renamed with 'kin rename'.",
                old_name
            ));
        }

        // Guard the other direction too: renaming *to* the base name would create
        // a local branch that `find_upstream` then prefers over a remote-only
        // base (e.g. `origin/main`), silently hijacking the stack base. Compare
        // against the base's short name so an `origin/main` base also blocks `main`.
        let base_short = upstream
            .split_once('/')
            .filter(|(remote, _)| repo.find_remote(remote).is_ok())
            .map(|(_, rest)| rest)
            .unwrap_or(upstream.as_str());
        if new_name == upstream || new_name == base_short {
            return Err(anyhow!(
                "Renaming to '{}' would shadow the stack base '{}'. Choose a different name.",
                new_name,
                upstream
            ));
        }
    }

    // A managed main worktree pins its branch in .git/kindra.toml, which Kindra
    // can't rewrite. Renaming that branch would leave the pin dangling, so the
    // next `kin wt main` would recreate a phantom branch off trunk and the main
    // worktree would get stuck. Refuse it (the default pin == trunk is already
    // covered by the upstream guard above; this catches an explicit non-trunk pin).
    if let Ok(wt_config) = crate::worktree::config::load_worktree_config(repo)
        && wt_config.main.enabled
        && old_name == wt_config.main.branch
    {
        return Err(anyhow!(
            "Branch '{}' is pinned as the managed main worktree branch in .git/kindra.toml. \
             Update `worktrees.main.branch` there instead of renaming.",
            old_name
        ));
    }

    if repo.find_branch(new_name, BranchType::Local).is_ok() {
        return Err(anyhow!("A branch named '{}' already exists.", new_name));
    }

    // Load (and thereby validate) the worktree metadata *before* the irreversible
    // `git branch -m`. An incompatible on-disk version or unreadable file must
    // fail fast here, not after the branch has already been renamed — otherwise
    // the ref would move while the worktree record still pointed at the old name.
    let mut metadata = WorktreeMetadata::load(repo)?;

    // Shell out to `git branch -m` so tracking config (branch.<name>.*) migrates
    // with the branch; git2's Branch::rename leaves those sections behind. Stack
    // parent/child relationships are derived from commit topology, so they follow
    // the rename automatically with no metadata to rewrite.
    let output = Command::new("git")
        .arg("branch")
        .arg("-m")
        .arg(old_name)
        .arg(new_name)
        .output()
        .context("Failed to run 'git branch -m'")?;
    if !output.status.success() {
        return Err(anyhow!(
            "Failed to rename '{}' to '{}': {}",
            old_name,
            new_name,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    // Keep managed-worktree metadata consistent if the renamed branch is tracked.
    // If persisting fails, roll the ref rename back so we never leave the branch
    // renamed with a stale worktree record pointing at the old name.
    if metadata.rename_branch(old_name, new_name)
        && let Err(save_err) = metadata.save(repo)
    {
        let rollback = Command::new("git")
            .args(["branch", "-m", new_name, old_name])
            .output();
        return match rollback {
            Ok(out) if out.status.success() => Err(save_err.context(format!(
                "Failed to update worktree metadata; rolled the rename of '{}' back",
                old_name
            ))),
            _ => Err(save_err.context(format!(
                "Failed to update worktree metadata AND failed to roll back the rename. \
                 Branch is now '{}' but its worktree record still says '{}'; \
                 rename it back manually or fix the metadata.",
                new_name, old_name
            ))),
        };
    }

    println!("Renamed '{}' to '{}'.", old_name, new_name);
    Ok(())
}
