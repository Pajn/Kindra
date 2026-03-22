use crate::worktree::WorktreeRole;
use crate::worktree::config::WorktreeConfig;
use crate::worktree::git::LiveWorktree;
use crate::worktree::metadata::{ManagedWorktreeRecord, WorktreeMetadata};
use anyhow::Result;
use git2::Repository;
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CleanupReason {
    Merged,
    StaleMetadata,
}

#[derive(Clone, Debug)]
pub struct CleanupCandidate {
    pub record: ManagedWorktreeRecord,
    pub live: Option<LiveWorktree>,
    pub reason: CleanupReason,
}

pub fn find_cleanup_candidates(
    repo: &Repository,
    config: &WorktreeConfig,
    metadata: &WorktreeMetadata,
    live_worktrees: &[LiveWorktree],
) -> Result<Vec<CleanupCandidate>> {
    let live_by_path = crate::worktree::git::live_worktree_map(live_worktrees);
    let merged_branches = if config.temp.delete_merged {
        crate::stack::collect_merged_local_branches(repo, &config.trunk, &[config.trunk.as_str()])?
            .into_iter()
            .collect::<HashSet<_>>()
    } else {
        HashSet::new()
    };

    let mut candidates = Vec::new();
    for record in metadata.records() {
        if record.role != WorktreeRole::Temp {
            continue;
        }

        let live = live_by_path.get(&record.normalized_path()).cloned();
        if live.is_none() && !record.path_buf().exists() {
            candidates.push(CleanupCandidate {
                record: record.clone(),
                live: None,
                reason: CleanupReason::StaleMetadata,
            });
            continue;
        }

        if merged_branches.contains(&record.branch) {
            candidates.push(CleanupCandidate {
                record: record.clone(),
                live,
                reason: CleanupReason::Merged,
            });
        }
    }

    candidates.sort_by(|left, right| left.record.branch.cmp(&right.record.branch));
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::{CleanupReason, find_cleanup_candidates};
    use crate::worktree::WorktreeRole;
    use crate::worktree::config::{
        HookListConfig, MainWorktreeConfig, ReviewWorktreeConfig, TempWorktreeConfig,
        WorktreeConfig,
    };
    use crate::worktree::metadata::WorktreeMetadata;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn finds_stale_temp_metadata() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        let mut metadata = WorktreeMetadata::default();
        metadata.upsert(WorktreeRole::Temp, "feature/a", &dir.path().join("missing"));

        let config = WorktreeConfig {
            root: PathBuf::from(".git/kindra-worktrees"),
            trunk: "main".to_string(),
            hooks: HookListConfig::default(),
            main: MainWorktreeConfig {
                enabled: true,
                branch: "main".to_string(),
                path: PathBuf::from(".git/kindra-worktrees/main"),
                allow_branch_switch: false,
                hooks: HookListConfig::default(),
            },
            review: ReviewWorktreeConfig {
                enabled: true,
                path: PathBuf::from(".git/kindra-worktrees/review"),
                reuse: true,
                clean_before_switch: true,
                hooks: HookListConfig::default(),
            },
            temp: TempWorktreeConfig {
                enabled: true,
                path_template: PathBuf::from(".git/kindra-worktrees/temp/{branch}"),
                delete_merged: false,
                hooks: HookListConfig::default(),
            },
        };

        let candidates = find_cleanup_candidates(&repo, &config, &metadata, &[]).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].reason, CleanupReason::StaleMetadata);
    }

    #[test]
    fn finds_merged_temp_metadata() {
        let dir = TempDir::new().unwrap();
        let init_status = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["init", "--initial-branch=main"])
            .status()
            .unwrap();
        assert!(init_status.success(), "git init failed");
        let repo = git2::Repository::open(dir.path()).unwrap();
        let mut repo_config = repo.config().unwrap();
        repo_config.set_str("user.name", "Test").unwrap();
        repo_config
            .set_str("user.email", "test@example.com")
            .unwrap();

        std::fs::write(dir.path().join("base.txt"), "base").unwrap();
        let add_base = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["add", "base.txt"])
            .status()
            .unwrap();
        assert!(add_base.success(), "git add base failed");
        let commit_base = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["commit", "-m", "base"])
            .status()
            .unwrap();
        assert!(commit_base.success(), "git commit base failed");

        let checkout_feature = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["checkout", "-b", "feature/a"])
            .status()
            .unwrap();
        assert!(checkout_feature.success(), "git checkout feature failed");
        std::fs::write(dir.path().join("feature.txt"), "feature").unwrap();
        let add_feature = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["add", "feature.txt"])
            .status()
            .unwrap();
        assert!(add_feature.success(), "git add feature failed");
        let commit_feature = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["commit", "-m", "feature"])
            .status()
            .unwrap();
        assert!(commit_feature.success(), "git commit feature failed");
        let checkout_main = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["checkout", "main"])
            .status()
            .unwrap();
        assert!(checkout_main.success(), "git checkout main failed");
        let merge_feature = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["merge", "--ff-only", "feature/a"])
            .status()
            .unwrap();
        assert!(merge_feature.success(), "git merge feature failed");

        let temp_path = dir.path().join(".git/kindra-worktrees/temp/feature-a");
        std::fs::create_dir_all(&temp_path).unwrap();
        let mut metadata = WorktreeMetadata::default();
        metadata.upsert(WorktreeRole::Temp, "feature/a", &temp_path);

        let config = WorktreeConfig {
            root: PathBuf::from(".git/kindra-worktrees"),
            trunk: "main".to_string(),
            hooks: HookListConfig::default(),
            main: MainWorktreeConfig {
                enabled: true,
                branch: "main".to_string(),
                path: PathBuf::from(".git/kindra-worktrees/main"),
                allow_branch_switch: false,
                hooks: HookListConfig::default(),
            },
            review: ReviewWorktreeConfig {
                enabled: true,
                path: PathBuf::from(".git/kindra-worktrees/review"),
                reuse: true,
                clean_before_switch: true,
                hooks: HookListConfig::default(),
            },
            temp: TempWorktreeConfig {
                enabled: true,
                path_template: PathBuf::from(".git/kindra-worktrees/temp/{branch}"),
                delete_merged: true,
                hooks: HookListConfig::default(),
            },
        };

        let candidates = find_cleanup_candidates(&repo, &config, &metadata, &[]).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].reason, CleanupReason::Merged);
    }
}
