use super::CheckoutSubcommand;
use super::find_upstream;
use crate::gh;
use crate::stack::{
    discover_pr_connected_stack, get_immediate_successors, get_stack_tips, visualize_stack,
};
use anyhow::{Context, Result, anyhow};
use git2::{BranchType, Repository};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn checkout(
    subcommand: &Option<CheckoutSubcommand>,
    branch: &Option<String>,
    all: bool,
) -> Result<()> {
    let repo = crate::open_repo()?;

    if let Some(branch) = branch {
        let lock = crate::state_io::RepoLock::acquire(&repo)?;
        crate::operation_state::ensure_idle(&repo, &lock, crate::operation_state::Allow::NOTHING)?;
        return crate::overrides::with_planned(&repo, false, || {
            checkout_branch_with_pr_hydration(&repo, branch)
        });
    }

    if all && subcommand.is_none() {
        let mut branch_names = Vec::new();
        let local_branches = repo.branches(Some(BranchType::Local))?;
        for res in local_branches {
            let (branch, _) = res?;
            if let Some(name) = branch.name()? {
                branch_names.push(name.to_string());
            }
        }
        branch_names.sort();

        if branch_names.is_empty() {
            println!("No local branches found.");
            return Ok(());
        }

        let selected_name = crate::commands::prompt_select(
            "Select branch to checkout:",
            branch_names,
            crate::commands::Fallback::Require("Run 'git checkout <branch>' instead."),
        )?;
        return perform_git_checkout(&selected_name);
    }

    let upstream_name = find_upstream(&repo)?.ok_or_else(|| {
        anyhow!("Could not find a base branch (init.defaultBranch, main, master, or trunk)")
    })?;
    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let upstream_id = upstream_obj.id();
    let head = repo.head()?;
    let head_id = head.peel_to_commit()?.id();

    let current_branch_name = if !repo.head_detached()? {
        head.shorthand().map(|s| s.to_string())
    } else {
        None
    };

    match subcommand {
        Some(CheckoutSubcommand::Up) => {
            let merge_base = repo.merge_base(upstream_id, head_id)?;
            let branches = crate::stack::get_stack_branches_from_merge_base(
                &repo,
                merge_base,
                head_id,
                upstream_id,
                &upstream_name,
            )?;
            let mut successors = get_immediate_successors(&repo, head_id, &branches)?;
            successors.sort();

            match successors.len() {
                0 => Err(anyhow!("Already at the top of the stack")),
                1 => perform_git_checkout(&successors[0]),
                _ => {
                    let selected = crate::commands::prompt_select(
                        "Multiple branches ahead. Select one:",
                        successors,
                        crate::commands::Fallback::Require("Run 'git checkout <branch>' instead."),
                    )?;
                    perform_git_checkout(&selected)
                }
            }
        }
        Some(CheckoutSubcommand::Down) => {
            let current_name = current_branch_name.ok_or_else(|| anyhow!("Not on a branch"))?;
            let parent_branches =
                find_first_parent_branches_via_git_log(&repo, &upstream_name, &current_name)?;

            match parent_branches.len() {
                0 => perform_git_checkout(&upstream_name),
                1 => perform_git_checkout(&parent_branches[0]),
                _ => {
                    let selected = crate::commands::prompt_select(
                        "Multiple parent branches found. Select one:",
                        parent_branches,
                        crate::commands::Fallback::Require("Run 'git checkout <branch>' instead."),
                    )?;
                    perform_git_checkout(&selected)
                }
            }
        }
        Some(CheckoutSubcommand::Top) => {
            let merge_base = repo.merge_base(upstream_id, head_id)?;
            let branches = crate::stack::get_stack_branches_from_merge_base(
                &repo,
                merge_base,
                head_id,
                upstream_id,
                &upstream_name,
            )?;
            let mut tips = get_stack_tips(&repo, &branches)?;
            tips.sort();
            match tips.len() {
                0 => Err(anyhow!("No branches in stack")),
                1 => perform_git_checkout(&tips[0]),
                _ => {
                    let selected = crate::commands::prompt_select(
                        "Multiple stack tips found. Select one:",
                        tips,
                        crate::commands::Fallback::Require("Run 'git checkout <branch>' instead."),
                    )?;
                    perform_git_checkout(&selected)
                }
            }
        }
        None => {
            let merge_base = repo.merge_base(upstream_id, head_id)?;
            let all_branches = crate::stack::get_stack_branches_from_merge_base(
                &repo,
                merge_base,
                head_id,
                upstream_id,
                &upstream_name,
            )?;

            let visualized = visualize_stack(&repo, &all_branches, current_branch_name.as_deref())?;

            if visualized.is_empty() {
                println!(
                    "No branches found in the current stack (excluding {}). Use --all to see everything.",
                    upstream_name
                );
                return Ok(());
            }

            let options: Vec<String> = visualized.iter().map(|v| v.display_name.clone()).collect();
            let selected_display = crate::commands::prompt_select(
                "Select branch to checkout:",
                options,
                crate::commands::Fallback::Require("Run 'git checkout <branch>' instead."),
            )?;

            let selected_name = visualized
                .iter()
                .find(|v| v.display_name == selected_display)
                .map(|v| v.name.clone())
                .ok_or_else(|| anyhow!("Failed to find selected branch '{}'", selected_display))?;

            perform_git_checkout(&selected_name)
        }
    }
}

/// One step of a checkout hydration's plan, in the spirit of the rebase
/// journal's steps (ADR 0001).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind")]
enum HydrationStep {
    /// Create the local `branch` at the recorded commit `tip`, tracking
    /// `remote_ref`, the remote-tracking branch `tip` was read from when the
    /// plan was made. A retry never reads `remote_ref` again, so a fetch in
    /// between cannot change what is created. A branch that already exists at
    /// `tip` counts as created: creation may have succeeded just before the
    /// cursor was saved.
    CreateBranch {
        branch: String,
        remote_ref: String,
        tip: String,
    },
    /// Check out `branch` and remove the journal. Always the last step.
    Checkout { branch: String },
}

/// How far a hydration has got: every step before `step` is done.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HydrationCursor {
    step: usize,
}

/// A checkout hydration's journal: the branches it creates, in order, the
/// checkout that finishes it, and the position reached. Branches that already
/// existed locally when the plan was made are left alone and are not steps.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct HydrationJournal {
    /// The GitHub repository whose PRs the stack was discovered from.
    repository: String,
    steps: Vec<HydrationStep>,
    cursor: HydrationCursor,
}

/// The format of the hydration journal this Kindra saves in
/// `kindra_checkout_state.json`.
///
/// The file holds `{"version": N, "journal": {...}}`, like the rebase journal
/// (`rebase_utils::JOURNAL_VERSION`), but versioned on its own. Kindra 1.1 and
/// earlier saved a flat journal (`branch`, `repository` and `steps`, each step
/// with a `completed` flag); they cannot parse the envelope and so refuse to
/// continue it rather than misread it. Every Kindra reads every older format
/// and refuses a newer one with advice; bump the version whenever an older
/// Kindra would misread what this one saves.
///
/// - 1: the first envelope. The journal records its plan as `steps` (the
///   branches to create, then the checkout) and its progress as a `cursor`.
///   A flat journal is converted when loaded (see [`LegacyHydration`]).
const HYDRATION_JOURNAL_VERSION: u64 = 1;

/// A hydration journal saved by Kindra 1.1 or earlier.
#[derive(Deserialize)]
struct LegacyHydration {
    branch: String,
    repository: String,
    steps: Vec<LegacyHydrationStep>,
}

#[derive(Deserialize)]
struct LegacyHydrationStep {
    branch: String,
    remote_ref: Option<String>,
    tip: String,
    completed: bool,
}

impl LegacyHydration {
    /// The plan as that Kindra ran it: it skipped every completed step (a
    /// branch that already existed, or one it had created) and created the
    /// rest in order before checking out `branch`. A step left to create
    /// always recorded where it came from.
    fn convert(self) -> Result<HydrationJournal> {
        let mut steps = Vec::new();
        for step in self.steps.into_iter().filter(|step| !step.completed) {
            let remote_ref = step.remote_ref.ok_or_else(|| {
                anyhow!(
                    "branch '{}' is left to create but records no remote-tracking branch",
                    step.branch
                )
            })?;
            steps.push(HydrationStep::CreateBranch {
                branch: step.branch,
                remote_ref,
                tip: step.tip,
            });
        }
        steps.push(HydrationStep::Checkout {
            branch: self.branch,
        });
        Ok(HydrationJournal {
            repository: self.repository,
            steps,
            cursor: HydrationCursor::default(),
        })
    }
}

pub(crate) fn hydration_state_path(repo: &Repository) -> PathBuf {
    crate::operation_state::PersistedOperation::Hydration.path(repo)
}

fn save_hydration(repo: &Repository, journal: &HydrationJournal) -> Result<()> {
    crate::state_io::write_atomic(
        &hydration_state_path(repo),
        &serde_json::to_string_pretty(&serde_json::json!({
            "version": HYDRATION_JOURNAL_VERSION,
            "journal": journal,
        }))?,
    )
}

fn load_hydration(repo: &Repository) -> Result<HydrationJournal> {
    let path = hydration_state_path(repo);
    parse_hydration(&path, &std::fs::read_to_string(&path)?)
}

/// Parse a saved hydration journal. One saved in a newer format, or that does
/// not parse, is reported with how to get past it. `kin abort` never reads
/// the journal, so it always stops a hydration.
fn parse_hydration(path: &Path, json: &str) -> Result<HydrationJournal> {
    let unreadable = |err: &dyn std::fmt::Display| {
        anyhow!(
            "Could not read the checkout hydration saved in {}: {err}. If a newer version of kin \
             saved it, finish it with that version, or run 'kin abort' to stop hydration.",
            path.display()
        )
    };
    let mut saved: serde_json::Value =
        serde_json::from_str(json).map_err(|err| unreadable(&err))?;
    match saved.get("version").map(serde_json::Value::as_u64) {
        None => serde_json::from_value::<LegacyHydration>(saved)
            .map_err(|err| unreadable(&err))?
            .convert()
            .map_err(|err| unreadable(&err)),
        Some(Some(version)) if version <= HYDRATION_JOURNAL_VERSION => {
            match saved.get_mut("journal").map(serde_json::Value::take) {
                Some(journal @ serde_json::Value::Object(_)) => {
                    serde_json::from_value(journal).map_err(|err| unreadable(&err))
                }
                _ => Err(unreadable(&"it has a version but no journal")),
            }
        }
        Some(_) => Err(anyhow!(
            "The checkout hydration saved in {} was saved by a newer version of kin (journal \
             version {}; this kin reads up to version {HYDRATION_JOURNAL_VERSION}). Finish it \
             with that version, or run 'kin abort' to stop hydration.",
            path.display(),
            saved["version"]
        )),
    }
}

fn checkout_branch_with_pr_hydration(repo: &Repository, branch: &str) -> Result<()> {
    fetch_all_remotes(repo)?;
    gh::check_gh()?;

    let upstream_name = find_upstream(repo)?;
    let (repository, prs) = gh::checkout_pr_snapshot()?;
    let (branch, creation_order) =
        discover_pr_connected_stack(repo, branch, upstream_name.as_deref(), &prs)?;
    let mut steps = Vec::new();
    for name in creation_order {
        // An existing local branch keeps its commits and its upstream.
        if repo.find_branch(&name, BranchType::Local).is_ok() {
            continue;
        }
        let remote_ref = resolve_remote_tracking_ref(repo, &name, &repository)?.ok_or_else(|| anyhow!(
            "No remote-tracking branch found for discovered stack branch '{}' in PR repository '{}'.", name, repository
        ))?;
        let tip = repo
            .find_branch(&remote_ref, BranchType::Remote)?
            .get()
            .peel_to_commit()?
            .id();
        steps.push(HydrationStep::CreateBranch {
            branch: name,
            remote_ref,
            tip: tip.to_string(),
        });
    }
    steps.push(HydrationStep::Checkout { branch });
    let journal = HydrationJournal {
        repository,
        steps,
        cursor: HydrationCursor::default(),
    };
    // Persist every source OID before creating any refs. A restart never fetches
    // again or silently switches to a different remote/commit halfway through.
    save_hydration(repo, &journal)?;
    run_hydration(repo, journal)
}

pub(crate) fn continue_hydration(repo: &Repository) -> Result<()> {
    let native = crate::operation_state::native_operation(repo);
    if native != crate::operation_state::NativeOperation::None {
        return Err(anyhow!("{} Then run 'kin continue'.", native.advice()));
    }
    run_hydration(repo, load_hydration(repo)?)
}

/// Run the journal's steps from its cursor, saving the cursor after each
/// branch it creates. A failed step leaves the cursor on it for `kin continue`.
fn run_hydration(repo: &Repository, mut journal: HydrationJournal) -> Result<()> {
    loop {
        let step = journal
            .steps
            .get(journal.cursor.step)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "The saved checkout hydration has no checkout step left to run. Run 'kin abort' to stop hydration."
                )
            })?;
        match step {
            HydrationStep::CreateBranch {
                branch,
                remote_ref,
                tip,
            } => {
                ensure_local_branch_for_checkout(repo, &branch, &remote_ref, &tip)
                    .context("Checkout hydration stopped. Fix the error and run 'kin continue', or 'kin abort' to stop hydration")?;
                journal.cursor.step += 1;
                save_hydration(repo, &journal)?;
            }
            HydrationStep::Checkout { branch } => {
                let mut plan = crate::overrides::Plan::default();
                crate::overrides::prepare(repo, plan.checkout_rev(repo, &branch))?;
                git_checkout(repo, &branch).context(
                    "Checkout hydration stopped. Run 'kin continue' to retry checkout or 'kin abort'",
                )?;
                std::fs::remove_file(hydration_state_path(repo))?;
                return Ok(());
            }
        }
    }
}

pub(crate) fn abort_hydration(repo: &Repository) -> Result<()> {
    // Hydration only adds branches; retaining them avoids deleting edits made
    // during an interruption (including a crash between creation and checkpoint).
    std::fs::remove_file(hydration_state_path(repo))?;
    println!("Checkout hydration aborted. Already-created branches were retained.");
    Ok(())
}

fn fetch_all_remotes(repo: &Repository) -> Result<()> {
    let output = crate::repository::git_command(repo)
        .args(["fetch", "--all", "--prune"])
        .output()
        .context("Failed to run `git fetch --all --prune`")?;

    if !output.status.success() {
        return Err(anyhow!(
            "git fetch --all --prune failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(())
}

fn ensure_local_branch_for_checkout(
    repo: &Repository,
    name: &str,
    remote_ref: &str,
    tip: &str,
) -> Result<()> {
    let tip = git2::Oid::from_str(tip)?;
    let mut branch = match repo.find_branch(name, BranchType::Local) {
        Ok(branch) => {
            // Creation may have succeeded just before a checkpoint failed.
            if branch.get().target() != Some(tip) {
                return Err(anyhow!(
                    "Branch '{}' changed since hydration was planned; refusing to overwrite it",
                    name
                ));
            }
            branch
        }
        Err(err) if err.code() == git2::ErrorCode::NotFound => {
            repo.branch(name, &repo.find_commit(tip)?, false)?
        }
        Err(err) => return Err(err.into()),
    };
    branch.set_upstream(Some(remote_ref))?;
    Ok(())
}

fn resolve_remote_tracking_ref(
    repo: &Repository,
    branch: &str,
    repository: &str,
) -> Result<Option<String>> {
    let mut candidates = Vec::new();
    for name in repo.remotes()?.iter().flatten() {
        let remote = repo.find_remote(name)?;
        // libgit2 applies insteadOf to remote.url(); retain the configured
        // GitHub identity when a transport rewrite uses a local/SSH alias.
        let configured = repo
            .config()?
            .get_string(&format!("remote.{name}.url"))
            .ok();
        let identity = remote
            .url()
            .and_then(gh::repository_identity)
            .or_else(|| configured.as_deref().and_then(gh::repository_identity));
        if identity.as_deref() != Some(repository) {
            continue;
        }
        let tracking = format!("{name}/{branch}");
        if let Ok(reference) = repo.find_branch(&tracking, BranchType::Remote) {
            candidates.push((tracking, reference.get().peel_to_commit()?.id()));
        }
    }
    candidates.sort();
    if let Some((_, first)) = candidates.first()
        && candidates.iter().any(|(_, tip)| tip != first)
    {
        return Err(anyhow!(
            "Ambiguous remote-tracking branches for '{}' in PR repository '{}'",
            branch,
            repository
        ));
    }
    Ok(candidates.into_iter().next().map(|(name, _)| name))
}

fn perform_git_checkout(name: &str) -> Result<()> {
    let repo = crate::open_repo()?;
    let lock = crate::state_io::RepoLock::acquire(&repo)?;
    crate::operation_state::ensure_idle(&repo, &lock, crate::operation_state::Allow::NOTHING)?;
    crate::overrides::with_planned(&repo, false, || {
        let mut plan = crate::overrides::Plan::default();
        crate::overrides::prepare(&repo, plan.checkout_rev(&repo, name))?;
        git_checkout(&repo, name)
    })
}

fn git_checkout(repo: &Repository, name: &str) -> Result<()> {
    let output = crate::repository::git_command(repo)
        .arg("checkout")
        .arg(name)
        .output()
        .with_context(|| format!("Failed to run `git checkout {name}`"))?;

    if !output.status.success() {
        return Err(anyhow!(
            "git checkout failed for branch '{}': {}",
            name,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(())
}

fn find_first_parent_branches_via_git_log(
    repo: &git2::Repository,
    upstream_name: &str,
    current_branch: &str,
) -> Result<Vec<String>> {
    let output = crate::repository::git_command(repo)
        .args([
            "log",
            "--first-parent",
            "--decorate=full",
            "--format=%H%x00%D",
            "HEAD",
            &format!("^{upstream_name}"),
        ])
        .output()?;

    if !output.status.success() {
        return Err(anyhow!(
            "Failed to inspect first-parent ancestry via git log"
        ));
    }

    let stdout = String::from_utf8(output.stdout)?;
    let mut lines = stdout.lines();
    let _ = lines.next();

    for line in lines {
        let mut parts = line.splitn(2, '\0');
        let _commit = parts.next();
        let decorations = parts.next().unwrap_or("");
        let mut names = Vec::new();

        for token in decorations.split(',') {
            let item = token.trim();
            if item.is_empty() {
                continue;
            }

            let maybe_ref = if let Some(rest) = item.strip_prefix("HEAD -> ") {
                rest.trim()
            } else {
                item
            };

            if let Some(local) = maybe_ref.strip_prefix("refs/heads/")
                && local != current_branch
                && local != upstream_name
            {
                names.push(local.to_string());
            }
        }

        if !names.is_empty() {
            names.sort();
            names.dedup();
            return Ok(names);
        }
    }

    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(json: &str) -> Result<HydrationJournal> {
        parse_hydration(Path::new("kindra_checkout_state.json"), json)
    }

    fn create(branch: &str, tip: &str) -> HydrationStep {
        HydrationStep::CreateBranch {
            branch: branch.to_string(),
            remote_ref: format!("origin/{branch}"),
            tip: tip.to_string(),
        }
    }

    /// Kindra 1.1 skipped completed steps wherever they were: a branch that
    /// existed when it planned is completed from the start.
    #[test]
    fn a_flat_journal_keeps_only_the_steps_left_to_run() {
        let journal = parsed(
            r#"{"branch":"c","repository":"github.com/test/project","steps":[
                {"branch":"a","remote_ref":null,"tip":"1111","completed":true},
                {"branch":"b","remote_ref":"origin/b","tip":"2222","completed":true},
                {"branch":"c","remote_ref":"origin/c","tip":"3333","completed":false},
                {"branch":"d","remote_ref":"origin/d","tip":"4444","completed":false}]}"#,
        )
        .unwrap();
        assert_eq!(
            journal,
            HydrationJournal {
                repository: "github.com/test/project".to_string(),
                steps: vec![
                    create("c", "3333"),
                    create("d", "4444"),
                    HydrationStep::Checkout {
                        branch: "c".to_string()
                    },
                ],
                cursor: HydrationCursor { step: 0 },
            }
        );
    }

    #[test]
    fn a_saved_journal_reads_back() {
        let journal = HydrationJournal {
            repository: "github.com/test/project".to_string(),
            steps: vec![
                create("a", "1111"),
                HydrationStep::Checkout {
                    branch: "a".to_string(),
                },
            ],
            cursor: HydrationCursor { step: 1 },
        };
        let json = serde_json::json!({
            "version": HYDRATION_JOURNAL_VERSION,
            "journal": journal,
        });
        assert_eq!(parsed(&json.to_string()).unwrap(), journal);
    }

    #[test]
    fn a_version_without_a_journal_is_malformed() {
        let err = parsed(r#"{"version":1,"branch":"a","repository":"r","steps":[]}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("it has a version but no journal"), "{err}");
        assert!(err.contains("kin abort"), "{err}");
    }

    #[test]
    fn a_journal_with_a_newer_version_is_refused_with_advice() {
        assert_eq!(HYDRATION_JOURNAL_VERSION, 1);
        for version in ["2", r#""1""#] {
            let err = parsed(&format!(r#"{{"version":{version},"journal":{{}}}}"#))
                .unwrap_err()
                .to_string();
            assert!(err.contains("newer version of kin"), "{err}");
            assert!(err.contains("kin abort"), "{err}");
        }
    }
}
