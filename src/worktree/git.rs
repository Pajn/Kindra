use crate::worktree::path_resolver::normalize_path;
use anyhow::{Context, Result, anyhow};
use git2::{BranchType, ErrorCode, Oid, Repository};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveWorktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub detached: bool,
}

impl LiveWorktree {
    pub fn normalized_path(&self) -> PathBuf {
        normalize_path(&self.path)
    }
}

pub fn list_live_worktrees(repo: &Repository) -> Result<Vec<LiveWorktree>> {
    let cwd = repo_root(repo)?;
    let output = Command::new("git")
        .current_dir(cwd)
        .arg("worktree")
        .arg("list")
        .arg("--porcelain")
        .output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "Failed to list git worktrees: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut worktrees = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;
    let mut detached = false;

    let flush = |worktrees: &mut Vec<LiveWorktree>,
                 current_path: &mut Option<PathBuf>,
                 current_branch: &mut Option<String>,
                 detached: &mut bool| {
        if let Some(path) = current_path.take() {
            worktrees.push(LiveWorktree {
                path: normalize_path(path),
                branch: current_branch.take(),
                detached: *detached,
            });
        }
        *detached = false;
    };

    for line in stdout.lines() {
        if line.is_empty() {
            flush(
                &mut worktrees,
                &mut current_path,
                &mut current_branch,
                &mut detached,
            );
            continue;
        }

        if let Some(path) = line.strip_prefix("worktree ") {
            if current_path.is_some() {
                flush(
                    &mut worktrees,
                    &mut current_path,
                    &mut current_branch,
                    &mut detached,
                );
            }
            current_path = Some(PathBuf::from(path.trim()));
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            current_branch = Some(
                branch_ref
                    .trim()
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch_ref.trim())
                    .to_string(),
            );
        } else if line == "detached" {
            detached = true;
        }
    }

    flush(
        &mut worktrees,
        &mut current_path,
        &mut current_branch,
        &mut detached,
    );
    Ok(worktrees)
}

pub fn live_worktree_map(worktrees: &[LiveWorktree]) -> HashMap<PathBuf, LiveWorktree> {
    worktrees
        .iter()
        .cloned()
        .map(|worktree| (worktree.normalized_path(), worktree))
        .collect()
}

pub fn add_worktree(repo: &Repository, path: &Path, branch: &str) -> Result<()> {
    ensure_local_branch_exists(repo, branch)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Kindra's managed worktrees intentionally allow role-based reuse even when the same branch
    // is already checked out elsewhere, so we always pass `git worktree add --force` here.
    // If worktree creation semantics become user-configurable later, this flag should follow that
    // explicit policy instead of being unconditional.
    let status = Command::new("git")
        .current_dir(repo_root(repo)?)
        .arg("worktree")
        .arg("add")
        .arg("--force")
        .arg(path)
        .arg(branch)
        .output()?;
    if !status.status.success() {
        return Err(anyhow!(
            "Failed to create worktree at '{}' for branch '{}': {}",
            path.display(),
            branch,
            String::from_utf8_lossy(&status.stderr).trim()
        ));
    }
    Ok(())
}

pub fn checkout_worktree_branch(
    path: &Path,
    branch: &str,
    discard_local_changes: bool,
) -> Result<()> {
    checkout_worktree_reference(path, branch, discard_local_changes, false)
}

pub fn checkout_worktree_detached(
    path: &Path,
    target: &str,
    discard_local_changes: bool,
) -> Result<()> {
    checkout_worktree_reference(path, target, discard_local_changes, true)
}

fn checkout_worktree_reference(
    path: &Path,
    target: &str,
    discard_local_changes: bool,
    detached: bool,
) -> Result<()> {
    let mut command = Command::new("git");
    command.current_dir(path).arg("checkout");
    if discard_local_changes {
        command.arg("--force");
    }
    if detached {
        command.arg("--detach");
    }
    command.arg("--ignore-other-worktrees").arg(target);

    let output = command.output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "Failed to switch worktree '{}' to '{}': {}",
            path.display(),
            target,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

pub fn remove_worktree(repo: &Repository, path: &Path, force: bool) -> Result<()> {
    let mut command = Command::new("git");
    command
        .current_dir(repo_root(repo)?)
        .arg("worktree")
        .arg("remove");
    if force {
        command.arg("--force");
    }
    command.arg(path);

    let output = command.output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "Failed to remove worktree '{}': {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

pub fn is_worktree_dirty(path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .current_dir(path)
        .arg("status")
        .arg("--porcelain")
        .arg("--untracked-files=normal")
        .output()
        .with_context(|| format!("Failed to inspect worktree '{}'.", path.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "Failed to inspect worktree '{}': {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(!output.stdout.is_empty())
}

pub fn repo_root(repo: &Repository) -> Result<&Path> {
    repo.workdir()
        .ok_or_else(|| anyhow!("Kindra worktree management requires a non-bare repository."))
}

pub fn current_branch(repo: &Repository) -> Result<Option<String>> {
    if repo.head_detached()? {
        return Ok(None);
    }
    Ok(repo.head()?.shorthand().map(str::to_string))
}

pub fn current_head_oid(path: &Path) -> Result<String> {
    let output = Command::new("git")
        .current_dir(path)
        .args(["rev-parse", "HEAD"])
        .output()
        .with_context(|| format!("Failed to resolve HEAD for worktree '{}'.", path.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "Failed to resolve HEAD for worktree '{}': {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn ensure_local_branch_exists(repo: &Repository, branch: &str) -> Result<()> {
    match repo.find_branch(branch, BranchType::Local) {
        Ok(_) => Ok(()),
        Err(err) => {
            if let Some(remote) = remote_for_branch(repo, branch)? {
                // Auto-create local branch tracking the remote
                let start_point = format!("{}/{}", remote, branch);
                let mut cmd = Command::new("git");
                cmd.current_dir(repo_root(repo)?)
                    .arg("branch")
                    .arg("--track")
                    .arg(branch)
                    .arg(&start_point);
                let output = cmd.output()?;
                if !output.status.success() {
                    return Err(anyhow!(
                        "Failed to create local branch '{}' tracking '{}/{}': {}",
                        branch,
                        remote,
                        branch,
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
                }
                return Ok(());
            }

            Err(err.into())
        }
    }
}

pub fn ensure_local_branch_exists_from_start_point(
    repo: &Repository,
    branch: &str,
    start_point: &str,
) -> Result<()> {
    if repo.find_branch(branch, BranchType::Local).is_ok() {
        return Ok(());
    }

    let mut command = Command::new("git");
    command.current_dir(repo_root(repo)?).arg("branch");
    if repo.find_branch(start_point, BranchType::Remote).is_ok() {
        command.arg("--track");
    }
    command.arg(branch).arg(start_point);

    let output = command.output()?;
    if output.status.success() {
        return Ok(());
    }

    ensure_local_branch_exists(repo, branch).map_err(|original_err| {
        anyhow!(
            "Failed to create local branch '{}' from '{}': {}\n{}",
            branch,
            start_point,
            String::from_utf8_lossy(&output.stderr).trim(),
            original_err
        )
    })
}

pub fn create_local_branch_from_start_point_strict(
    repo: &Repository,
    branch: &str,
    start_point: &str,
) -> Result<bool> {
    match repo.find_branch(branch, BranchType::Local) {
        Ok(_) => return Ok(false),
        Err(err) if err.code() == ErrorCode::NotFound => {}
        Err(err) => return Err(err.into()),
    }

    let mut command = Command::new("git");
    command.current_dir(repo_root(repo)?).arg("branch");
    if repo.find_branch(start_point, BranchType::Remote).is_ok() {
        command.arg("--track");
    }
    command.arg(branch).arg(start_point);

    let output = command.output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "Failed to create local branch '{}' from '{}': {}",
            branch,
            start_point,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(true)
}

pub fn delete_local_branch_if_tip_matches(
    repo: &Repository,
    branch: &str,
    expected_tip: Oid,
) -> Result<bool> {
    match repo.find_branch(branch, BranchType::Local) {
        Ok(current_branch) => match current_branch.get().target() {
            Some(current_tip) if current_tip == expected_tip => {}
            Some(_) | None => return Ok(false),
        },
        Err(err) if err.code() == ErrorCode::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    }

    let ref_name = format!("refs/heads/{branch}");
    let expected_tip_text = expected_tip.to_string();
    let output = Command::new("git")
        .current_dir(repo_root(repo)?)
        .args(["update-ref", "-d", &ref_name, &expected_tip_text])
        .output()?;
    if output.status.success() {
        return Ok(true);
    }

    match repo.find_branch(branch, BranchType::Local) {
        Ok(current_branch) => match current_branch.get().target() {
            Some(current_tip) if current_tip != expected_tip => Ok(false),
            None => Ok(false),
            Some(_) => Err(anyhow!(
                "Failed to delete local branch '{}' at {}: {}",
                branch,
                expected_tip,
                String::from_utf8_lossy(&output.stderr).trim()
            )),
        },
        Err(err) if err.code() == ErrorCode::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn remote_for_branch(repo: &Repository, branch: &str) -> Result<Option<String>> {
    let branches = repo.branches(Some(BranchType::Remote))?;
    for branch_result in branches {
        let (remote_branch, _) = branch_result?;
        let Some(name) = remote_branch.name()? else {
            continue;
        };
        let Some((remote, remote_branch_name)) = name.split_once('/') else {
            continue;
        };
        if remote_branch_name == branch {
            return Ok(Some(remote.to_string()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::{ensure_local_branch_exists, list_live_worktrees, live_worktree_map};
    use git2::BranchType;
    use tempfile::TempDir;

    #[test]
    fn lists_worktrees() {
        let dir = TempDir::new().unwrap();
        let init_status = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["init", "--initial-branch=main"])
            .status()
            .unwrap();
        assert!(init_status.success(), "git init failed");
        let repo = git2::Repository::open(dir.path()).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Test").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let add_status = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["add", "a.txt"])
            .status()
            .unwrap();
        assert!(add_status.success(), "git add failed");
        let commit_status = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["commit", "-m", "init"])
            .status()
            .unwrap();
        assert!(commit_status.success(), "git commit failed");

        let worktrees = list_live_worktrees(&repo).unwrap();
        let map = live_worktree_map(&worktrees);
        assert_eq!(map.len(), 1);
        assert!(
            map.values()
                .any(|worktree| worktree.branch.as_deref() == Some("main"))
        );
    }

    #[test]
    fn auto_creates_local_branch_when_only_remote_exists() {
        let remote_dir = TempDir::new().unwrap();
        let init_remote_status = std::process::Command::new("git")
            .current_dir(remote_dir.path())
            .args(["init", "--bare"])
            .status()
            .unwrap();
        assert!(init_remote_status.success(), "git init --bare failed");

        let seed_dir = TempDir::new().unwrap();
        let init_seed_status = std::process::Command::new("git")
            .current_dir(seed_dir.path())
            .args(["init", "--initial-branch=main"])
            .status()
            .unwrap();
        assert!(init_seed_status.success(), "git init failed");

        let seed_repo = git2::Repository::open(seed_dir.path()).unwrap();
        let mut cfg = seed_repo.config().unwrap();
        cfg.set_str("user.name", "Test").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();

        std::fs::write(seed_dir.path().join("a.txt"), "hello").unwrap();
        let add_status = std::process::Command::new("git")
            .current_dir(seed_dir.path())
            .args(["add", "a.txt"])
            .status()
            .unwrap();
        assert!(add_status.success(), "git add failed");
        let commit_status = std::process::Command::new("git")
            .current_dir(seed_dir.path())
            .args(["commit", "-m", "init"])
            .status()
            .unwrap();
        assert!(commit_status.success(), "git commit failed");

        let checkout_status = std::process::Command::new("git")
            .current_dir(seed_dir.path())
            .args(["checkout", "-b", "feature/test"])
            .status()
            .unwrap();
        assert!(checkout_status.success(), "git checkout failed");

        std::fs::write(seed_dir.path().join("feature.txt"), "feature").unwrap();
        let add_feature_status = std::process::Command::new("git")
            .current_dir(seed_dir.path())
            .args(["add", "feature.txt"])
            .status()
            .unwrap();
        assert!(add_feature_status.success(), "git add failed");
        let feature_commit_status = std::process::Command::new("git")
            .current_dir(seed_dir.path())
            .args(["commit", "-m", "feature"])
            .status()
            .unwrap();
        assert!(feature_commit_status.success(), "git commit failed");

        let remote_path = remote_dir.path().display().to_string();
        let add_remote_status = std::process::Command::new("git")
            .current_dir(seed_dir.path())
            .args(["remote", "add", "origin", &remote_path])
            .status()
            .unwrap();
        assert!(add_remote_status.success(), "git remote add failed");
        let push_status = std::process::Command::new("git")
            .current_dir(seed_dir.path())
            .args(["push", "origin", "main", "feature/test"])
            .status()
            .unwrap();
        assert!(push_status.success(), "git push failed");

        let clone_dir = TempDir::new().unwrap();
        let clone_path = clone_dir.path().join("clone");
        let clone_status = std::process::Command::new("git")
            .current_dir(clone_dir.path())
            .args(["clone", &remote_path, clone_path.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(clone_status.success(), "git clone failed");

        let repo = git2::Repository::open(&clone_path).unwrap();

        // ensure_local_branch_exists should auto-create local branch from remote
        ensure_local_branch_exists(&repo, "feature/test").unwrap();

        // Verify local branch exists
        let local_branch = repo.find_branch("feature/test", BranchType::Local).unwrap();

        // Verify it tracks origin/feature/test
        let upstream = local_branch.upstream().unwrap();
        let upstream_name = upstream.name().unwrap().unwrap();
        assert_eq!(upstream_name, "origin/feature/test");
    }

    #[test]
    fn preserves_original_error_when_branch_is_missing_everywhere() {
        let dir = TempDir::new().unwrap();
        let init_status = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["init", "--initial-branch=main"])
            .status()
            .unwrap();
        assert!(init_status.success(), "git init failed");

        let repo = git2::Repository::open(dir.path()).unwrap();
        let expected = repo
            .find_branch("missing", BranchType::Local)
            .err()
            .unwrap()
            .to_string();
        let actual = ensure_local_branch_exists(&repo, "missing")
            .unwrap_err()
            .to_string();
        assert_eq!(actual, expected);
    }
}
