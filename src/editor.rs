use anyhow::{Context, Result, anyhow};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn launch_editor(path: &Path) -> Result<()> {
    let editor = resolve_editor();
    let status = editor_command(&editor, path)
        .status()
        .with_context(|| format!("Failed to launch editor '{editor}'"))?;

    if !status.success() {
        return Err(anyhow!("Editor exited with non-zero status"));
    }

    Ok(())
}

/// Resolve the editor command following git's own precedence:
/// `$GIT_EDITOR` > `core.editor` > `$VISUAL` > `$EDITOR` > `vi`.
///
/// The returned string is a *command line* (it may contain arguments and
/// quoting), not just a program name — see [`editor_command`]. This is the
/// single source of truth for editor selection across the CLI; `kin continue`
/// forwards its result to `git` so subprocess rebases pick the same editor.
pub(crate) fn resolve_editor() -> String {
    select_editor(
        non_empty_env("GIT_EDITOR"),
        git_core_editor(),
        non_empty_env("VISUAL"),
        non_empty_env("EDITOR"),
    )
}

/// Apply the `$GIT_EDITOR` > `core.editor` > `$VISUAL` > `$EDITOR` > `vi`
/// precedence to already-resolved sources. Split out from [`resolve_editor`] so
/// the ordering can be unit-tested without touching process-global env vars or
/// the working directory.
fn select_editor(
    git_editor: Option<String>,
    core_editor: Option<String>,
    visual: Option<String>,
    editor: Option<String>,
) -> String {
    git_editor
        .or(core_editor)
        .or(visual)
        .or(editor)
        .unwrap_or_else(|| "vi".to_string())
}

fn non_empty_env(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

/// Read `core.editor` following git's config precedence. Uses the repository's
/// fully-layered config (local > global > system) when run inside a repo — so a
/// repo-local `core.editor` is honored, matching `git` — and falls back to the
/// global/system config otherwise. No `git` subprocess is required.
fn git_core_editor() -> Option<String> {
    let cfg = git2::Repository::discover(".")
        .and_then(|repo| repo.config())
        .or_else(|_| git2::Config::open_default())
        .ok()?;
    core_editor_value(&cfg)
}

/// Extract a non-empty `core.editor` from an already-resolved config. Split out
/// so the repo-local layering can be tested without depending on the process
/// working directory.
fn core_editor_value(cfg: &git2::Config) -> Option<String> {
    match cfg.get_string("core.editor") {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

/// Build the process that runs `editor` (a command line, possibly with
/// arguments and quoting) against `path`.
///
/// Like git, we hand the command line to a shell rather than splitting on
/// whitespace ourselves. This keeps quoted programs (`"C:/Program Files/..."`),
/// embedded flags (`code --wait`), and other shell constructs working. The
/// file path is passed as a positional argument so spaces in it are safe
/// regardless of how the editor command is written.
#[cfg(unix)]
fn editor_command(editor: &str, path: &Path) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(editor) // becomes $0
        .arg(path); // becomes "$@"
    cmd
}

#[cfg(not(unix))]
fn editor_command(editor: &str, path: &Path) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.arg("/C")
        .arg(format!("{editor} \"{}\"", path.display()));
    cmd
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

    /// Open the configured editor (resolved via the
    /// `$GIT_EDITOR` > `core.editor` > `$VISUAL` > `$EDITOR` > `vi` precedence)
    /// on the existing draft contents and return the result. Used for the
    /// recovery / retry path where the file already holds the user's text.
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

    /// Editor env vars are process-global, so tests that set them must not run
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
        // GIT_EDITOR has the highest precedence, so this is deterministic even
        // on machines with a global core.editor configured.
        unsafe { env::set_var("GIT_EDITOR", &script) };

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

        unsafe { env::remove_var("GIT_EDITOR") };
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
        unsafe { env::set_var("GIT_EDITOR", &script) };

        let draft = Draft::new(dir.path().join("kindra-drafts").join("body.md"));
        let out = draft.edit("PREFILL").unwrap();
        assert_eq!(out, "PREFILL\nEDITED");

        // reedit reopens the current on-disk contents (retry / recovery path).
        let out2 = draft.reedit().unwrap();
        assert_eq!(out2, "PREFILL\nEDITED\nEDITED");

        unsafe { env::remove_var("GIT_EDITOR") };
    }

    /// The editor command is handed to a shell, so a program path containing
    /// spaces (quoted) and trailing flags both survive — the old
    /// whitespace-split logic mangled either one.
    #[cfg(unix)]
    #[test]
    fn launch_editor_handles_quoted_program_and_flags() {
        let _guard = EDITOR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        // A directory with a space in its name forces the command to be quoted.
        let bin_dir = dir.path().join("editor bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let script = bin_dir.join("ed.sh");
        // The editor expects a leading `--flag` before the file path, proving
        // both the quoted program and its argument reached it intact.
        fs::write(
            &script,
            "#!/bin/sh\n[ \"$1\" = \"--flag\" ] || exit 3\nprintf ' EDITED' >> \"$2\"\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        // SAFETY: single-threaded test guarded by EDITOR_LOCK.
        unsafe { env::set_var("GIT_EDITOR", format!("\"{}\" --flag", script.display())) };

        let draft = Draft::new(dir.path().join("kindra-drafts").join("q.md"));
        let out = draft.edit("BODY").unwrap();
        assert_eq!(out, "BODY EDITED");

        unsafe { env::remove_var("GIT_EDITOR") };
    }

    #[test]
    fn select_editor_follows_full_precedence() {
        let s = |v: &str| Some(v.to_string());

        // Highest wins when present.
        assert_eq!(
            select_editor(s("g"), s("c"), s("vis"), s("ed")),
            "g",
            "GIT_EDITOR outranks all"
        );
        // Falls through one source at a time.
        assert_eq!(
            select_editor(None, s("c"), s("vis"), s("ed")),
            "c",
            "core.editor is next"
        );
        assert_eq!(
            select_editor(None, None, s("vis"), s("ed")),
            "vis",
            "VISUAL outranks EDITOR"
        );
        assert_eq!(
            select_editor(None, None, None, s("ed")),
            "ed",
            "EDITOR is used when nothing higher is set"
        );
        // Hardcoded final fallback when every source is absent.
        assert_eq!(
            select_editor(None, None, None, None),
            "vi",
            "vi is the last-resort default"
        );
    }

    #[test]
    fn core_editor_value_reads_repo_local_config() {
        // A repo-local `core.editor` must be honored: `repo.config()` layers
        // local > global > system, so this is what git itself would use. The
        // old code read only the global/system config and missed this.
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        repo.config()
            .unwrap()
            .set_str("core.editor", "repo-local-ed --wait")
            .unwrap();

        assert_eq!(
            core_editor_value(&repo.config().unwrap()).as_deref(),
            Some("repo-local-ed --wait")
        );
    }
}
