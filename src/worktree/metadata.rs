use crate::worktree::WorktreeRole;
use crate::worktree::path_resolver::normalize_path;
use anyhow::{Result, anyhow};
use fs2::FileExt;
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const METADATA_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedWorktreeRecord {
    pub role: WorktreeRole,
    pub branch: String,
    pub path: String,
    pub created_at: u64,
    pub last_used_at: u64,
}

impl ManagedWorktreeRecord {
    pub fn path_buf(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }

    pub fn normalized_path(&self) -> PathBuf {
        normalize_path(&self.path)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct MetadataFile {
    version: u32,
    worktrees: Vec<ManagedWorktreeRecord>,
}

#[derive(Debug)]
pub struct WorktreeMetadata {
    file: MetadataFile,
    original_file: MetadataFile,
    path: Option<PathBuf>,
}

impl WorktreeMetadata {
    pub fn load(repo: &Repository) -> Result<Self> {
        let path = metadata_path(repo);
        let file = read_metadata(path.as_path())?;

        Ok(Self {
            original_file: file.clone(),
            file,
            path: Some(path),
        })
    }

    pub fn save(self, repo: &Repository) -> Result<()> {
        let path = self.path.unwrap_or_else(|| metadata_path(repo));
        let _lock = open_locked_metadata_file(&path)?;
        let mut latest = read_metadata(path.as_path())?;
        apply_delta(&self.original_file, &self.file, &mut latest);
        let raw = serde_json::to_string_pretty(&latest)?;
        let temp_path = temp_metadata_path(&path);
        match fs::remove_file(&temp_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(anyhow!(
                    "Failed to remove stale metadata temp file '{}': {}",
                    temp_path.display(),
                    err
                ));
            }
        }
        let mut temp = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        temp.write_all(raw.as_bytes())?;
        temp.sync_all()?;
        drop(temp);
        fs::rename(&temp_path, &path)?;
        Ok(())
    }

    pub fn records(&self) -> &[ManagedWorktreeRecord] {
        &self.file.worktrees
    }

    pub fn find_role(&self, role: WorktreeRole) -> Option<&ManagedWorktreeRecord> {
        self.file
            .worktrees
            .iter()
            .find(|record| record.role == role)
    }

    pub fn find_temp_branch(&self, branch: &str) -> Option<&ManagedWorktreeRecord> {
        self.file
            .worktrees
            .iter()
            .find(|record| record.role == WorktreeRole::Temp && record.branch == branch)
    }

    pub fn find_by_path(&self, path: &Path) -> Option<&ManagedWorktreeRecord> {
        let path = normalize_path(path);
        self.file
            .worktrees
            .iter()
            .find(|record| record.normalized_path() == path)
    }

    pub fn upsert(&mut self, role: WorktreeRole, branch: &str, path: &Path) {
        let now = unix_timestamp();
        let normalized = normalize_path(path).to_string_lossy().to_string();

        if let Some(existing) = self
            .file
            .worktrees
            .iter_mut()
            .find(|record| record.role == role && record.branch == branch)
        {
            existing.path = normalized;
            existing.last_used_at = now;
            return;
        }

        match role {
            WorktreeRole::Main | WorktreeRole::Review => {
                self.file.worktrees.retain(|record| record.role != role);
            }
            WorktreeRole::Temp => {
                self.file
                    .worktrees
                    .retain(|record| !(record.role == role && record.branch == branch));
            }
        }

        self.file.worktrees.push(ManagedWorktreeRecord {
            role,
            branch: branch.to_string(),
            path: normalized,
            created_at: now,
            last_used_at: now,
        });
    }

    pub fn remove_role(&mut self, role: WorktreeRole) {
        self.file.worktrees.retain(|record| record.role != role);
    }

    pub fn remove_temp_branch(&mut self, branch: &str) {
        self.file
            .worktrees
            .retain(|record| !(record.role == WorktreeRole::Temp && record.branch == branch));
    }
}

impl Default for WorktreeMetadata {
    fn default() -> Self {
        let file = MetadataFile {
            version: METADATA_VERSION,
            worktrees: Vec::new(),
        };
        Self {
            original_file: file.clone(),
            file,
            path: None,
        }
    }
}

pub fn metadata_path(repo: &Repository) -> PathBuf {
    repo.commondir().join("kindra_worktrees.json")
}

fn open_locked_metadata_file(path: &Path) -> Result<File> {
    let lock_path = metadata_lock_path(path);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    file.lock_exclusive()?;
    Ok(file)
}

fn read_metadata(path: &Path) -> Result<MetadataFile> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err.into()),
    };

    if raw.trim().is_empty() {
        return Ok(MetadataFile {
            version: METADATA_VERSION,
            worktrees: Vec::new(),
        });
    }

    let file: MetadataFile = serde_json::from_str(&raw)?;
    if file.version != METADATA_VERSION {
        return Err(anyhow!(
            "Unsupported Kindra worktree metadata version {} in {}.",
            file.version,
            path.display()
        ));
    }

    Ok(file)
}

fn apply_delta(original: &MetadataFile, updated: &MetadataFile, latest: &mut MetadataFile) {
    let original_map = record_map(original);
    let updated_map = record_map(updated);

    for key in original_map.keys() {
        if !updated_map.contains_key(key) {
            match key.0 {
                WorktreeRole::Main | WorktreeRole::Review => {
                    latest.worktrees.retain(|record| record.role != key.0);
                }
                WorktreeRole::Temp => {
                    latest.worktrees.retain(|record| record_key(record) != *key);
                }
            }
        }
    }

    for (key, updated_record) in updated_map {
        let changed = match original_map.get(&key) {
            Some(original_record) => original_record != &updated_record,
            None => true,
        };
        if changed {
            upsert_record(&mut latest.worktrees, updated_record);
        }
    }
}

fn record_map(file: &MetadataFile) -> HashMap<(WorktreeRole, String), ManagedWorktreeRecord> {
    file.worktrees
        .iter()
        .cloned()
        .map(|record| (record_key(&record), record))
        .collect()
}

fn record_key(record: &ManagedWorktreeRecord) -> (WorktreeRole, String) {
    (record.role, record.branch.clone())
}

fn upsert_record(records: &mut Vec<ManagedWorktreeRecord>, record: ManagedWorktreeRecord) {
    if let Some(existing) = records
        .iter_mut()
        .find(|existing| existing.role == record.role && existing.branch == record.branch)
    {
        *existing = record;
        return;
    }

    match record.role {
        WorktreeRole::Main | WorktreeRole::Review => {
            records.retain(|existing| existing.role != record.role);
        }
        WorktreeRole::Temp => {
            records.retain(|existing| {
                !(existing.role == WorktreeRole::Temp && existing.branch == record.branch)
            });
        }
    }

    records.push(record);
}

fn metadata_lock_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.lock",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("metadata")
    ))
}

fn temp_metadata_path(path: &Path) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let filename = format!(
        "{}.tmp.{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("metadata"),
        std::process::id(),
        suffix
    );
    path.with_file_name(filename)
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::{
        METADATA_VERSION, ManagedWorktreeRecord, MetadataFile, WorktreeMetadata, apply_delta,
        metadata_lock_path, metadata_path,
    };
    use crate::worktree::WorktreeRole;
    use fs2::FileExt;
    use std::fs::OpenOptions;
    use std::thread::sleep;
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn round_trips_metadata() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        let mut metadata = WorktreeMetadata::default();
        metadata.upsert(
            WorktreeRole::Temp,
            "feature/a",
            dir.path().join("temp").as_path(),
        );
        metadata.save(&repo).unwrap();

        let loaded = WorktreeMetadata::load(&repo).unwrap();
        assert_eq!(loaded.records().len(), 1);
        assert_eq!(loaded.records()[0].branch, "feature/a");
    }

    #[test]
    fn removes_temp_branch() {
        let mut metadata = WorktreeMetadata::default();
        metadata.upsert(WorktreeRole::Temp, "feature/a", std::path::Path::new("one"));
        metadata.upsert(WorktreeRole::Temp, "feature/b", std::path::Path::new("two"));

        metadata.remove_temp_branch("feature/a");

        assert_eq!(metadata.records().len(), 1);
        assert_eq!(metadata.records()[0].branch, "feature/b");
    }

    #[test]
    fn upsert_updates_existing_temp_record_in_place() {
        let mut metadata = WorktreeMetadata::default();
        metadata.upsert(WorktreeRole::Temp, "feature/a", std::path::Path::new("one"));
        let original = metadata.records()[0].clone();

        sleep(Duration::from_secs(1));
        metadata.upsert(WorktreeRole::Temp, "feature/a", std::path::Path::new("two"));

        assert_eq!(metadata.records().len(), 1);
        assert_eq!(metadata.records()[0].created_at, original.created_at);
        assert!(metadata.records()[0].last_used_at >= original.last_used_at);
        assert_eq!(
            metadata.records()[0].path_buf(),
            std::path::PathBuf::from("two")
        );
    }

    #[test]
    fn metadata_path_uses_common_git_dir() {
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
        let branch_status = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["branch", "linked"])
            .status()
            .unwrap();
        assert!(branch_status.success(), "git branch failed");

        let worktree_path = dir.path().join("linked-worktree");
        let worktree_status = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["worktree", "add", worktree_path.to_str().unwrap(), "linked"])
            .status()
            .unwrap();
        assert!(worktree_status.success(), "git worktree add failed");

        let worktree_repo = git2::Repository::open(&worktree_path).unwrap();
        assert_eq!(metadata_path(&repo), metadata_path(&worktree_repo));
        assert_eq!(
            metadata_path(&repo),
            repo.commondir().join("kindra_worktrees.json")
        );
    }

    #[test]
    fn load_does_not_hold_exclusive_lock() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        WorktreeMetadata::default().save(&repo).unwrap();

        let _metadata = WorktreeMetadata::load(&repo).unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(metadata_lock_path(&metadata_path(&repo)))
            .unwrap();
        let mut locked = false;
        for _ in 0..10 {
            if file.try_lock_exclusive().is_ok() {
                locked = true;
                break;
            }
            sleep(Duration::from_millis(10));
        }
        assert!(
            locked,
            "metadata load should not keep the sidecar lock held"
        );
    }

    #[test]
    fn apply_delta_removes_singleton_role_even_if_branch_changed_concurrently() {
        let original = MetadataFile {
            version: METADATA_VERSION,
            worktrees: vec![ManagedWorktreeRecord {
                role: WorktreeRole::Review,
                branch: "feature-a".to_string(),
                path: "/tmp/review-a".to_string(),
                created_at: 1,
                last_used_at: 1,
            }],
        };
        let updated = MetadataFile {
            version: METADATA_VERSION,
            worktrees: Vec::new(),
        };
        let mut latest = MetadataFile {
            version: METADATA_VERSION,
            worktrees: vec![ManagedWorktreeRecord {
                role: WorktreeRole::Review,
                branch: "feature-b".to_string(),
                path: "/tmp/review-b".to_string(),
                created_at: 2,
                last_used_at: 2,
            }],
        };

        apply_delta(&original, &updated, &mut latest);

        assert!(latest.worktrees.is_empty());
    }
}
