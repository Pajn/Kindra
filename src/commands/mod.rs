pub mod abort_cmd;
pub mod absorb_cmd;
pub mod checkout;
pub mod commit;
pub mod continue_cmd;
pub mod move_cmd;
pub mod pr;
pub(crate) mod pr_merge;
pub mod push;
pub mod rename;
pub mod reorder;
pub mod restack;
pub mod run;
pub mod shell_init;
pub mod split;
pub mod status_cmd;
pub mod sync;
pub mod tree;
pub mod worktree;

use crate::interaction::{Interaction, current as interaction, input_required};
use anyhow::{Context, Result, anyhow};
use clap::Subcommand;
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use git2::{BranchType, Repository};
use serde::Deserialize;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::PathBuf;

/// What a prompt helper should do when it cannot ask (non-interactive mode).
///
/// Every prompt call site must state this explicitly, so the behavior of a
/// headless run is visible and reviewable at the point of the prompt rather than
/// hidden in a shared default.
pub enum Fallback<T> {
    /// No sensible unattended answer: fail loudly with this hint. Maps to a
    /// distinct exit code so callers can tell missing-input from real failure.
    Require(&'static str),
    /// Use this value when we cannot ask.
    Default(T),
}

#[derive(Subcommand, Clone, Copy)]
pub enum CheckoutSubcommand {
    /// Checkout the branch above the current one
    Up,
    /// Checkout the branch below the current one
    Down,
    /// Checkout the top branch in the stack
    Top,
}

pub struct CommitInfo {
    pub id: String,
    pub summary: String,
}

pub fn local_branch_completer() -> ArgValueCompleter {
    ArgValueCompleter::new(local_branch_candidates)
}

fn local_branch_candidates(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let Ok(repo) = Repository::discover(".") else {
        return Vec::new();
    };
    let Ok(branches) = repo.branches(Some(BranchType::Local)) else {
        return Vec::new();
    };

    let mut candidates = branches
        .filter_map(|branch| {
            let (branch, _) = branch.ok()?;
            let name = branch.name().ok()??;
            name.starts_with(current)
                .then(|| CompletionCandidate::new(name).help(Some("local branch".into())))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.get_value().cmp(right.get_value()));
    candidates
}

pub fn fixup_commit_completer() -> ArgValueCompleter {
    ArgValueCompleter::new(fixup_commit_candidates)
}

/// Suggest the commits in the current stack for `kin commit --fixup <sha>` — the
/// same set `resolve_fixup_commit` accepts as targets — offering the abbreviated
/// SHA as the value and the commit subject as help. Stack discovery is delegated
/// to [`crate::stack::enumerate_current_stack_commits`]. Best-effort: any failure
/// to resolve the stack yields no suggestions rather than an error.
fn fixup_commit_candidates(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let Ok(repo) = Repository::discover(".") else {
        return Vec::new();
    };
    let Ok(commits) = crate::stack::enumerate_current_stack_commits(&repo) else {
        return Vec::new();
    };
    commits
        .iter()
        .filter_map(|commit| {
            let short: String = commit.commit_id.to_string().chars().take(12).collect();
            short.starts_with(current).then(|| {
                let subject = commit.message.lines().next().unwrap_or("").to_string();
                CompletionCandidate::new(short).help(Some(subject.into()))
            })
        })
        .collect()
}

/// Prompt the user to pick one of `options`.
///
/// A single option is always unambiguous and returned directly (it consumes no
/// scripted index). Otherwise behavior depends on the resolved interaction mode:
/// interactive prompts, scripted uses the next test index, and non-interactive
/// applies `fallback` — either a caller-supplied default or a hard error.
pub fn prompt_select(
    message: &str,
    options: Vec<String>,
    fallback: Fallback<String>,
) -> Result<String> {
    if options.is_empty() {
        return Err(anyhow!("No options available for selection"));
    }

    if let Interaction::Interactive = interaction() {
        return inquire::Select::new(message, options)
            .prompt()
            .context("Selection failed");
    }

    // A single option is unambiguous: selecting it is deterministic and safe.
    if options.len() == 1 {
        println!(
            "{} (only one option available, selecting: {})",
            message, options[0]
        );
        return Ok(options[0].clone());
    }

    if let Some(scripted) = interaction().scripted()
        && let Some(idx) = scripted.next_selection()
        && idx < options.len()
    {
        println!("Options:");
        for (i, opt) in options.iter().enumerate() {
            println!("{}: {}", i, opt);
        }
        println!(
            "{} (test override: auto-selecting option {})",
            message, options[idx]
        );
        return Ok(options[idx].clone());
    }

    match fallback {
        Fallback::Default(value) => Ok(value),
        Fallback::Require(hint) => Err(input_required(format!(
            "Cannot choose between {} options without a terminal: {} {}",
            options.len(),
            message,
            hint
        ))),
    }
}

/// Prompt the user to pick zero or more of `options`.
///
/// Non-interactive runs apply `fallback` (label/reviewer prompts pass an empty
/// default, since selecting none is benign).
pub fn prompt_multi_select<T: std::fmt::Display + Clone>(
    message: &str,
    options: Vec<T>,
    fallback: Fallback<Vec<T>>,
) -> Result<Vec<T>> {
    if let Interaction::Interactive = interaction() {
        return inquire::MultiSelect::new(message, options)
            .prompt()
            .context("Multi-selection failed");
    }

    if let Some(scripted) = interaction().scripted() {
        let selected = scripted
            .multi_selections()
            .iter()
            .filter_map(|&idx| options.get(idx).cloned())
            .collect::<Vec<_>>();

        println!("Options:");
        for (i, opt) in options.iter().enumerate() {
            println!("{}: {}", i, opt);
        }
        println!(
            "{} (test override: auto-selecting {} option(s))",
            message,
            selected.len()
        );
        return Ok(selected);
    }

    match fallback {
        Fallback::Default(value) => Ok(value),
        Fallback::Require(hint) => Err(input_required(format!("{message} {hint}"))),
    }
}

/// Prompt for a yes/no confirmation.
///
/// Under `--yes` the action is accepted (`true`). Otherwise non-interactive runs
/// apply `fallback`.
pub fn prompt_confirm(message: &str, fallback: Fallback<bool>) -> Result<bool> {
    match interaction() {
        Interaction::Interactive => inquire::Confirm::new(message)
            .with_default(false)
            .prompt()
            .context("Confirmation failed"),
        mode if mode.assume_yes() => {
            println!("{message} (--yes: accepting)");
            Ok(true)
        }
        _ => match fallback {
            Fallback::Default(value) => {
                println!(
                    "{message} (non-interactive: {})",
                    if value { "accepting" } else { "declining" }
                );
                Ok(value)
            }
            Fallback::Require(hint) => Err(input_required(format!("{message} {hint}"))),
        },
    }
}

pub fn find_upstream(repo: &Repository) -> Result<Option<String>> {
    if let Some(upstream) = read_repo_upstream_override(repo)? {
        return Ok(Some(upstream));
    }

    let mut candidates = Vec::new();
    if let Ok(default_branch) = repo.config()?.get_string("init.defaultBranch") {
        let default_branch = default_branch.trim();
        if !default_branch.is_empty() {
            candidates.push(default_branch.to_string());
        }
    }
    candidates.extend(["main", "master", "trunk"].iter().map(|s| s.to_string()));

    let mut seen = HashSet::new();
    candidates.retain(|candidate| seen.insert(candidate.clone()));

    for name in &candidates {
        if repo.find_branch(name, BranchType::Local).is_ok() {
            return Ok(Some(name.clone()));
        }
    }

    let mut remote_candidates = Vec::new();
    for name in &candidates {
        if !name.starts_with("origin/") {
            remote_candidates.push(format!("origin/{name}"));
        }
    }

    for name in remote_candidates {
        if branch_exists(repo, &name) {
            return Ok(Some(name));
        }
    }

    Ok(None)
}

/// The local branch name a base/upstream ref would shadow. [`find_upstream`] may
/// return a remote-qualified ref (e.g. `origin/main`) when the base lives only on
/// a remote; a new local `main` would then hijack the stack base. Strip a leading
/// `<remote>/` only when `<remote>` is a real configured remote, so an ordinary
/// branch like `feature/x` is left intact. Single source of truth for that
/// reduction so callers don't each open-code a subtly different variant.
pub(crate) fn base_short_name(repo: &Repository, upstream: &str) -> String {
    upstream
        .split_once('/')
        .filter(|(remote, _)| repo.find_remote(remote).is_ok())
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_else(|| upstream.to_string())
}

/// Branch names that must never be the destination of a push (or the target of a
/// remote delete) driven from a differently-named local branch.
///
/// Deliberately wider than "the trunk [`find_upstream`] resolved to":
///
/// - `find_upstream` returns the *first* candidate that exists, so in a repo that
///   has both `main` and `master` only one would be protected — overwriting the
///   other is just as destructive.
/// - The resolved base is included both as-is and reduced through
///   [`base_short_name`]. A refspec destination is always a bare branch name while
///   `find_upstream` may return `origin/main`, but `base_short_name` reduces on
///   "first segment names a configured remote", which mangles a base like
///   `release/2024` in a repo that also has a remote named `release`. Keeping both
///   spellings means the comparison cannot miss on that mismatch.
///
/// Listing candidates that do not exist in this repo costs nothing: a branch can
/// only track a ref that exists, so an absent name is never a push destination.
pub(crate) fn protected_push_targets(repo: &Repository) -> Result<Vec<String>> {
    let mut protected: Vec<String> = Vec::new();
    let add = |protected: &mut Vec<String>, name: String| {
        if !name.is_empty() && !protected.contains(&name) {
            protected.push(name);
        }
    };

    if let Some(upstream) = find_upstream(repo)? {
        add(&mut protected, base_short_name(repo, &upstream));
        add(&mut protected, upstream);
    }
    if let Ok(default_branch) = repo.config()?.get_string("init.defaultBranch") {
        add(&mut protected, default_branch.trim().to_string());
    }
    for name in ["main", "master", "trunk"] {
        add(&mut protected, name.to_string());
    }

    Ok(protected)
}

/// If acting on `remote_ref` — a push destination or a remote delete target,
/// derived from `local_name`'s upstream — would hit a protected base branch under
/// a different name, return that base's name.
///
/// Git's default `branch.autoSetupMerge=true` makes `git switch -c feature
/// origin/main` set `branch.feature.merge = refs/heads/main`, so anything that
/// reads a branch's upstream to find "its" remote branch gets `main` back. For a
/// push that means `feature:main`, which force-updates the base; for a delete it
/// means deleting the base on the remote. Neither is ever intended.
///
/// The comparison is on branch names only, ignoring which remote they live on.
/// That errs toward refusing (a branch tracking `fork/main` is refused even though
/// it would only touch the user's fork), which is the safe direction: every way
/// this check can be wrong should be a false refusal, never a missed one. Acting
/// on a base while standing on it (`local_name == remote_ref`) is legitimate.
pub(crate) fn foreign_base_target<'a>(
    local_name: &str,
    remote_ref: &str,
    protected: &'a [String],
) -> Option<&'a str> {
    if local_name == remote_ref {
        return None;
    }
    protected
        .iter()
        .find(|name| name.as_str() == remote_ref)
        .map(String::as_str)
}

fn branch_exists(repo: &Repository, name: &str) -> bool {
    repo.find_branch(name, BranchType::Local).is_ok()
        || repo.find_branch(name, BranchType::Remote).is_ok()
}

fn resolve_branch_name(repo: &Repository, name: &str) -> Option<String> {
    if branch_exists(repo, name) {
        return Some(name.to_string());
    }

    if !name.starts_with("origin/") {
        let origin_name = format!("origin/{name}");
        if branch_exists(repo, &origin_name) {
            return Some(origin_name);
        }
    }

    None
}

#[derive(Deserialize)]
struct RepoConfig {
    upstream_branch: Option<String>,
    restack: Option<RestackConfig>,
    rebase: Option<RebaseConfig>,
}

#[derive(Deserialize, Clone, Copy)]
struct RestackConfig {
    history_limit: Option<usize>,
}

#[derive(Deserialize, Clone, Copy)]
struct RebaseConfig {
    autostash: Option<bool>,
}

#[derive(Deserialize)]
struct GlobalConfig {
    restack: Option<RestackConfig>,
    rebase: Option<RebaseConfig>,
}

pub const DEFAULT_RESTACK_HISTORY_LIMIT: usize = 100;

pub fn resolve_restack_history_limit(
    repo: &Repository,
    cli_override: Option<usize>,
) -> Result<usize> {
    if let Some(limit) = cli_override {
        return Ok(limit);
    }

    if let Some(limit) = read_repo_config(repo)?
        .restack
        .and_then(|cfg| cfg.history_limit)
    {
        return Ok(limit);
    }

    if let Some(limit) =
        read_global_config()?.and_then(|cfg| cfg.restack.and_then(|r| r.history_limit))
    {
        return Ok(limit);
    }

    Ok(DEFAULT_RESTACK_HISTORY_LIMIT)
}

/// Fold the mutually-exclusive `--autostash` / `--no-autostash` CLI flags into a
/// single override for [`resolve_rebase_autostash`]: `Some(true)` / `Some(false)`
/// when one is set, `None` to fall back to config. Centralized so the many
/// command call sites can't drift on the flag→override mapping.
pub fn autostash_override(autostash: bool, no_autostash: bool) -> Option<bool> {
    if autostash {
        Some(true)
    } else if no_autostash {
        Some(false)
    } else {
        None
    }
}

/// Resolve the effective autostash setting for a rebase-style command and then
/// enforce the clean-or-autostash contract up front, returning the resolved
/// flag. Combines the [`autostash_override`] → [`resolve_rebase_autostash`] →
/// [`crate::rebase_utils::ensure_rebase_working_tree`] sequence every rebase
/// command shares, so a dirty tree with `--no-autostash` fails fast and
/// identically everywhere.
pub fn resolve_and_check_autostash(
    repo: &Repository,
    autostash: bool,
    no_autostash: bool,
) -> Result<bool> {
    let resolved = resolve_rebase_autostash(repo, autostash_override(autostash, no_autostash))?;
    crate::rebase_utils::ensure_rebase_working_tree(repo, resolved)?;
    Ok(resolved)
}

pub fn resolve_rebase_autostash(repo: &Repository, cli_override: Option<bool>) -> Result<bool> {
    if let Some(autostash) = cli_override {
        return Ok(autostash);
    }

    if let Some(autostash) = read_repo_config(repo)?.rebase.and_then(|cfg| cfg.autostash) {
        return Ok(autostash);
    }

    if let Some(autostash) =
        read_global_config()?.and_then(|cfg| cfg.rebase.and_then(|r| r.autostash))
    {
        return Ok(autostash);
    }

    // Check git's native rebase.autostash config
    if let Ok(autostash) = repo.config()?.get_bool("rebase.autostash") {
        return Ok(autostash);
    }

    Ok(false)
}

/// Turn a commit-message subject into a git-safe branch slug: ASCII-lowercased,
/// with every run of non-alphanumeric characters collapsed to a single `-`,
/// trimmed of leading/trailing `-`, and capped in length. Returns `None` when
/// nothing usable remains (an empty or all-punctuation subject) so callers can
/// fall back to requiring an explicit name.
pub fn slugify_subject(subject: &str) -> Option<String> {
    const MAX_LEN: usize = 50;

    let mut slug = String::new();
    let mut pending_separator = false;
    for ch in subject.chars() {
        if ch.is_ascii_alphanumeric() {
            // Only emit a separator once we know a real character follows, so
            // leading punctuation never produces a leading `-`.
            if pending_separator && !slug.is_empty() {
                slug.push('-');
            }
            pending_separator = false;
            slug.push(ch.to_ascii_lowercase());
        } else {
            pending_separator = true;
        }
    }

    // The slug is pure ASCII, so truncating on a byte boundary is safe; drop any
    // separator the cut leaves dangling at the end.
    if slug.len() > MAX_LEN {
        slug.truncate(MAX_LEN);
        while slug.ends_with('-') {
            slug.pop();
        }
    }

    if slug.is_empty() { None } else { Some(slug) }
}

/// Return the first of `base`, `base-2`, `base-3`, … that is neither an existing
/// local branch nor already present in `taken` (names claimed earlier in the
/// same operation). Errors if the chosen name is not a valid git branch name.
pub fn disambiguate_branch_name(
    repo: &Repository,
    base: &str,
    taken: &HashSet<String>,
) -> Result<String> {
    let is_taken = |name: &str| -> Result<bool> {
        if taken.contains(name) {
            return Ok(true);
        }
        match repo.find_branch(name, BranchType::Local) {
            Ok(_) => Ok(true),
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(false),
            Err(e) => Err(anyhow!(e)),
        }
    };

    let mut candidate = base.to_string();
    let mut suffix = 2;
    while is_taken(&candidate)? {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }

    if !git2::Branch::name_is_valid(&candidate)? {
        return Err(anyhow!(
            "Derived branch name '{candidate}' is not a valid git branch name; pass a name explicitly."
        ));
    }
    Ok(candidate)
}

fn read_repo_upstream_override(repo: &Repository) -> Result<Option<String>> {
    let cfg = read_repo_config(repo)?;

    let upstream = cfg
        .upstream_branch
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    match upstream {
        Some(upstream) => resolve_branch_name(repo, &upstream)
            .map(Some)
            .ok_or_else(|| {
                anyhow!(
                    "Configured upstream branch '{}' in .git/kindra.toml was not found",
                    upstream
                )
            }),
        None => Ok(None),
    }
}

fn read_repo_config(repo: &Repository) -> Result<RepoConfig> {
    read_toml_config(repo.path().join("kindra.toml"), "repository")?.map_or_else(
        || {
            Ok(RepoConfig {
                upstream_branch: None,
                restack: None,
                rebase: None,
            })
        },
        Ok,
    )
}

fn read_global_config() -> Result<Option<GlobalConfig>> {
    let Some(config_path) = global_config_path() else {
        return Ok(None);
    };
    read_toml_config(config_path, "global")
}

fn read_toml_config<T: for<'de> Deserialize<'de>>(
    config_path: PathBuf,
    config_kind: &str,
) -> Result<Option<T>> {
    if !config_path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&config_path).with_context(|| {
        format!(
            "Failed to read {config_kind} config at {}",
            config_path.display()
        )
    })?;
    let cfg = toml::from_str(&raw).with_context(|| {
        format!(
            "Failed to parse {config_kind} config at {}",
            config_path.display()
        )
    })?;
    Ok(Some(cfg))
}

fn global_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("kindra").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::{foreign_base_target, slugify_subject};

    #[test]
    fn slugify_basic() {
        assert_eq!(
            slugify_subject("Add cool parser").as_deref(),
            Some("add-cool-parser")
        );
    }

    #[test]
    fn slugify_collapses_and_trims_punctuation() {
        assert_eq!(
            slugify_subject("  Fix: the (null) deref!!  ").as_deref(),
            Some("fix-the-null-deref")
        );
    }

    #[test]
    fn slugify_drops_non_ascii() {
        // Each non-ASCII char is a separator boundary, so accented letters split
        // the surrounding word rather than being transliterated.
        assert_eq!(
            slugify_subject("café déjà vu").as_deref(),
            Some("caf-d-j-vu")
        );
    }

    #[test]
    fn slugify_empty_and_punctuation_only_yield_none() {
        assert_eq!(slugify_subject(""), None);
        assert_eq!(slugify_subject("   "), None);
        assert_eq!(slugify_subject("!!! ---"), None);
    }

    #[test]
    fn slugify_truncates_without_trailing_dash() {
        let long = "a".repeat(60);
        let slug = slugify_subject(&long).unwrap();
        assert_eq!(slug.len(), 50);

        // A cut landing on a separator must not leave a dangling '-'.
        let subject = format!("{} more words here", "word ".repeat(10));
        let slug = slugify_subject(&subject).unwrap();
        assert!(!slug.ends_with('-'), "slug ended with '-': {slug}");
        assert!(slug.len() <= 50);
    }

    #[test]
    fn foreign_base_target_flags_only_a_differently_named_branch() {
        let protected = vec!["main".to_string(), "master".to_string()];

        // The incident: a branch tracking the base, so its push destination is
        // the base.
        assert_eq!(
            foreign_base_target("feature", "main", &protected),
            Some("main")
        );
        assert_eq!(
            foreign_base_target("ci/checks-frontend-runners", "main", &protected),
            Some("main")
        );

        // A sibling long-lived branch is protected too, not just the one
        // `find_upstream` happens to resolve to first.
        assert_eq!(
            foreign_base_target("feature", "master", &protected),
            Some("master")
        );

        // Standing on the base and pushing it is legitimate.
        assert_eq!(foreign_base_target("main", "main", &protected), None);

        // An ordinary branch pushing to its own remote branch.
        assert_eq!(foreign_base_target("feature", "feature", &protected), None);

        // A branch that merely looks base-ish is not a base.
        assert_eq!(foreign_base_target("feature", "mainline", &protected), None);

        // A base whose name contains a slash is matched whole, so a repo with a
        // remote named `release` cannot disarm the check.
        let slashed = vec!["release/2024".to_string()];
        assert_eq!(
            foreign_base_target("feature", "release/2024", &slashed),
            Some("release/2024")
        );

        // Nothing protected: nothing to flag.
        assert_eq!(foreign_base_target("feature", "main", &[]), None);
    }
}
