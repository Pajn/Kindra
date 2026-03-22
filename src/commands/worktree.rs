use crate::open_repo;
use crate::worktree::roles;
use crate::worktree::ui::print_list;
use anyhow::Result;
use clap::{Args, Subcommand};

#[derive(Subcommand, Clone, Debug)]
pub enum WorktreeSubcommand {
    /// List Kindra-managed worktrees and their state
    List,
    /// Ensure the persistent main worktree exists
    Main,
    /// Ensure the reusable review worktree exists and points at a branch
    Review(ReviewArgs),
    /// Create or reuse a temp worktree for a branch
    Temp(TempArgs),
    /// Print the path for a managed worktree target
    Path(PathArgs),
    /// Remove a managed worktree target
    Remove(RemoveArgs),
    /// Clean up merged or stale temp worktrees
    Cleanup(CleanupArgs),
}

#[derive(Args, Clone, Debug)]
pub struct ReviewArgs {
    /// Branch to check out in the review worktree. Defaults to the current branch.
    pub branch: Option<String>,

    /// Discard local changes in the review worktree when switching branches
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Clone, Debug)]
pub struct TempArgs {
    /// Branch to materialize in a temp worktree. Defaults to the current branch.
    pub branch: Option<String>,
}

#[derive(Args, Clone, Debug)]
pub struct PathArgs {
    /// `main`, `review`, or a temp worktree branch name (`branch:<name>` disambiguates)
    pub target: String,
}

#[derive(Args, Clone, Debug)]
pub struct RemoveArgs {
    /// `main`, `review`, or a temp worktree branch name (`branch:<name>` disambiguates)
    pub target: String,

    /// Skip the confirmation prompt
    #[arg(long)]
    pub yes: bool,

    /// Force removal when git requires it (for example a dirty worktree)
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Clone, Debug)]
pub struct CleanupArgs {
    /// Skip the confirmation prompt
    #[arg(long)]
    pub yes: bool,

    /// Force removal when git requires it (for example a dirty worktree)
    #[arg(long)]
    pub force: bool,
}

pub fn worktree(subcommand: &Option<WorktreeSubcommand>) -> Result<()> {
    let repo = open_repo()?;

    match subcommand {
        None | Some(WorktreeSubcommand::List) => {
            let rows = roles::list_managed_worktrees(&repo)?;
            print_list(&rows);
        }
        Some(WorktreeSubcommand::Main) => {
            let result = roles::ensure_main(&repo)?;
            println!("{}", result.path.display());
        }
        Some(WorktreeSubcommand::Review(args)) => {
            let result = roles::ensure_review(&repo, args.branch.as_deref(), args.force)?;
            println!("{}", result.path.display());
        }
        Some(WorktreeSubcommand::Temp(args)) => {
            let result = roles::ensure_temp(&repo, args.branch.as_deref())?;
            println!("{}", result.path.display());
        }
        Some(WorktreeSubcommand::Path(args)) => {
            let path = roles::resolve_existing_path(&repo, &args.target)?;
            println!("{}", path.display());
        }
        Some(WorktreeSubcommand::Remove(args)) => {
            let result = roles::remove_target(&repo, &args.target, args.yes, args.force)?;
            if result.metadata_only {
                println!(
                    "Removed stale metadata for {} worktree '{}' ({})",
                    result.role,
                    result.branch,
                    result.path.display()
                );
            } else {
                println!(
                    "Removed {} worktree '{}' ({})",
                    result.role,
                    result.branch,
                    result.path.display()
                );
            }
        }
        Some(WorktreeSubcommand::Cleanup(args)) => {
            let summary = roles::cleanup_temp_worktrees(&repo, args.yes, args.force)?;
            if summary.candidates == 0 {
                println!("No temp worktrees are eligible for cleanup.");
            } else {
                println!(
                    "Cleanup complete: found {} temp worktree candidate(s), removed {}, skipped {}.",
                    summary.candidates,
                    summary.removed.len(),
                    summary.skipped
                );
            }
        }
    }

    Ok(())
}
