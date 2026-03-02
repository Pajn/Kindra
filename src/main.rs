mod commands;
mod editor;
mod gh;
mod rebase_utils;
pub mod stack;

use crate::commands::abort_cmd::abort_cmd;
use crate::commands::checkout::checkout;
use crate::commands::commit::commit;
use crate::commands::continue_cmd::continue_cmd;
use crate::commands::move_cmd::{MoveArgs, move_cmd};
use crate::commands::pr::pr;
use crate::commands::push::push;
use crate::commands::split::split;
use crate::commands::status_cmd::status_cmd;
use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use commands::CheckoutSubcommand;

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
    /// Pushes all branches with upstreams (atomic, force-with-lease)
    Push,
    /// Create or update pull requests for all branches with upstreams
    Pr,
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
    /// Commits and rebases dependent branches
    Commit {
        /// Arguments to pass to git commit
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Continue an in-progress move or commit operation
    Continue,
    /// Abort an in-progress move or commit operation
    Abort,
    /// Show the status of an in-progress move or commit operation
    Status,
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

fn main() -> Result<()> {
    let _restorer = TerminalRestorer;
    let cli = Cli::parse();

    match &cli.command {
        Commands::Split => split()?,
        Commands::Push => push()?,
        Commands::Pr => pr()?,
        Commands::Commit { args } => commit(args)?,
        Commands::Checkout { subcommand, all } => checkout(subcommand, *all)?,
        Commands::Move(args) => move_cmd(args)?,
        Commands::Continue => continue_cmd()?,
        Commands::Abort => abort_cmd()?,
        Commands::Status => status_cmd()?,
        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            match shell {
                ShellChoice::Bash => {
                    generate(Shell::Bash, &mut cmd, "gits", &mut std::io::stdout())
                }
                ShellChoice::Zsh => generate(Shell::Zsh, &mut cmd, "gits", &mut std::io::stdout()),
                ShellChoice::Fish => {
                    generate(Shell::Fish, &mut cmd, "gits", &mut std::io::stdout())
                }
                ShellChoice::PowerShell => {
                    generate(Shell::PowerShell, &mut cmd, "gits", &mut std::io::stdout())
                }
                ShellChoice::Elvish => {
                    generate(Shell::Elvish, &mut cmd, "gits", &mut std::io::stdout())
                }
                ShellChoice::Nu => generate(
                    clap_complete_nushell::Nushell,
                    &mut cmd,
                    "gits",
                    &mut std::io::stdout(),
                ),
            }
        }
    }

    Ok(())
}
