mod commands;
mod editor;
mod gh;
mod interaction;
mod oplog;
mod rebase_utils;
mod repository;
mod runtime;
mod stack;
mod state_io;
mod worktree;

use crate::commands::abort_cmd::abort_cmd;
use crate::commands::checkout::checkout;
use crate::commands::commit::commit;
use crate::commands::continue_cmd::continue_cmd;
use crate::commands::move_cmd::{MoveArgs, move_cmd};
use crate::commands::pr::{PrSubcommand, pr};
use crate::commands::push::push;
use crate::commands::rename::{RenameArgs, rename};
use crate::commands::reorder::{ReorderArgs, reorder};
use crate::commands::restack::{RestackArgs, restack};
use crate::commands::run::{RunArgs, run};
use crate::commands::split::{SplitArgs, split};
use crate::commands::status_cmd::status_cmd;
use crate::commands::sync::{SyncArgs, sync};
use crate::commands::tree::{TreeArgs, tree};
use crate::commands::worktree::{WorktreeSubcommand, worktree};
pub use crate::repository::open_repo;
use anyhow::Result;
use clap::{Arg, ArgAction, Command, CommandFactory, Parser, Subcommand};
use clap_complete::generate;
use commands::CheckoutSubcommand;
use std::process::Command as ProcessCommand;

#[derive(Parser)]
#[command(name = "kin")]
#[command(about = "A wrapper around git to aid certain workflows", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Never prompt; fail loudly if a prompt needs a real answer (for agents/CI).
    /// To force interactive prompts without a TTY, set KIN_INTERACTIVE=1.
    #[arg(long = "no-interactive", global = true, conflicts_with = "yes")]
    no_interactive: bool,

    /// Never prompt; auto-answer "yes" to every confirmation. Note this can
    /// choose the non-default action, including for destructive prompts.
    #[arg(long, global = true)]
    yes: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Opens $EDITOR to manage branches in a stack of commits
    Split(SplitArgs),
    /// Pushes all branches with upstreams (atomic, force-with-lease)
    Push,
    /// Create/update PRs for stack branches, or open existing PRs in the browser
    Pr {
        #[command(subcommand)]
        subcommand: Option<PrSubcommand>,
        /// Skip the automatic flatten/push preflight before creating PRs
        #[arg(long)]
        no_push: bool,
        /// Include PRs authored by other GitHub users in stack updates
        #[arg(long)]
        all: bool,
        /// Set labels on all created PRs (can be specified multiple times)
        #[arg(long)]
        label: Vec<String>,
        /// Title for created PRs (required non-interactively when a branch has
        /// multiple commits); applies to every new PR in the stack
        #[arg(long)]
        title: Option<String>,
        /// Build PR bodies from the branch commits instead of prompting/template
        #[arg(long)]
        body_from_commits: bool,
        /// Create new PRs as drafts
        #[arg(long, overrides_with = "no_draft")]
        draft: bool,
        /// Create new PRs as ready for review (overrides --draft)
        #[arg(long = "no-draft", overrides_with = "draft")]
        no_draft: bool,
        /// Request a reviewer on created PRs (can be specified multiple times)
        #[arg(long = "reviewer")]
        reviewer: Vec<String>,
    },
    /// Interactive branch checkout
    #[command(alias = "co")]
    Checkout {
        #[command(subcommand)]
        subcommand: Option<CheckoutSubcommand>,
        /// List all local branches instead of just the stack
        #[arg(long)]
        all: bool,
    },
    /// Move current branch stack onto another branch
    Move(MoveArgs),
    /// Rename a branch (defaults to the current branch), preserving the stack
    Rename(RenameArgs),
    /// Reorder the current stack by editing branch parents
    Reorder(ReorderArgs),
    /// Rebase the current stack onto the upstream branch in one pass
    Sync(SyncArgs),
    /// Repair stack dependencies by rebasing detached children onto the current branch
    Restack(RestackArgs),
    /// Run a command on each branch in the stack
    Run(RunArgs),
    /// Commits and rebases dependent branches
    Commit {
        /// Arguments to pass to git commit. Supports --on <branch> and --force.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Continue an in-progress Kindra operation
    Continue,
    /// Abort an in-progress Kindra operation
    Abort,
    /// Show the status of an in-progress Kindra operation
    Status,
    /// Undo the most recent stack-rewriting operation (sync, reorder, move, restack, split)
    Undo {
        /// Move branches even if they changed since the operation or the working tree is dirty
        #[arg(long)]
        force: bool,
    },
    /// Reapply the most recently undone operation
    Redo {
        /// Move branches even if they changed since the undo or the working tree is dirty
        #[arg(long)]
        force: bool,
    },
    /// List recent stack-rewriting operations and the current undo position
    #[command(alias = "oplog")]
    Reflog,
    /// Visualize the stack tree
    #[command(alias = "t")]
    Tree(TreeArgs),
    /// Manage Kindra worktrees
    #[command(alias = "wt")]
    Worktree {
        #[command(subcommand)]
        subcommand: Option<WorktreeSubcommand>,
    },
    /// Generate shell completions
    Completions {
        /// The shell to generate completions for
        shell: ShellChoice,
    },
}

#[derive(clap::ValueEnum, Clone, Copy)]
enum ShellChoice {
    Bash,
    Zsh,
    Fish,
    #[value(name = "powershell", alias = "power-shell")]
    PowerShell,
    Elvish,
    Nu,
}

struct TerminalRestorer;

impl Drop for TerminalRestorer {
    fn drop(&mut self) {
        // crossterm::terminal::disable_raw_mode is safe to call unconditionally
        // as it will return an error if raw mode wasn't enabled, which we ignore here.
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

fn main() {
    std::process::exit(real_main());
}

/// Distinct exit code for "a prompt needed input that was unavailable in
/// non-interactive mode", so scripts can tell it apart from a real failure (1).
const EXIT_INPUT_REQUIRED: i32 = 3;

fn real_main() -> i32 {
    runtime::install_quiet_output_panic_hook();
    let _restorer = TerminalRestorer;

    match dispatch() {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("Error: {err:#}");
            if err.downcast_ref::<interaction::InputRequired>().is_some() {
                EXIT_INPUT_REQUIRED
            } else {
                1
            }
        }
    }
}

fn dispatch() -> Result<()> {
    // SAFETY: We are at the very beginning of the program, before any threads
    // are spawned.
    unsafe {
        runtime::configure_runtime_tuning()?;
    }

    clap_complete::CompleteEnv::with_factory(completion_command)
        .bin("kin")
        .complete();

    let cli = Cli::parse();

    // Clap's `trailing_var_arg` on the `commit` subcommand captures global flags
    // placed after `commit` (e.g. `kin commit --amend --yes`) into its
    // pass-through args instead of binding them to `Cli`. Recover them here so
    // the interaction mode still honours them; `parse_commit_args` strips them
    // from what is forwarded to `git commit`.
    let (no_interactive, yes) = match &cli.command {
        Commands::Commit { args } => {
            crate::commands::commit::recover_interaction_flags(args, cli.no_interactive, cli.yes)
        }
        _ => (cli.no_interactive, cli.yes),
    };
    interaction::init(interaction::resolve(no_interactive, yes));

    match &cli.command {
        Commands::Split(args) => split(args)?,
        Commands::Push => push()?,
        Commands::Pr {
            subcommand,
            no_push,
            all,
            label,
            title,
            body_from_commits,
            draft,
            no_draft,
            reviewer,
        } => {
            let draft = if *draft {
                Some(true)
            } else if *no_draft {
                Some(false)
            } else {
                None
            };
            pr(
                subcommand,
                *no_push,
                *all,
                crate::commands::pr::PrCreateOptions {
                    title: title.clone(),
                    body_from_commits: *body_from_commits,
                    draft,
                    reviewers: reviewer.clone(),
                    labels: label.clone(),
                },
            )?
        }
        Commands::Checkout { subcommand, all } => checkout(subcommand, *all)?,
        Commands::Move(args) => move_cmd(args)?,
        Commands::Rename(args) => rename(args)?,
        Commands::Reorder(args) => reorder(args)?,
        Commands::Sync(args) => sync(args)?,
        Commands::Restack(args) => restack(args)?,
        Commands::Run(args) => run(args)?,
        Commands::Commit { args } => commit(args)?,
        Commands::Continue => continue_cmd()?,
        Commands::Abort => abort_cmd()?,
        Commands::Status => status_cmd()?,
        Commands::Undo { force } => oplog::undo(*force)?,
        Commands::Redo { force } => oplog::redo(*force)?,
        Commands::Reflog => oplog::reflog()?,
        Commands::Tree(args) => tree(args)?,
        Commands::Worktree { subcommand } => worktree(subcommand)?,
        Commands::Completions { shell } => match shell {
            ShellChoice::Bash => print_dynamic_completion_script("bash")?,
            ShellChoice::Zsh => print_dynamic_completion_script("zsh")?,
            ShellChoice::Fish => print_dynamic_completion_script("fish")?,
            ShellChoice::PowerShell => print_dynamic_completion_script("powershell")?,
            ShellChoice::Elvish => print_dynamic_completion_script("elvish")?,
            ShellChoice::Nu => generate(
                clap_complete_nushell::Nushell,
                &mut completion_command(),
                "kin",
                &mut std::io::stdout(),
            ),
        },
    }

    Ok(())
}

fn completion_command() -> Command {
    let mut cmd = Cli::command();
    if let Some(commit_cmd) = cmd.find_subcommand_mut("commit") {
        let updated = std::mem::replace(commit_cmd, Command::new("commit")).arg(
            Arg::new("on")
                .long("on")
                .value_name("branch")
                .num_args(0..=1)
                .action(ArgAction::Set)
                .help("Commit onto another branch instead of the current one")
                .add(crate::commands::local_branch_completer()),
        );
        *commit_cmd = updated;
    }
    cmd
}

fn print_dynamic_completion_script(shell: &str) -> Result<()> {
    let output = ProcessCommand::new(std::env::current_exe()?)
        .env("COMPLETE", shell)
        .output()?;
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "failed to generate {shell} completions: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}
