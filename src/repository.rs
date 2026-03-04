use anyhow::{Context, Result};
use git2::Repository;

pub fn open_repo() -> Result<Repository> {
    Repository::discover(".")
        .context("Failed to find or open git repository. Are you in a git repo?")
}
