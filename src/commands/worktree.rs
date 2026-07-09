use crate::open_repo;
use crate::worktree::roles;
use crate::worktree::ui::print_list;
use anyhow::Result;
use clap::{Args, Subcommand};
use std::io::IsTerminal;

#[derive(Subcommand, Clone, Debug)]
pub enum WorktreeSubcommand {
    /// List Kindra-managed worktrees and their state
    List,
    /// Ensure the persistent main worktree exists
    Main,
    /// Ensure the reusable review worktree exists and points at a branch
    Review(ReviewArgs),
    /// Create or reuse a temp worktree for a branch, or create a new branch with `-b`
    Temp(TempArgs),
    /// Create (or reuse) a durable worktree for a branch in a sibling directory
    Add(AddArgs),
    /// Print the path for a managed worktree target
    Path(PathArgs),
    /// Change directory to a worktree (requires shell integration; see `kin shell-init`)
    Cd(PathArgs),
    /// Remove a managed worktree target
    Remove(RemoveArgs),
    /// Clean up merged or stale temp worktrees
    Cleanup(CleanupArgs),
}

#[derive(Args, Clone, Debug)]
pub struct ReviewArgs {
    /// Branch to check out in the review worktree. Defaults to the current branch.
    #[arg(add = crate::commands::local_branch_completer())]
    pub branch: Option<String>,

    /// Discard local changes in the review worktree when switching branches
    #[arg(long)]
    pub force: bool,
}

/// The shared `[-b <new-branch>] [<branch-or-start-point>]` argument shape used
/// by both `kin wt temp` and `kin wt add`.
#[derive(Args, Clone, Debug)]
pub struct NewBranchOrTargetArgs {
    /// Create and check out a new branch in the worktree
    #[arg(short = 'b', long = "branch", value_name = "NEW_BRANCH")]
    pub new_branch: Option<String>,

    /// Branch to materialize, or start point when used with `-b`. Defaults to the current branch.
    #[arg(
        value_name = "BRANCH_OR_START_POINT",
        add = crate::commands::local_branch_completer()
    )]
    pub target: Option<String>,
}

#[derive(Args, Clone, Debug)]
pub struct TempArgs {
    #[command(flatten)]
    pub branch: NewBranchOrTargetArgs,
}

#[derive(Args, Clone, Debug)]
pub struct AddArgs {
    #[command(flatten)]
    pub branch: NewBranchOrTargetArgs,

    /// Explicit path for the worktree, overriding the configured default location
    #[arg(long, value_name = "PATH")]
    pub path: Option<std::path::PathBuf>,
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

    /// Force removal when git requires it (for example a dirty worktree)
    #[arg(long)]
    pub force: bool,

    /// Also delete the associated local branch (default when the branch is
    /// merged into trunk for temp or plain worktrees)
    #[arg(long, conflicts_with = "keep_branch")]
    pub with_branch: bool,

    /// Do not delete the associated local branch
    #[arg(long)]
    pub keep_branch: bool,
}

#[derive(Args, Clone, Debug)]
pub struct CleanupArgs {
    /// Force removal when git requires it (for example a dirty worktree)
    #[arg(long)]
    pub force: bool,

    /// Do not delete the merged local branches (by default cleanup deletes
    /// branches for merged temp worktrees)
    #[arg(long)]
    pub keep_branch: bool,
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
            let result = match args.branch.new_branch.as_deref() {
                Some(new_branch) => {
                    roles::ensure_temp_new_branch(&repo, new_branch, args.branch.target.as_deref())?
                }
                None => roles::ensure_temp(&repo, args.branch.target.as_deref())?,
            };
            println!("{}", result.path.display());
        }
        Some(WorktreeSubcommand::Add(args)) => {
            let result = roles::ensure_added(
                &repo,
                args.branch.new_branch.as_deref(),
                args.branch.target.as_deref(),
                args.path.as_deref(),
            )?;
            println!("{}", result.path.display());
        }
        Some(WorktreeSubcommand::Path(args)) => {
            let path = roles::resolve_existing_path(&repo, &args.target)?;
            println!("{}", path.display());
        }
        Some(WorktreeSubcommand::Cd(args)) => {
            let path = roles::resolve_existing_path(&repo, &args.target)?;
            println!("{}", path.display());
            // When stdout is a terminal, the shell wrapper isn't capturing us, so
            // this printed the path but didn't actually change directory. Nudge
            // the user toward enabling integration. Under the wrapper, stdout is a
            // pipe, so this stays quiet.
            if std::io::stdout().is_terminal() {
                eprintln!(
                    "note: 'kin wt cd' only changes directory with shell integration active. \
                     Enable it by adding `eval \"$(kin shell-init <shell>)\"` (bash/zsh) or \
                     `kin shell-init fish | source` to your shell config."
                );
            }
        }
        Some(WorktreeSubcommand::Remove(args)) => {
            let result = roles::remove_target(
                &repo,
                &args.target,
                args.force,
                args.keep_branch,
                args.with_branch,
            )?;
            let mut msg = format!(
                "Removed {} worktree '{}' ({})",
                result.role,
                result.branch,
                result.path.display()
            );
            if result.branch_deleted {
                if let Some(tip) = &result.deleted_branch_tip {
                    let short: String = tip.chars().take(12).collect();
                    msg.push_str(&format!(" and deleted branch (was {})", short));
                } else {
                    msg.push_str(" and deleted branch");
                }
            }
            println!("{}", msg);
        }
        Some(WorktreeSubcommand::Cleanup(args)) => {
            let summary = roles::cleanup_temp_worktrees(&repo, args.force, args.keep_branch)?;
            if summary.candidates == 0 {
                println!("No temp worktrees are eligible for cleanup.");
            } else {
                let branches_deleted = summary.removed.iter().filter(|r| r.branch_deleted).count();
                let mut msg = format!(
                    "Cleanup complete: found {} temp worktree candidate(s), removed {}, skipped {}.",
                    summary.candidates,
                    summary.removed.len(),
                    summary.skipped
                );
                if branches_deleted > 0 {
                    msg.push_str(&format!(" Deleted {} branch(es).", branches_deleted));
                }
                println!("{}", msg);
            }
        }
    }

    Ok(())
}
