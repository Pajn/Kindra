use crate::commands::find_upstream;
use crate::worktree::path_resolver::{expand_path_template, normalize_path, temp_template_root};
use anyhow::{Context, Result, anyhow};
use git2::{BranchType, Repository};
use serde::Deserialize;
use std::fs;
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
pub struct WorktreeConfig {
    pub root: PathBuf,
    pub trunk: String,
    pub hooks: HookListConfig,
    pub main: MainWorktreeConfig,
    pub review: ReviewWorktreeConfig,
    pub temp: TempWorktreeConfig,
}

#[derive(Debug, Deserialize)]
struct RepoConfigFile {
    #[serde(default)]
    upstream_branch: Option<String>,
    #[serde(default)]
    worktrees: Option<RawWorktreeConfig>,
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
    let cfg = read_repo_config(repo)?;
    let raw = cfg.worktrees.unwrap_or_default();
    let root = resolve_config_path(config_base, raw.root.as_deref().unwrap_or(DEFAULT_ROOT));
    let default_main_path = root.join("main");
    let default_review_path = root.join("review");
    let default_temp_template = root.join("temp").join("{branch}");

    let trunk = match raw
        .trunk
        .or(cfg.upstream_branch)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        Some(trunk) => trunk,
        None => find_upstream(repo)?.unwrap_or_else(|| "main".to_string()),
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

    let config = WorktreeConfig {
        root: normalize_path(root),
        trunk,
        hooks,
        main,
        review,
        temp,
    };
    validate_config(&config)?;
    Ok(config)
}

fn read_repo_config(repo: &Repository) -> Result<RepoConfigFile> {
    let path = repo.commondir().join("kindra.toml");
    if !path.exists() {
        return Ok(RepoConfigFile {
            upstream_branch: None,
            worktrees: None,
        });
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read repository config at {}", path.display()))?;
    toml::from_str(&raw)
        .with_context(|| format!("Failed to parse repository config at {}", path.display()))
}

fn resolve_config_path(base: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        normalize_path(path)
    } else {
        normalize_path(base.join(path))
    }
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
    }
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
