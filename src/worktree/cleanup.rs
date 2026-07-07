use crate::worktree::WorktreeRole;
use crate::worktree::config::WorktreeConfig;
use crate::worktree::git::LiveWorktree;
use crate::worktree::roles::role_for_path;
use anyhow::Result;
use git2::Repository;
use std::collections::HashSet;
use std::path::PathBuf;

/// A temp worktree eligible for cleanup because its branch has already been
/// merged into trunk. The managed set is derived from git's live worktrees plus
/// the configured temp path — there is no stored metadata to reconcile, so the
/// old "stale metadata" reason no longer exists (a worktree whose directory is
/// gone is git's own prunable state, handled separately).
#[derive(Clone, Debug)]
pub struct CleanupCandidate {
    pub branch: String,
    pub path: PathBuf,
    pub live: LiveWorktree,
}

pub fn find_cleanup_candidates(
    repo: &Repository,
    config: &WorktreeConfig,
    live_worktrees: &[LiveWorktree],
) -> Result<Vec<CleanupCandidate>> {
    if !config.temp.delete_merged {
        return Ok(Vec::new());
    }

    let merged_branches =
        crate::stack::collect_merged_local_branches(repo, &config.trunk, &[config.trunk.as_str()])?
            .into_iter()
            .collect::<HashSet<_>>();

    let mut candidates = Vec::new();
    for live in live_worktrees {
        // Only temp worktrees are cleanup candidates. Reuse the shared role
        // classifier rather than re-deriving the temp-root boundary here, so the
        // two never drift (and a main/review worktree nested under an overlapping
        // temp root is correctly excluded).
        if role_for_path(config, &live.normalized_path())? != Some(WorktreeRole::Temp) {
            continue;
        }
        // A worktree whose directory is gone is git's own prunable state (see the
        // struct doc): leave it to `git worktree prune` rather than cleaning it
        // here. Excluding it also keeps the downstream `is_worktree_dirty` check
        // from failing on a nonexistent path and aborting the whole run.
        if !live.path.exists() {
            continue;
        }
        let Some(branch) = &live.branch else {
            // A detached temp worktree has no branch to compare against trunk.
            continue;
        };
        if merged_branches.contains(branch) {
            candidates.push(CleanupCandidate {
                branch: branch.clone(),
                path: live.path.clone(),
                live: live.clone(),
            });
        }
    }

    candidates.sort_by(|left, right| left.branch.cmp(&right.branch));
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::find_cleanup_candidates;
    use crate::worktree::config::{
        HookListConfig, MainWorktreeConfig, ReviewWorktreeConfig, TempWorktreeConfig,
        WorktreeConfig,
    };
    use crate::worktree::git::LiveWorktree;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    /// Build a config whose managed paths live under `dir` (absolute, as in real
    /// usage where `resolve_config_path` anchors them at the repo root).
    fn config_for(dir: &Path) -> WorktreeConfig {
        let root = dir.join(".git/kindra-worktrees");
        WorktreeConfig {
            root: root.clone(),
            trunk: "main".to_string(),
            hooks: HookListConfig::default(),
            main: MainWorktreeConfig {
                enabled: true,
                branch: "main".to_string(),
                path: root.join("main"),
                allow_branch_switch: false,
                hooks: HookListConfig::default(),
            },
            review: ReviewWorktreeConfig {
                enabled: true,
                path: root.join("review"),
                reuse: true,
                clean_before_switch: true,
                hooks: HookListConfig::default(),
            },
            temp: TempWorktreeConfig {
                enabled: true,
                path_template: root.join("temp").join("{branch}"),
                delete_merged: true,
                hooks: HookListConfig::default(),
            },
        }
    }

    fn live(path: PathBuf, branch: &str) -> LiveWorktree {
        LiveWorktree {
            path,
            branch: Some(branch.to_string()),
            detached: false,
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "--initial-branch=main"]);
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        std::fs::write(dir.join("base.txt"), "base").unwrap();
        git(dir, &["add", "base.txt"]);
        git(dir, &["commit", "-m", "base"]);
    }

    #[test]
    fn finds_merged_temp_worktree() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        git(dir.path(), &["checkout", "-b", "feature/a"]);
        std::fs::write(dir.path().join("feature.txt"), "feature").unwrap();
        git(dir.path(), &["add", "feature.txt"]);
        git(dir.path(), &["commit", "-m", "feature"]);
        git(dir.path(), &["checkout", "main"]);
        git(dir.path(), &["merge", "--ff-only", "feature/a"]);
        // An unmerged sibling branch (its own commit is not in trunk) that must
        // not become a candidate.
        git(dir.path(), &["checkout", "-b", "feature/b"]);
        std::fs::write(dir.path().join("b.txt"), "b").unwrap();
        git(dir.path(), &["add", "b.txt"]);
        git(dir.path(), &["commit", "-m", "b work"]);
        git(dir.path(), &["checkout", "main"]);

        let repo = git2::Repository::open(dir.path()).unwrap();
        let config = config_for(dir.path());
        let feature_a = dir.path().join(".git/kindra-worktrees/temp/feature-a");
        let feature_b = dir.path().join(".git/kindra-worktrees/temp/feature-b");
        // The worktree directories exist on disk (only their presence matters here).
        std::fs::create_dir_all(&feature_a).unwrap();
        std::fs::create_dir_all(&feature_b).unwrap();
        let live_worktrees = vec![live(feature_a, "feature/a"), live(feature_b, "feature/b")];

        let candidates = find_cleanup_candidates(&repo, &config, &live_worktrees).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].branch, "feature/a");
    }

    #[test]
    fn skips_merged_temp_worktree_with_missing_directory() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        git(dir.path(), &["checkout", "-b", "feature/a"]);
        std::fs::write(dir.path().join("feature.txt"), "feature").unwrap();
        git(dir.path(), &["add", "feature.txt"]);
        git(dir.path(), &["commit", "-m", "feature"]);
        git(dir.path(), &["checkout", "main"]);
        git(dir.path(), &["merge", "--ff-only", "feature/a"]);

        let repo = git2::Repository::open(dir.path()).unwrap();
        let config = config_for(dir.path());
        // A merged temp branch whose worktree directory no longer exists (git
        // still lists it, prunable). It is left to `git worktree prune`, not
        // cleaned here — so it must not appear as a candidate.
        let live_worktrees = vec![live(
            dir.path().join(".git/kindra-worktrees/temp/feature-a"),
            "feature/a",
        )];

        let candidates = find_cleanup_candidates(&repo, &config, &live_worktrees).unwrap();
        assert!(
            candidates.is_empty(),
            "a merged temp worktree whose directory is gone must be excluded"
        );
    }

    #[test]
    fn ignores_worktrees_outside_the_temp_root() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        git(dir.path(), &["branch", "main-copy"]);

        let repo = git2::Repository::open(dir.path()).unwrap();
        let config = config_for(dir.path());
        // The main worktree sits under the root but not the temp root, so even if
        // its branch counts as merged it is never a temp cleanup candidate.
        let live_worktrees = vec![live(
            dir.path().join(".git/kindra-worktrees/main"),
            "main-copy",
        )];

        let candidates = find_cleanup_candidates(&repo, &config, &live_worktrees).unwrap();
        assert!(candidates.is_empty());
    }
}
