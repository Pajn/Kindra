mod commands;
mod stack;

use crate::commands::checkout::checkout;
use crate::commands::move_cmd::{MoveArgs, move_cmd};
use crate::commands::push::push;
use crate::commands::split::split;
use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};

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
    /// Interactive branch checkout
    #[command(alias = "co")]
    Checkout {
        #[command(subcommand)]
        subcommand: Option<CheckoutSubcommand>,
    },
    /// Move current branch stack onto another branch
    Move(MoveArgs),
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

#[derive(Subcommand)]
pub enum CheckoutSubcommand {
    /// Checkout the branch above the current one
    Up,
    /// Checkout the branch below the current one
    Down,
    /// Checkout the top branch in the stack
    Top,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Split => split()?,
        Commands::Push => push()?,
        Commands::Checkout { subcommand } => checkout(subcommand)?,
        Commands::Move(args) => move_cmd(args)?,
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
