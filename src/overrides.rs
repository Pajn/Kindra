//! Suspend local overlays for an entire Git operation, including all of its
//! intermediate checkouts. Callers hold RepoLock until finalization completes.
//! State lives in the worktree's private git directory: linked worktrees share
//! configuration, but must never share suspended contents or recovery state.
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use git2::{Repository, RepositoryState};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    paths: Vec<String>,
    apply: Vec<String>,
}

#[derive(Deserialize)]
struct RepoConfig {
    overrides: Option<Config>,
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
enum Phase {
    Preparing,
    Suspended,
    Applying,
}

#[derive(Deserialize, Serialize)]
struct State {
    config: Config,
    phase: Phase,
    // Persist the user's intent before removing any overlay files, so recovery
    // cannot inadvertently reapply overrides after an interrupted removal.
    #[serde(default)]
    removing: bool,
    head: String,
    tree: String,
    files: Vec<SavedFile>,
}

#[derive(Deserialize, Serialize)]
struct SavedFile {
    path: String,
    tracked: bool,
    skip: bool,
    contents: Contents,
}

#[derive(Deserialize, Serialize)]
enum Contents {
    Missing,
    File { data: String, mode: u32 },
    Symlink(PathBuf),
}

pub fn state_path(repo: &Repository) -> PathBuf {
    repo.path().join("kindra_overrides_state.json")
}

fn disabled_path(repo: &Repository) -> PathBuf {
    repo.path().join("kindra_overrides_disabled")
}

pub fn is_disabled(repo: &Repository) -> bool {
    disabled_path(repo).exists()
}

fn config(repo: &Repository) -> Result<Option<Config>> {
    let path = repo.commondir().join("kindra.toml");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let config = toml::from_str::<RepoConfig>(&text)
        .with_context(|| format!("Failed to parse {}", path.display()))?
        .overrides;
    if let Some(config) = &config
        && (config.paths.is_empty()
            || config.paths.iter().any(|p| p.trim().is_empty())
            || config.apply.is_empty()
            || config.apply.iter().any(|p| p.trim().is_empty()))
    {
        bail!("overrides.paths and overrides.apply must be non-empty lists of non-empty strings");
    }
    Ok(config)
}

fn root(repo: &Repository) -> Result<&Path> {
    repo.workdir()
        .context("Local overrides require a non-bare worktree")
}

fn git(repo: &Repository, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut child = Command::new("git")
        .current_dir(root(repo)?)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let write_result = if let Some(input) = input {
        child
            .stdin
            .take()
            .context("Missing git stdin")?
            .write_all(input)
    } else {
        Ok(())
    };
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    write_result?;
    Ok(output.stdout)
}

fn paths(repo: &Repository, config: &Config, args: &[&str]) -> Result<Vec<String>> {
    let mut args = args.to_vec();
    args.push("--");
    args.extend(config.paths.iter().map(String::as_str));
    git(repo, &args, None)?
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8(s.to_vec()).context("Override paths must be valid UTF-8"))
        .collect()
}

fn input(paths: &[String]) -> Vec<u8> {
    paths.iter().flat_map(|p| p.bytes().chain([0])).collect()
}

fn flags(repo: &Repository, paths: &[String], skip: bool) -> Result<()> {
    if !paths.is_empty() {
        git(
            repo,
            &[
                "update-index",
                if skip {
                    "--skip-worktree"
                } else {
                    "--no-skip-worktree"
                },
                "-z",
                "--stdin",
            ],
            Some(&input(paths)),
        )?;
    }
    Ok(())
}

fn save(repo: &Repository, state: &State) -> Result<()> {
    crate::state_io::write_atomic_private(&state_path(repo), &serde_json::to_string(state)?)
}

fn busy(repo: &Repository) -> bool {
    repo.state() != RepositoryState::Clean
        || crate::rebase_utils::state_path(repo).exists()
        || crate::commands::run::run_state_exists(repo)
}

fn check_staged(repo: &Repository, config: &Config) -> Result<()> {
    if !paths(repo, config, &["diff", "--cached", "--name-only", "-z"])?.is_empty() {
        bail!(
            "An override path has staged changes. Commit or unstage them before managing overrides; the index was left intact."
        );
    }
    Ok(())
}

// Refuse traversal through an overlay symlink: restoring a tracked child must
// never write into an external overrides source directory through that link.
fn safe_path(repo: &Repository, name: &str) -> Result<PathBuf> {
    let path = Path::new(name);
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
        || path
            .components()
            .any(|c| c.as_os_str().eq_ignore_ascii_case(".git"))
    {
        bail!("Unsafe override path: {name:?}");
    }
    let root = root(repo)?;
    let full = root.join(path);
    let mut parent = full.parent();
    while let Some(path) = parent.filter(|path| *path != root) {
        match fs::symlink_metadata(path) {
            Ok(meta) if !meta.is_dir() => {
                bail!("Override parent is not a directory: {}", path.display())
            }
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err.into()),
            _ => {}
        }
        parent = path.parent();
    }
    Ok(full)
}

fn snapshot(repo: &Repository, config: Config) -> Result<State> {
    check_staged(repo, &config)?;
    // skip-worktree belongs to sparse checkout too; do not fight its index rules.
    if repo
        .config()?
        .get_bool("core.sparseCheckout")
        .unwrap_or(false)
    {
        bail!("Local overrides are not supported with sparse checkout");
    }
    let tracked: BTreeSet<_> = paths(repo, &config, &["ls-files", "--cached", "-z"])?
        .into_iter()
        .collect();
    let skipped: BTreeSet<_> = paths(repo, &config, &["ls-files", "-v", "-z"])?
        .into_iter()
        .filter(|p| p.starts_with("S ") || p.starts_with("s "))
        .map(|p| p[2..].to_owned())
        .collect();
    let mut all = tracked.clone();
    all.extend(paths(
        repo,
        &config,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?);
    all.extend(paths(
        repo,
        &config,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
    )?);
    let mut files = Vec::new();
    for name in all {
        let path = safe_path(repo, &name)?;
        let contents = match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => Contents::Symlink(fs::read_link(&path)?),
            Ok(meta) if meta.is_file() => {
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    meta.permissions().mode()
                };
                #[cfg(not(unix))]
                let mode = u32::from(meta.permissions().readonly());
                Contents::File {
                    data: STANDARD.encode(fs::read(&path)?),
                    mode,
                }
            }
            Ok(_) => bail!(
                "Override path is not a regular file or symlink: {}",
                path.display()
            ),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Contents::Missing,
            Err(err) => return Err(err.into()),
        };
        files.push(SavedFile {
            tracked: tracked.contains(&name),
            skip: skipped.contains(&name),
            path: name,
            contents,
        });
    }
    Ok(State {
        config,
        phase: Phase::Preparing,
        removing: false,
        files,
        head: String::from_utf8(git(repo, &["rev-parse", "HEAD"], None)?)?,
        tree: String::from_utf8(git(repo, &["write-tree"], None)?)?,
    })
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn suspend(repo: &Repository, state: &mut State) -> Result<()> {
    let tracked: Vec<_> = state
        .files
        .iter()
        .filter(|f| f.tracked)
        .map(|f| f.path.clone())
        .collect();
    flags(repo, &tracked, false)?;
    for file in &state.files {
        remove_file(&safe_path(repo, &file.path)?)?;
    }
    if !tracked.is_empty() {
        git(
            repo,
            &["checkout-index", "--force", "-z", "--stdin"],
            Some(&input(&tracked)),
        )?;
    }
    state.phase = Phase::Suspended;
    save(repo, state)
}

fn prepare(repo: &Repository, mut state: State) -> Result<State> {
    // Failure here must leave the original files and flags untouched.
    save(repo, &state)?;
    if let Err(err) = suspend(repo, &mut state) {
        // No Git operation has started, even if the Suspended checkpoint failed.
        return match rollback_preparation(repo, &state) {
            Ok(()) => Err(err),
            Err(restore) => Err(anyhow!(
                "{err:#}\nOverride recovery also failed: {restore:#}"
            )),
        };
    }
    Ok(state)
}

// The disabled marker must be durable before discarding the recovery state.
// Both files may coexist after interruption; recovery completes removal first.
fn finish_removal(repo: &Repository) -> Result<()> {
    crate::state_io::write_atomic(
        &disabled_path(repo),
        "Run 'kin overrides apply' to re-enable overrides in this worktree.\n",
    )?;
    fs::remove_file(state_path(repo))?;
    eprintln!(
        "Local overrides removed and disabled in this worktree. Run 'kin overrides apply' to re-enable them."
    );
    Ok(())
}

fn rollback_preparation(repo: &Repository, state: &State) -> Result<()> {
    if git(repo, &["rev-parse", "HEAD"], None)? != state.head.as_bytes()
        || git(repo, &["write-tree"], None)? != state.tree.as_bytes()
    {
        bail!(
            "Git changed during override preparation. Saved override contents are in {}; restore them manually before removing this state.",
            state_path(repo).display()
        );
    }
    for file in &state.files {
        let path = safe_path(repo, &file.path)?;
        remove_file(&path)?;
        if !matches!(file.contents, Contents::Missing) {
            fs::create_dir_all(path.parent().context("Override path has no parent")?)?;
        }
        match &file.contents {
            Contents::Missing => {}
            Contents::File { data, mode } => {
                fs::write(&path, STANDARD.decode(data)?)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&path, fs::Permissions::from_mode(*mode))?;
                }
                #[cfg(not(unix))]
                {
                    let mut permissions = fs::metadata(&path)?.permissions();
                    permissions.set_readonly(*mode != 0);
                    fs::set_permissions(&path, permissions)?;
                }
            }
            Contents::Symlink(target) => {
                #[cfg(unix)]
                std::os::unix::fs::symlink(target, &path)?;
                #[cfg(windows)]
                std::os::windows::fs::symlink_file(target, &path)?;
            }
        }
    }
    for skip in [false, true] {
        let files: Vec<_> = state
            .files
            .iter()
            .filter(|f| f.tracked && f.skip == skip)
            .map(|f| f.path.clone())
            .collect();
        flags(repo, &files, skip)?;
    }
    fs::remove_file(state_path(repo))?;
    Ok(())
}

fn apply(repo: &Repository, state: &mut State) -> Result<()> {
    check_staged(repo, &state.config)?;
    state.phase = Phase::Applying;
    save(repo, state)?;
    for script in &state.config.apply {
        eprintln!("Applying local overrides: {script}");
        #[cfg(windows)]
        let mut command = {
            let mut c = Command::new("cmd");
            c.args(["/C", script]);
            c
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut c = Command::new("sh");
            c.args(["-c", script]);
            c
        };
        let status = command
            .current_dir(root(repo)?)
            .env("KINDRA_WORKTREE_PATH", root(repo)?)
            .env(
                "KINDRA_WORKTREE_BRANCH",
                repo.head()?.shorthand().unwrap_or("HEAD"),
            )
            .status()?;
        if !status.success() {
            bail!(
                "Override apply hook failed ({status}). Fix the hook and run 'kin continue' in {}. Recovery state: {}",
                root(repo)?.display(),
                state_path(repo).display()
            );
        }
    }
    let tracked = paths(repo, &state.config, &["ls-files", "--cached", "-z"])?;
    flags(repo, &tracked, true)?;
    // A failed apply keeps both recovery state and the disabled marker. Only a
    // fully successful apply re-enables automatic override management.
    remove_file(&disabled_path(repo))?;
    fs::remove_file(state_path(repo))?;
    Ok(())
}

/// Wrap a complete operation while the caller holds the repository lock.
/// A stopped operation keeps its overlay suspended across continue/abort; an
/// ordinary error still reapplies it. Recovery never restores index contents,
/// which would destroy staged conflict resolutions.
pub fn with_suspended<T>(
    repo: &Repository,
    recovery: bool,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let mut state = if state_path(repo).exists() {
        if !recovery {
            bail!(
                "Local overrides are suspended or awaiting recovery. Run 'kin continue' or 'kin abort' in {}.",
                root(repo)?.display()
            );
        }
        let state: State = serde_json::from_str(&fs::read_to_string(state_path(repo))?)?;
        if state.phase == Phase::Preparing {
            rollback_preparation(repo, &state)?;
            return operation();
        }
        if state.removing {
            if busy(repo) {
                bail!(
                    "Cannot finish removing overrides while a Git or Kindra operation is in progress."
                );
            }
            finish_removal(repo)?;
            return operation();
        }
        state
    } else {
        if recovery || is_disabled(repo) {
            return operation();
        }
        let Some(config) = config(repo)? else {
            return operation();
        };
        if busy(repo) {
            bail!(
                "Cannot suspend local overrides while a Git or Kindra operation is in progress. Finish it with continue/abort first."
            );
        }
        prepare(repo, snapshot(repo, config)?)?
    };
    let result = operation();
    if busy(repo) {
        eprintln!(
            "Local overrides remain suspended. They will be reapplied after 'kin continue' or 'kin abort' completes."
        );
        return result;
    }
    let applied = apply(repo, &mut state);
    match (result, applied) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(err), Ok(())) | (Ok(_), Err(err)) => Err(err),
        (Err(err), Err(restore)) => Err(anyhow!(
            "{err:#}\nReapplying local overrides also failed: {restore:#}"
        )),
    }
}

pub fn apply_current(repo: &Repository) -> Result<()> {
    if state_path(repo).exists() {
        if busy(repo) {
            bail!(
                "Local overrides must remain suspended while an operation is in progress. Use 'kin continue' or 'kin abort'."
            );
        }
        with_suspended(repo, true, || Ok(()))
    } else if is_disabled(repo) {
        if busy(repo) {
            bail!(
                "Cannot re-enable local overrides while a Git or Kindra operation is in progress."
            );
        }
        let config = config(repo)?.context("No [overrides] configuration found")?;
        let state = snapshot(repo, config)?;
        // Once removed, these paths are ordinary working files. An explicit
        // apply must not discard new edits, including untracked original files.
        if !paths(repo, &state.config, &["diff", "--name-only", "-z"])?.is_empty()
            || state.files.iter().any(|file| !file.tracked)
        {
            bail!(
                "Override paths have uncommitted changes. Commit, stash, or move those edits before running 'kin overrides apply'. Overrides remain disabled."
            );
        }
        let mut state = prepare(repo, state)?;
        apply(repo, &mut state)
    } else {
        with_suspended(repo, false, || Ok(()))
    }
}

/// Restore the index's versions and leave automatic application disabled in
/// this worktree. Repeating the command never discards edits to the originals.
pub fn remove_current(repo: &Repository) -> Result<()> {
    if state_path(repo).exists() {
        bail!(
            "Local override recovery is pending. Run 'kin continue' or 'kin abort' before removing overrides."
        );
    }
    if is_disabled(repo) {
        eprintln!("Local overrides are already disabled in this worktree.");
        return Ok(());
    }
    if busy(repo) {
        bail!("Cannot remove local overrides while a Git or Kindra operation is in progress.");
    }
    let config = config(repo)?.context("No [overrides] configuration found")?;
    let mut state = snapshot(repo, config)?;
    state.removing = true;
    prepare(repo, state)?;
    finish_removal(repo)
}
