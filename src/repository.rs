use anyhow::{Context, Result};
use git2::Repository;
use std::process::Command;

pub fn open_repo() -> Result<Repository> {
    Repository::discover(".")
        .context("Failed to find or open git repository. Are you in a git repo?")
}

/// Variables that make git read a different repository, object store, index
/// or ref namespace than the one it would discover. Git exports several of
/// them to hooks, and wrappers set them too; `open_repo` ignores them all.
const REPOSITORY_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
];

/// A `git` command that reads `repo`, the repository libgit2 opened, whatever
/// the environment names. `--git-dir` is the worktree's own Git directory, so
/// a linked worktree still reaches the refs in the common directory.
pub fn git_command(repo: &Repository) -> Command {
    let mut command = Command::new("git");
    for name in REPOSITORY_ENV {
        command.env_remove(name);
    }
    command.arg("--git-dir").arg(repo.path());
    command
}
