//! Kindra configuration files.
//!
//! **Repository config** is the `kindra.toml` in a repository's common Git
//! directory, so one file is shared by every worktree of the repository.
//! **Global config** is the user's `kindra/config.toml` under the platform
//! config directory; it applies to every repository and supports only the
//! `[restack]` and `[rebase]` sections, which repository config overrides.
//!
//! This module owns locating, reading and parsing both files. Each document is
//! parsed once into a TOML table; the modules that own a section deserialize
//! it themselves through [`ConfigFile::section`], so no single schema couples
//! every module to every section. A syntax error fails every command that
//! reads the file; a type error in a section fails only that section's
//! consumers.
//!
//! Read Kindra config only through this module.

use anyhow::{Context, Result};
use git2::Repository;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

const REPO_CONFIG_FILE: &str = "kindra.toml";

/// Top-level keys of repository config and the module that owns each.
/// Anything else is reported as unknown so typos don't go unnoticed.
const KNOWN_REPO_KEYS: &[&str] = &[
    // `upstream_branch` is the trunk; resolved by `crate::trunk`.
    "upstream_branch",
    // Settings layered over global config, below.
    "rebase",
    "restack",
    // `crate::worktree::config`.
    "worktrees",
    // `crate::overrides`.
    "overrides",
    // `crate::hooks`.
    "hooks",
];

/// Default for `[restack] history_limit`.
pub const DEFAULT_RESTACK_HISTORY_LIMIT: usize = 100;

/// A parsed Kindra config file. Missing files parse as empty documents.
#[derive(Debug)]
pub struct ConfigFile {
    kind: &'static str,
    path: PathBuf,
    table: toml::Table,
}

impl ConfigFile {
    fn load(kind: &'static str, path: PathBuf) -> Result<Self> {
        let table = match std::fs::read_to_string(&path) {
            Ok(raw) => raw
                .parse::<toml::Table>()
                .with_context(|| format!("Failed to parse {kind} config at {}", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("Failed to read {kind} config at {}", path.display())
                });
            }
        };
        Ok(Self { kind, path, table })
    }

    /// Where the file lives (or would live), for messages that point the user
    /// at the setting to change.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Deserialize the top-level `key` (a table section or a plain value),
    /// or `None` when it is absent.
    pub fn section<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let Some(value) = self.table.get(key) else {
            return Ok(None);
        };
        value.clone().try_into().map(Some).with_context(|| {
            format!(
                "Invalid `{key}` in {} config at {}",
                self.kind,
                self.path.display()
            )
        })
    }

    fn warn_unknown_keys(&self, known: &[&str]) {
        for key in self
            .table
            .keys()
            .filter(|key| !known.contains(&key.as_str()))
        {
            eprintln!(
                "warning: ignoring unknown key `{key}` in {} config at {}",
                self.kind,
                self.path.display()
            );
        }
    }
}

/// The repository config for `repo`, from its common Git directory — the same
/// file whichever worktree `repo` was opened from.
///
/// Memoised per process by canonical common directory. The file's size and
/// modification time are checked on every call, so a rewritten file is
/// reparsed rather than served stale.
pub fn repo_config(repo: &Repository) -> Result<Arc<ConfigFile>> {
    type Cache = BTreeMap<PathBuf, (Option<FileStamp>, Arc<ConfigFile>)>;
    static CACHE: Mutex<Cache> = Mutex::new(BTreeMap::new());

    let common_dir = repo.commondir();
    let common_dir = std::fs::canonicalize(common_dir).unwrap_or_else(|_| common_dir.to_path_buf());
    let path = common_dir.join(REPO_CONFIG_FILE);
    let stamp = FileStamp::of(&path);

    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((cached_stamp, config)) = cache.get(&path)
        && *cached_stamp == stamp
    {
        return Ok(Arc::clone(config));
    }

    let config = Arc::new(ConfigFile::load("repository", path.clone())?);
    config.warn_unknown_keys(KNOWN_REPO_KEYS);
    cache.insert(path, (stamp, Arc::clone(&config)));
    Ok(config)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl FileStamp {
    fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

/// Where the global config lives: `kindra/config.toml` under
/// `dirs::config_dir()`.
pub fn global_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("kindra").join("config.toml"))
}

fn global_config() -> Result<Option<ConfigFile>> {
    global_config_path()
        .map(|path| ConfigFile::load("global", path))
        .transpose()
}

#[derive(Deserialize)]
struct RestackSection {
    history_limit: Option<usize>,
}

#[derive(Deserialize)]
struct RebaseSection {
    autostash: Option<bool>,
}

/// The first value `pick` finds in repository config, then global config.
fn layered<T>(
    repo: &Repository,
    pick: impl Fn(&ConfigFile) -> Result<Option<T>>,
) -> Result<Option<T>> {
    if let Some(value) = pick(&*repo_config(repo)?)? {
        return Ok(Some(value));
    }
    match global_config()? {
        Some(global) => pick(&global),
        None => Ok(None),
    }
}

/// How far back restack searches for rewritten bases: the CLI flag, then
/// `[restack] history_limit` from repository then global config, then
/// [`DEFAULT_RESTACK_HISTORY_LIMIT`].
pub fn restack_history_limit(repo: &Repository, cli_override: Option<usize>) -> Result<usize> {
    if let Some(limit) = cli_override {
        return Ok(limit);
    }
    let configured = layered(repo, |file| {
        Ok(file
            .section::<RestackSection>("restack")?
            .and_then(|section| section.history_limit))
    })?;
    Ok(configured.unwrap_or(DEFAULT_RESTACK_HISTORY_LIMIT))
}

/// Whether rebase-style commands autostash: the CLI flag, then
/// `[rebase] autostash` from repository then global config, then Git's own
/// `rebase.autostash`, then off.
pub fn rebase_autostash(repo: &Repository, cli_override: Option<bool>) -> Result<bool> {
    if let Some(autostash) = cli_override {
        return Ok(autostash);
    }
    let configured = layered(repo, |file| {
        Ok(file
            .section::<RebaseSection>("rebase")?
            .and_then(|section| section.autostash))
    })?;
    if let Some(autostash) = configured {
        return Ok(autostash);
    }
    if let Ok(autostash) = repo.config()?.get_bool("rebase.autostash") {
        return Ok(autostash);
    }
    Ok(false)
}

/// The configured trunk name (`upstream_branch`), trimmed, or `None` when it
/// is absent or blank. Resolve it to a branch with [`crate::trunk`].
pub(crate) fn configured_trunk(repo: &Repository) -> Result<Option<(String, PathBuf)>> {
    let config = repo_config(repo)?;
    let name = config
        .section::<String>("upstream_branch")?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    Ok(name.map(|name| (name, config.path().to_path_buf())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn rewritten_config_is_reparsed() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let path = repo.commondir().join(REPO_CONFIG_FILE);

        std::fs::write(&path, "[restack]\nhistory_limit = 5\n").unwrap();
        assert_eq!(restack_history_limit(&repo, None).unwrap(), 5);

        std::fs::write(&path, "[restack]\nhistory_limit = 12345\n").unwrap();
        assert_eq!(restack_history_limit(&repo, None).unwrap(), 12345);

        std::fs::remove_file(&path).unwrap();
        assert!(
            repo_config(&repo)
                .unwrap()
                .section::<toml::Table>("restack")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn section_type_error_names_key_and_path() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        std::fs::write(
            repo.commondir().join(REPO_CONFIG_FILE),
            "[rebase]\nautostash = \"yes\"\n",
        )
        .unwrap();

        let err = rebase_autostash(&repo, None).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("Invalid `rebase` in repository config at"),
            "{message}"
        );
        assert!(message.contains(REPO_CONFIG_FILE), "{message}");
        // Other sections are unaffected.
        let config = repo_config(&repo).unwrap();
        assert!(
            config
                .section::<RestackSection>("restack")
                .unwrap()
                .is_none()
        );
    }
}
