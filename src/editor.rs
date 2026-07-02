use anyhow::{Context, Result, anyhow};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn launch_editor(path: &Path) -> Result<()> {
    let editor = env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let mut editor_parts = editor.split_whitespace();
    let editor_program = editor_parts
        .next()
        .ok_or_else(|| anyhow!("EDITOR is empty"))?;
    let editor_args: Vec<&str> = editor_parts.collect();

    let status = Command::new(editor_program)
        .args(&editor_args)
        .arg(path)
        .status()
        .with_context(|| format!("Failed to launch editor '{}'", editor))?;

    if !status.success() {
        return Err(anyhow!("Editor exited with non-zero status"));
    }

    Ok(())
}

/// Compute a stable draft-file path under the git dir, mirroring git's own
/// `COMMIT_EDITMSG` convention. `key` is sanitized so branch names containing
/// `/` (or other separators) don't escape the drafts directory.
///
/// Sanitization alone is lossy — `feature/foo` and `feature-foo` would collapse
/// to the same filename and clobber each other's drafts — so a short
/// deterministic hash of the *original* key is appended. Same key ⇒ same path
/// (stable recovery); different keys ⇒ different paths (no collision).
pub fn draft_path(git_dir: &Path, key: &str) -> PathBuf {
    let safe: String = key
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '-',
        })
        .collect();
    git_dir
        .join("kindra-drafts")
        .join(format!("{safe}-{:08x}.md", draft_key_hash(key)))
}

/// Small deterministic hash (FNV-1a, folded to 32 bits) used only to
/// disambiguate draft filenames. Stable across runs and binary versions.
fn draft_key_hash(key: &str) -> u32 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash ^ (hash >> 32)) as u32
}

/// A durable draft file backing an `$EDITOR` session.
///
/// Unlike a `NamedTempFile`, the backing file is **not** deleted when this
/// value is dropped. It survives editor failures, downstream operation
/// failures (e.g. a failed `gh pr create`), and process crashes, so the user's
/// text can always be recovered. Call [`Draft::discard`] on the success path to
/// remove it.
pub struct Draft {
    path: PathBuf,
}

impl Draft {
    /// Create a draft handle for `path`. Does not touch the filesystem until
    /// [`Draft::edit`] writes to it.
    pub fn new(path: PathBuf) -> Self {
        Draft { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The current on-disk contents, if a non-empty draft exists (e.g. left
    /// behind by an earlier failed run).
    pub fn recover(&self) -> Option<String> {
        match fs::read_to_string(&self.path) {
            Ok(contents) if !contents.trim().is_empty() => Some(contents),
            _ => None,
        }
    }

    /// Write `prefill` to the draft file, open `$EDITOR` on it, and return the
    /// edited contents. The file is left in place on return (success or error).
    pub fn edit(&self, prefill: &str) -> Result<String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create drafts directory '{}'", parent.display())
            })?;
        }
        fs::write(&self.path, prefill)
            .with_context(|| format!("Failed to write draft '{}'", self.path.display()))?;
        self.reedit()
    }

    /// Resume-aware edit: if a non-empty draft already exists on disk (left by
    /// an earlier failed run), reopen it as-is instead of overwriting it with
    /// `prefill`; otherwise behave like [`Draft::edit`]. This is the safe
    /// default for buffers that are regenerated each run (split/reorder), where
    /// a blind `edit()` would clobber the recovery file before the user could
    /// fix and re-run.
    pub fn edit_or_resume(&self, prefill: &str) -> Result<String> {
        if self.recover().is_some() {
            self.reedit()
        } else {
            self.edit(prefill)
        }
    }

    /// Open `$EDITOR` on the existing draft contents and return the result.
    /// Used for the recovery / retry path where the file already holds the
    /// user's text.
    pub fn reedit(&self) -> Result<String> {
        launch_editor(&self.path)?;
        fs::read_to_string(&self.path)
            .with_context(|| format!("Failed to read draft '{}'", self.path.display()))
    }

    /// Remove the draft file. Call on the success path. Missing files are
    /// ignored; a failed removal is not fatal (the stale draft would simply be
    /// offered for recovery next time).
    pub fn discard(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `$EDITOR` is process-global, so tests that set it must not run
    /// concurrently. Serialize them behind this lock.
    static EDITOR_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn draft_path_sanitizes_branch_separators() {
        let p = draft_path(Path::new("/repo/.git"), "pr-body-feature/foo bar");
        let name = p.file_name().unwrap().to_str().unwrap();
        // Readable sanitized stem, plus an 8-hex disambiguator, under the drafts dir.
        assert!(p.starts_with("/repo/.git/kindra-drafts"));
        assert!(
            name.starts_with("pr-body-feature-foo-bar-") && name.ends_with(".md"),
            "unexpected draft filename: {name}"
        );
    }

    #[test]
    fn draft_path_avoids_collisions_between_keys_that_sanitize_alike() {
        let git = Path::new("/repo/.git");
        // These sanitize to the same readable stem but must not share a file.
        let a = draft_path(git, "pr-body-feature/foo");
        let b = draft_path(git, "pr-body-feature-foo");
        assert_ne!(a, b, "distinct branch names must map to distinct drafts");
        // Same key is stable across calls (so recovery finds the right file).
        assert_eq!(a, draft_path(git, "pr-body-feature/foo"));
    }

    #[test]
    fn edit_or_resume_reopens_existing_draft_without_overwriting() {
        let _guard = EDITOR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // A no-op $EDITOR leaves the file untouched, so a resumed draft's content
        // must come from disk, not from `prefill`.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("noop-editor.sh");
        fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        // SAFETY: single-threaded test; no other thread reads the environment.
        unsafe { env::set_var("EDITOR", &script) };

        let draft = Draft::new(dir.path().join("kindra-drafts").join("resume.md"));
        fs::create_dir_all(draft.path().parent().unwrap()).unwrap();
        fs::write(draft.path(), "SAVED FROM EARLIER RUN").unwrap();

        // A pre-existing draft is resumed, not clobbered by the fresh prefill.
        let resumed = draft.edit_or_resume("FRESHLY GENERATED PREFILL").unwrap();
        assert_eq!(resumed, "SAVED FROM EARLIER RUN");

        // With no draft present, it falls back to writing the prefill.
        draft.discard();
        let fresh = draft.edit_or_resume("FRESHLY GENERATED PREFILL").unwrap();
        assert_eq!(fresh, "FRESHLY GENERATED PREFILL");

        unsafe { env::remove_var("EDITOR") };
    }

    #[test]
    fn recover_reflects_disk_state_and_discard_removes() {
        let dir = tempfile::tempdir().unwrap();
        let draft = Draft::new(dir.path().join("kindra-drafts").join("d.md"));

        // No file yet, and whitespace-only content, are both "nothing to recover".
        assert!(draft.recover().is_none());
        fs::create_dir_all(draft.path().parent().unwrap()).unwrap();
        fs::write(draft.path(), "   \n").unwrap();
        assert!(draft.recover().is_none());

        fs::write(draft.path(), "real body").unwrap();
        assert_eq!(draft.recover().as_deref(), Some("real body"));

        draft.discard();
        assert!(draft.recover().is_none());
        assert!(!draft.path().exists());
    }

    #[test]
    fn edit_writes_prefill_then_captures_editor_output() {
        let _guard = EDITOR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // A fake $EDITOR that appends a marker line to whatever it's given,
        // proving the prefill reached the file and the edits are read back.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-editor.sh");
        fs::write(&script, "#!/bin/sh\nprintf '\\nEDITED' >> \"$1\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }

        // SAFETY: single-threaded test; no other thread reads the environment.
        unsafe { env::set_var("EDITOR", &script) };

        let draft = Draft::new(dir.path().join("kindra-drafts").join("body.md"));
        let out = draft.edit("PREFILL").unwrap();
        assert_eq!(out, "PREFILL\nEDITED");

        // reedit reopens the current on-disk contents (retry / recovery path).
        let out2 = draft.reedit().unwrap();
        assert_eq!(out2, "PREFILL\nEDITED\nEDITED");

        unsafe { env::remove_var("EDITOR") };
    }
}
