use anyhow::{Context, Result};
use git2::Repository;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn open_repo() -> Result<Repository> {
    Repository::discover(".")
        .context("Failed to find or open git repository. Are you in a git repo?")
}

/// Variables that name a repository, work tree, index or view of history
/// other than the one discovered from the working directory. `kin` removes
/// them from its own environment at startup ([`forget_inherited_repository`])
/// and from every git child, so each sees the same repository and the same
/// history as the libgit2 handle, which `open_repo` builds without reading
/// any of them. Git exports several to hooks, and wrappers set them too. This
/// is git's own list of repository-local variables
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

/// Remove [`REPOSITORY_ENV`] from `kin`'s own environment, so the whole
/// process works on the repository discovered from its working directory:
/// the absorb engine, which opens the repository in-process and would honour
/// `GIT_DIR`, and every process `kin` starts that is not a git child built by
/// [`git_command`] — `kin run`'s command, hooks, editors and `gh`, which
/// then find the repository from their own working directory as git would
/// there. Git children still get the discovered repository named explicitly.
///
/// # Safety
///
/// Changing the environment is unsound while another thread may read or
/// write it. Call this once at the start of `main`, before any thread is
/// spawned.
pub unsafe fn forget_inherited_repository() {
    for name in REPOSITORY_ENV {
        // SAFETY: the caller guarantees the process is single-threaded.
        unsafe { std::env::remove_var(name) };
    }
}

/// A `git` command that acts on `repo`, the repository libgit2 opened, and
/// its work tree and index, whatever the environment names. Every git child
/// Kindra runs is built here or by [`git_command_in_worktree`].
///
/// The repository is named the way `--git-dir` and `--work-tree` name it
/// (git's options only export these variables): `GIT_DIR` is the worktree's
/// own Git directory, so a linked worktree still reaches the refs in the
/// common directory, and `GIT_WORK_TREE` is its work tree. Without the work
/// tree, git would take the current directory for its top. The arguments are
/// the caller's alone, so the subcommand comes first. The child keeps the
/// current directory, so paths and pathspecs relative to it mean what they
/// mean to git run there; set another one for paths relative to elsewhere.
/// Git's hooks and editors inherit these variables, as they do from git.
///
/// Replace refs are off, for commands that rewrite history too: git
/// substitutes `refs/replace/` objects by default and libgit2 never does, so
/// with them the two would disagree on a commit's parents and so on which
/// branches contain which. A rebase planned with libgit2 would then replay
/// other commits than planned, and bake the replacements into the rewritten
/// history.
pub fn git_command(repo: &Repository) -> Command {
    let mut command = Command::new("git");
    for name in REPOSITORY_ENV {
        command.env_remove(name);
    }
    command.env("GIT_NO_REPLACE_OBJECTS", "1");
    command.env("GIT_DIR", without_trailing_separator(repo.path()));
    if let Some(workdir) = repo.workdir() {
        command.env("GIT_WORK_TREE", without_trailing_separator(workdir));
    }
    command
}

/// libgit2 ends directory paths with a separator. Git compares a linked
/// worktree's Git directory with its own path for it character by character
/// to tell which worktree is the current one, and the separator would make
/// it take the current worktree for another.
fn without_trailing_separator(path: &Path) -> PathBuf {
    path.components().collect()
}

/// A `git` command that acts on the worktree checked out at `path`, which
/// need not be the current one, as [`git_command`] acts on the current one.
/// It runs in `path`.
pub fn git_command_in_worktree(path: &Path) -> Result<Command> {
    let worktree = Repository::open(path)
        .with_context(|| format!("'{}' is not a Git worktree.", path.display()))?;
    let mut command = git_command(&worktree);
    command.current_dir(path);
    Ok(command)
}
