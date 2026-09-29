use anyhow::{Context, Result};
use git2::Repository;
use std::process::Command;

pub fn open_repo() -> Result<Repository> {
    Repository::discover(".")
        .context("Failed to find or open git repository. Are you in a git repo?")
}

/// Variables removed from a git child so it sees the same repository and the
/// same history as the libgit2 handle, which `open_repo` builds without
/// reading any of them. Git exports several to hooks, and wrappers set them
/// too. This is git's own list of repository-local variables
/// (`git rev-parse --local-env-vars`) plus `GIT_NAMESPACE`, which narrows the
/// refs git lists.
///
/// Git's config variables (`GIT_CONFIG`, `GIT_CONFIG_PARAMETERS`,
/// `GIT_CONFIG_COUNT` and its keys) stay: they carry the user's `git -c`
/// settings, such as `safe.directory`, which the child may need to open the
/// repository at all, and they name no other repository or history.
const REPOSITORY_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_GRAFT_FILE",
    "GIT_SHALLOW_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_NAMESPACE",
];

/// A `git` command that reads `repo`, the repository libgit2 opened, whatever
/// the environment names. `--git-dir` is the worktree's own Git directory, so
/// a linked worktree still reaches the refs in the common directory.
///
/// Replace refs are off: git substitutes `refs/replace/` objects by default
/// and libgit2 never does, so with them the two would disagree on a commit's
/// parents and so on which branches contain which.
pub fn git_command(repo: &Repository) -> Command {
    let mut command = Command::new("git");
    for name in REPOSITORY_ENV {
        command.env_remove(name);
    }
    command.env("GIT_NO_REPLACE_OBJECTS", "1");
    command.arg("--git-dir").arg(repo.path());
    command
}
