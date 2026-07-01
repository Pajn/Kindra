//! Shared helpers for durably persisting Kindra's on-disk operation state and
//! serializing concurrent `kin` invocations against each other.
//!
//! Two hazards motivate this module:
//!
//! 1. A crash (or full disk) partway through rewriting a state file used to leave
//!    behind a truncated, unparseable JSON document. Every later command then
//!    treated the corrupt file as an active operation and refused to run, and even
//!    `kin abort` could not recover because it also parses the file. [`write_atomic`]
//!    writes to a sibling temp file, fsyncs it, and renames it into place so readers
//!    only ever observe a complete document.
//! 2. Two `kin` processes running concurrently in the same repository could both
//!    pass the "is an operation already in progress?" check and then clobber each
//!    other's state file. [`RepoLock`] takes a whole-repository advisory lock so the
//!    second process fails fast with a clear message instead.

use anyhow::{Context, Result, anyhow};
use fs2::FileExt;
use git2::Repository;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Atomically replace the contents of `path` with `contents`.
///
/// Writes to a uniquely named temp file in the same directory, flushes it to
/// disk, and renames it over the destination. A crash can leave a stale
/// `*.tmp.*` file behind but never a partially written destination file.
pub fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create directory '{}' for state file",
                parent.display()
            )
        })?;
    }

    let temp_path = temp_path_for(path);
    // Clear any leftover temp file from a previous crashed write.
    match fs::remove_file(&temp_path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(anyhow!(
                "Failed to remove stale temp file '{}': {}",
                temp_path.display(),
                err
            ));
        }
    }

    let mut temp = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)
        .with_context(|| format!("Failed to create temp file '{}'", temp_path.display()))?;
    temp.write_all(contents.as_bytes())?;
    temp.sync_all()?;
    drop(temp);

    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "Failed to move temp file '{}' into place at '{}'",
            temp_path.display(),
            path.display()
        )
    })?;

    // Fsync the containing directory so the rename (the directory entry update)
    // is itself durable, not just the file contents. Best-effort: some platforms
    // do not permit opening a directory as a file.
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        && let Ok(dir) = File::open(parent)
    {
        let _ = dir.sync_all();
    }

    Ok(())
}

fn temp_path_for(path: &Path) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let filename = format!(
        "{}.tmp.{}.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        std::process::id(),
        suffix
    );
    path.with_file_name(filename)
}

/// Path to the repository-wide advisory lock file guarding Kindra operations.
pub fn lock_path(repo: &Repository) -> PathBuf {
    repo.path().join("kindra.lock")
}

/// A held exclusive lock over all Kindra state mutations in a repository.
///
/// The lock is released when the guard is dropped, i.e. when the current `kin`
/// process exits. It only serializes *concurrent* processes; it is not held
/// across the multiple invocations that make up a single conflict-interrupted
/// operation (the on-disk state file already represents that).
#[must_use = "dropping the guard immediately releases the lock"]
pub struct RepoLock {
    _file: File,
}

impl RepoLock {
    /// Acquire the repository lock, failing fast if another `kin` process holds it.
    pub fn acquire(repo: &Repository) -> Result<Self> {
        let path = lock_path(repo);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("Failed to open Kindra lock file '{}'", path.display()))?;

        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self { _file: file }),
            // Only genuine lock contention means another process holds the lock.
            Err(err) if err.kind() == fs2::lock_contended_error().kind() => Err(anyhow!(
                "Another 'kin' process is operating on this repository. Wait for it to finish and try again. \
                 If you are sure no other 'kin' process is running, remove '{}'.",
                path.display()
            )),
            // Anything else (unsupported locking, permissions, I/O) is a real error
            // and must be surfaced rather than disguised as contention.
            Err(err) => Err(anyhow::Error::from(err).context(format!(
                "Failed to acquire Kindra lock file '{}'",
                path.display()
            ))),
        }
    }
}
