use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum OverridesSubcommand {
    /// Apply local overrides, or retry a failed apply after fixing its hook
    Apply,
    /// Restore repository files and disable overrides in this worktree until apply
    Remove,
    /// Show configured local files compared with HEAD, including hidden overrides
    Diff,
}

pub fn overrides(command: &OverridesSubcommand) -> Result<()> {
    let repo = crate::open_repo()?;
    let _lock = crate::state_io::RepoLock::acquire(&repo)?;
    match command {
        OverridesSubcommand::Apply => crate::overrides::apply_current(&repo),
        OverridesSubcommand::Remove => crate::overrides::remove_current(&repo),
        OverridesSubcommand::Diff => crate::overrides::diff_current(&repo),
    }
}
