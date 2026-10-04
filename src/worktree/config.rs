use crate::worktree::path_resolver::{expand_path_template, normalize_path, temp_template_root};
use anyhow::{Result, anyhow};
use git2::{BranchType, Repository};
use serde::Deserialize;
use std::path::{Path, PathBuf};

const DEFAULT_ROOT: &str = ".git/kindra-worktrees";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookListConfig {
    pub on_create: Vec<String>,
    pub on_checkout: Vec<String>,
    pub on_remove: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MainWorktreeConfig {
    pub enabled: bool,
    pub branch: String,
    pub path: PathBuf,
    pub allow_branch_switch: bool,
    pub hooks: HookListConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewWorktreeConfig {
    pub enabled: bool,
    pub path: PathBuf,
    pub reuse: bool,
    pub clean_before_switch: bool,
    pub hooks: HookListConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TempWorktreeConfig {
    pub enabled: bool,
    pub path_template: PathBuf,
    pub delete_merged: bool,
    pub hooks: HookListConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupWorktreeConfig {
    /// Recognize Git worktrees at or beneath this directory. Never create here.
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeConfig {
    /// The repository config file these settings come from, for messages that
    /// point the user at it.
    pub config_path: PathBuf,
    pub root: PathBuf,
    pub trunk: String,
    pub hooks: HookListConfig,
    pub main: MainWorktreeConfig,
    pub review: ReviewWorktreeConfig,
    pub temp: TempWorktreeConfig,
    pub cleanup: Vec<CleanupWorktreeConfig>,
    /// Default `{branch}` template for `kin wt add`. Unlike the role paths this is
    /// deliberately *not* constrained under `root` — added worktrees default to a
    /// visible sibling directory when possible, fall back to `<repo>/worktrees`
    /// when the repo has no parent directory, and `add` also accepts an explicit
    /// path that overrides it.
    pub add_path_template: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorktreeConfig {
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    trunk: Option<String>,
    #[serde(default)]
    hooks: Option<RawHookListConfig>,
    #[serde(default)]
    main: Option<RawMainWorktreeConfig>,
    #[serde(default)]
    review: Option<RawReviewWorktreeConfig>,
    #[serde(default)]
    temp: Option<RawTempWorktreeConfig>,
    #[serde(default)]
    cleanup: Vec<RawCleanupWorktreeConfig>,
    #[serde(default)]
    add_path_template: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHookListConfig {
    #[serde(default)]
    on_create: Vec<String>,
    #[serde(default)]
    on_checkout: Vec<String>,
    #[serde(default)]
    on_remove: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMainWorktreeConfig {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    allow_branch_switch: Option<bool>,
    #[serde(default)]
    on_create: Vec<String>,
    #[serde(default)]
    on_checkout: Vec<String>,
    #[serde(default)]
    on_remove: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReviewWorktreeConfig {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    reuse: Option<bool>,
    #[serde(default)]
    clean_before_switch: Option<bool>,
    #[serde(default, alias = "setup_on_create")]
    on_create: Vec<String>,
    #[serde(default, alias = "setup_on_checkout")]
    on_checkout: Vec<String>,
    #[serde(default)]
    on_remove: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTempWorktreeConfig {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    path_template: Option<String>,
    #[serde(default)]
    delete_merged: Option<bool>,
    #[serde(default)]
    on_create: Vec<String>,
    #[serde(default)]
    on_checkout: Vec<String>,
    #[serde(default)]
    on_remove: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCleanupWorktreeConfig {
    path: String,
}

pub fn load_worktree_config(repo: &Repository) -> Result<WorktreeConfig> {
    if repo.workdir().is_none() {
        return Err(anyhow!(
            "Kindra worktree management requires a non-bare repository."
        ));
    }
    let config_base = repo.commondir().parent().ok_or_else(|| {
        anyhow!(
            "Failed to determine repository root from '{}'.",
            repo.commondir().display()
        )
    })?;
    let repo_config = crate::config::repo_config(repo)?;
    let raw = repo_config
        .section::<RawWorktreeConfig>("worktrees")?
        .unwrap_or_default();
    let root = resolve_config_path(config_base, raw.root.as_deref().unwrap_or(DEFAULT_ROOT));
    let default_main_path = root.join("main");
    let default_review_path = root.join("review");
    let default_temp_template = root.join("temp").join("{branch}");
    // `kin wt add` defaults to a sibling directory next to the repo (e.g.
    // `../<repo>-worktrees/{branch}`), or `<repo>/worktrees/{branch}` when the
    // repo has no parent directory, so added worktrees are visible to editors
    // and `git status`, unlike the role worktrees tucked under `.git`.
    let default_add_template = {
        let repo_name = config_base
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());
        let siblings = config_base
            .parent()
            .map(|parent| parent.join(format!("{repo_name}-worktrees")))
            .unwrap_or_else(|| config_base.join("worktrees"));
        siblings.join("{branch}")
    };

    // An explicit `worktrees.trunk` is used as written; otherwise the resolved
    // trunk (which honours `upstream_branch`).
    let trunk = match raw
        .trunk
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        Some(trunk) => trunk,
        None => crate::trunk::resolve_trunk(repo)?.unwrap_or_else(|| "main".to_string()),
    };

    let hooks = raw.hooks.map(hook_list).unwrap_or_default();

    let main_raw = raw.main.unwrap_or_default();
    let main_branch = main_raw
        .branch
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default_main_branch(repo, &trunk));
    let main = MainWorktreeConfig {
        enabled: main_raw.enabled.unwrap_or(true),
        branch: main_branch,
        path: main_raw
            .path
            .map(|value| resolve_config_path(config_base, &value))
            .unwrap_or_else(|| normalize_path(default_main_path.clone())),
        allow_branch_switch: main_raw.allow_branch_switch.unwrap_or(false),
        hooks: role_hook_list(main_raw.on_create, main_raw.on_checkout, main_raw.on_remove),
    };

    let review_raw = raw.review.unwrap_or_default();
    let review = ReviewWorktreeConfig {
        enabled: review_raw.enabled.unwrap_or(true),
        path: review_raw
            .path
            .map(|value| resolve_config_path(config_base, &value))
            .unwrap_or_else(|| normalize_path(default_review_path.clone())),
        reuse: review_raw.reuse.unwrap_or(true),
        clean_before_switch: review_raw.clean_before_switch.unwrap_or(true),
        hooks: role_hook_list(
            review_raw.on_create,
            review_raw.on_checkout,
            review_raw.on_remove,
        ),
    };

    let temp_raw = raw.temp.unwrap_or_default();
    let temp = TempWorktreeConfig {
        enabled: temp_raw.enabled.unwrap_or(true),
        path_template: temp_raw
            .path_template
            .map(|value| resolve_config_path(config_base, &value))
            .unwrap_or_else(|| normalize_path(default_temp_template.clone())),
        delete_merged: temp_raw.delete_merged.unwrap_or(true),
        hooks: role_hook_list(temp_raw.on_create, temp_raw.on_checkout, temp_raw.on_remove),
    };

    let add_path_template = raw
        .add_path_template
        .map(|value| resolve_config_path(config_base, &value))
        .unwrap_or_else(|| normalize_path(default_add_template));

    let cleanup = raw
        .cleanup
        .into_iter()
        .map(|entry| {
            if entry.path.trim().is_empty() {
                return Err(anyhow!("worktrees.cleanup.path must not be empty."));
            }
            // Unlike creation paths, cleanup-only directories can live outside root:
            // only this repository's registered Git worktrees are ever considered.
            Ok(CleanupWorktreeConfig {
                path: resolve_cleanup_path(config_base, &entry.path),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let config = WorktreeConfig {
        config_path: repo_config.path().to_path_buf(),
        root: normalize_path(root),
        trunk,
        hooks,
        main,
        review,
        temp,
        cleanup,
        add_path_template,
    };
    validate_config(&config)?;
    Ok(config)
}

fn resolve_config_path(base: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        normalize_path(path)
    } else {
        normalize_path(base.join(path))
    }
}

fn resolve_cleanup_path(base: &Path, value: &str) -> PathBuf {
    canonical_worktree_path(&resolve_config_path(base, value))
}

/// Match Git’s canonical paths even when only an ancestor exists on disk.
pub(crate) fn canonical_worktree_path(path: &Path) -> PathBuf {
    let path = normalize_path(path);
    // Git reports canonical worktree paths. Resolve aliases such as /tmp on
    // macOS, including when the configured directory has not been created yet.
    for ancestor in path.ancestors() {
        if let Ok(canonical) = std::fs::canonicalize(ancestor)
            && let Ok(suffix) = path.strip_prefix(ancestor)
        {
            return normalize_path(canonical.join(suffix));
        }
    }
    path
}

fn validate_config(config: &WorktreeConfig) -> Result<()> {
    if config.main.enabled && config.main.branch.trim().is_empty() {
        return Err(anyhow!(
            "Configured worktree main branch must be non-empty when main worktrees are enabled."
        ));
    }

    if config.main.enabled && config.main.allow_branch_switch {
        return Err(anyhow!(
            "worktrees.main.allow_branch_switch = true is not supported in the current MVP."
        ));
    }

    if config.main.enabled && config.review.enabled && config.main.path == config.review.path {
        return Err(anyhow!(
            "Configured main and review worktree paths must not be the same."
        ));
    }

    let temp_root = if config.temp.enabled {
        Some(temp_template_root(&config.temp.path_template)?)
    } else {
        None
    };

    if (config.main.enabled && !path_is_inside_repo(&config.root, &config.main.path))
        || (config.review.enabled && !path_is_inside_repo(&config.root, &config.review.path))
        || (config.temp.enabled && !path_is_inside_repo(&config.root, temp_root.as_ref().unwrap()))
    {
        return Err(anyhow!(
            "Configured main/review/temp worktree paths must live under the managed worktree root '{}'.",
            config.root.display()
        ));
    }

    if config.temp.enabled {
        expand_path_template(&config.temp.path_template, "validation-branch")?;
        // `{branch}` must be the final path component. Temp worktrees are
        // recognized by matching a live path against the template root (everything
        // before `{branch}`) on component boundaries, so the placeholder embedded
        // inside a component (`.../temp-{branch}`) or followed by more components
        // (`.../temp/{branch}/nested`) can't be classified reliably.
        let branch_is_trailing_component = config
            .temp
            .path_template
            .components()
            .next_back()
            .and_then(|component| component.as_os_str().to_str())
            == Some("{branch}");
        if !branch_is_trailing_component {
            return Err(anyhow!(
                "worktrees.temp.path_template must end with `{{branch}}` as its final path component \
                 (e.g. `.../temp/{{branch}}`); `{}` does not.",
                config.temp.path_template.display()
            ));
        }
    }
    // The add template is unconstrained in location but must still be branch-
    // templated so `kin wt add` can derive a distinct path per branch.
    expand_path_template(&config.add_path_template, "validation-branch")?;
    Ok(())
}

fn default_main_branch(repo: &Repository, trunk: &str) -> String {
    let trimmed = trunk.trim();
    if trimmed.is_empty() {
        return "main".to_string();
    }

    if let Some(local) = trimmed.strip_prefix("refs/heads/") {
        return local.to_string();
    }

    if repo.find_branch(trimmed, BranchType::Local).is_ok() {
        return trimmed.to_string();
    }

    if let Some(remote_ref) = trimmed.strip_prefix("refs/remotes/")
        && let Some((_, branch)) = remote_ref.split_once('/')
    {
        return branch.to_string();
    }

    if repo.find_branch(trimmed, BranchType::Remote).is_ok()
        && let Some((_, branch)) = trimmed.split_once('/')
    {
        return branch.to_string();
    }

    trimmed.to_string()
}

fn path_is_inside_repo(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
}

fn hook_list(raw: RawHookListConfig) -> HookListConfig {
    HookListConfig {
        on_create: raw.on_create,
        on_checkout: raw.on_checkout,
        on_remove: raw.on_remove,
    }
}

fn role_hook_list(
    on_create: Vec<String>,
    on_checkout: Vec<String>,
    on_remove: Vec<String>,
) -> HookListConfig {
    HookListConfig {
        on_create,
        on_checkout,
        on_remove,
    }
}

#[cfg(test)]
mod tests {
    use super::load_worktree_config;
    use tempfile::TempDir;

    #[test]
    fn uses_defaults_when_config_missing() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let config = load_worktree_config(&repo).unwrap();
        assert!(config.main.path.ends_with(".git/kindra-worktrees/main"));
        assert!(config.review.path.ends_with(".git/kindra-worktrees/review"));
        assert!(
            config
                .temp
                .path_template
                .ends_with(".git/kindra-worktrees/temp/{branch}")
        );
    }

    fn repo_with_branch(dir: &TempDir, refname: &str) -> git2::Repository {
        let repo = git2::Repository::init(dir.path()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some(refname), &sig, &sig, "initial", &tree, &[])
            .unwrap();
        drop(tree);
        repo
    }

    #[test]
    fn default_trunk_is_the_resolved_upstream_branch() {
        let dir = TempDir::new().unwrap();
        // Only a remote-tracking `origin/dev` exists; `upstream_branch` resolves
        // to it, and the main worktree pins the local name.
        let repo = repo_with_branch(&dir, "refs/remotes/origin/dev");
        std::fs::write(
            repo.commondir().join("kindra.toml"),
            "upstream_branch = \" dev \"\n",
        )
        .unwrap();

        let config = load_worktree_config(&repo).unwrap();
        assert_eq!(config.trunk, "origin/dev");
        assert_eq!(config.main.branch, "dev");
    }

    #[test]
    fn default_trunk_rejects_missing_upstream_branch() {
        let dir = TempDir::new().unwrap();
        let repo = repo_with_branch(&dir, "refs/heads/main");
        std::fs::write(
            repo.commondir().join("kindra.toml"),
            "upstream_branch = \"nonexistent\"\n",
        )
        .unwrap();

        let err = load_worktree_config(&repo).unwrap_err();
        assert!(
            err.to_string()
                .contains("Configured upstream branch 'nonexistent'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn explicit_trunk_is_used_as_written() {
        let dir = TempDir::new().unwrap();
        let repo = repo_with_branch(&dir, "refs/heads/main");
        std::fs::write(
            repo.commondir().join("kindra.toml"),
            "upstream_branch = \"main\"\n\n[worktrees]\ntrunk = \"release\"\n",
        )
        .unwrap();

        let config = load_worktree_config(&repo).unwrap();
        assert_eq!(config.trunk, "release");
    }

    #[test]
    fn rejects_invalid_main_switching() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        std::fs::write(
            repo.commondir().join("kindra.toml"),
            "[worktrees.main]\nallow_branch_switch = true\n",
        )
        .unwrap();

        let err = load_worktree_config(&repo).unwrap_err();
        assert!(err.to_string().contains("allow_branch_switch"));
    }

    #[test]
    fn rejects_temp_template_outside_root() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        std::fs::write(
            repo.commondir().join("kindra.toml"),
            "[worktrees]\nroot = \".git/kindra-worktrees\"\n\n[worktrees.temp]\npath_template = \"../outside/{branch}\"\n",
        )
        .unwrap();

        let err = load_worktree_config(&repo).unwrap_err();
        assert!(err.to_string().contains("main/review/temp worktree paths"));
    }

    #[test]
    fn rejects_temp_template_with_branch_not_on_a_component_boundary() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        // `{branch}` embedded inside a component (`temp-{branch}`) can't be
        // classified reliably by component-boundary matching, so it is rejected.
        std::fs::write(
            repo.commondir().join("kindra.toml"),
            "[worktrees]\nroot = \".git/kindra-worktrees\"\n\n[worktrees.temp]\npath_template = \".git/kindra-worktrees/temp-{branch}\"\n",
        )
        .unwrap();

        let err = load_worktree_config(&repo).unwrap_err();
        assert!(
            err.to_string().contains("final path component"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_temp_template_with_branch_not_trailing() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        // `{branch}` is a standalone component but not the last one, so the
        // template root is a parent dir and classification would be too loose.
        std::fs::write(
            repo.commondir().join("kindra.toml"),
            "[worktrees]\nroot = \".git/kindra-worktrees\"\n\n[worktrees.temp]\npath_template = \".git/kindra-worktrees/temp/{branch}/nested\"\n",
        )
        .unwrap();

        let err = load_worktree_config(&repo).unwrap_err();
        assert!(
            err.to_string().contains("final path component"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn disabled_temp_with_invalid_template_still_loads() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        // A disabled temp section carrying an invalid template (no `{branch}`)
        // must not block loading — the temp checks are skipped.
        std::fs::write(
            repo.commondir().join("kindra.toml"),
            "[worktrees.temp]\nenabled = false\npath_template = \"no-placeholder\"\n",
        )
        .unwrap();

        let config = load_worktree_config(&repo).unwrap();
        assert!(!config.temp.enabled);
    }

    #[test]
    fn skips_disabled_role_validation() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        std::fs::write(
            repo.commondir().join("kindra.toml"),
            "[worktrees.main]\nenabled = false\nallow_branch_switch = true\n\n[worktrees.temp]\nenabled = false\npath_template = \"../outside/{branch}\"\n",
        )
        .unwrap();

        let config = load_worktree_config(&repo).unwrap();
        assert!(!config.main.enabled);
        assert!(!config.temp.enabled);
    }
}
