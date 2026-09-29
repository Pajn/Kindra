use anyhow::{Result, anyhow};
use git2::{Oid, Repository};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::repository::git_command;

use crate::set_aside::{self, SetAsides};
use crate::stack::collect_first_parent_chain;

/// The command that paused, persisted as `operation`. It is a label for `kin
/// status`, refusals and messages only: behaviour comes from what the journal
/// records, such as its [`RebaseOptions`] and bases.
///
/// Kindra 1.1 and earlier saved restack as `Move` and absorb as `Commit`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Move,
    Reorder,
    Commit,
    Sync,
    Restack,
    Absorb,
}

impl Operation {
    /// The command's name, as the user typed it.
    pub fn command(self) -> &'static str {
        match self {
            Operation::Move => "move",
            Operation::Reorder => "reorder",
            Operation::Commit => "commit",
            Operation::Sync => "sync",
            Operation::Restack => "restack",
            Operation::Absorb => "absorb",
        }
    }

    /// The command's name at the start of a sentence.
    fn title(self) -> &'static str {
        match self {
            Operation::Move => "Move",
            Operation::Reorder => "Reorder",
            Operation::Commit => "Commit",
            Operation::Sync => "Sync",
            Operation::Restack => "Restack",
            Operation::Absorb => "Absorb",
        }
    }
}

/// How [`run_rebase_loop`] runs each branch's `git rebase`. Every option is
/// off by default, which is how move, restack, reorder, commit and absorb
/// replay their branches.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct RebaseOptions {
    /// Keep commits the new base already has and commits that become empty
    /// (`--reapply-cherry-picks --empty=keep`), so a sync never drops a
    /// commit because the trunk has an equivalent one.
    pub keep_cherry_picks: bool,
    /// Leave a branch already on its new base as it is instead of recreating
    /// its commits (no `--no-ff`).
    pub fast_forward: bool,
    /// Leave other branches that point into the replayed range where they
    /// are (no `--update-refs`): a sync of the trunk moves only the trunk.
    pub keep_other_refs: bool,
    /// Each rebase runs only when the operation starts. `kin continue`
    /// finishes a stopped rebase, but where one is neither stopped nor
    /// complete, for example because it was aborted with Git, it refuses
    /// rather than start it again.
    pub no_restart: bool,
}

/// What a journal saved before [`JOURNAL_VERSION`] 3 recorded in `replay`
/// about how its rebases run. Loading converts it into the [`RebaseOptions`]
/// and bases that version 3 records instead.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
enum LegacyReplay {
    /// Replay each remaining branch onto its new base; the original branch
    /// lands on `target_branch`.
    Branches,
    /// Tree sync: replay each remaining branch onto its planned parent,
    /// keeping cherry-picks and empty commits.
    SyncTree,
    /// Linear sync: one Git rebase of `original_branch` onto `target_branch`,
    /// run once, keeping cherry-picks and empty commits.
    SyncLinear,
}

/// The fields of a journal saved before version 3 that say how it replays.
#[derive(Deserialize)]
struct LegacyReplayFields {
    operation: Operation,
    #[serde(default)]
    replay: Option<LegacyReplay>,
    #[serde(default)]
    parent_name_map: HashMap<String, String>,
}

impl LegacyReplay {
    /// How the journal `fields` replays: its `replay`, or for a journal saved
    /// without one (Kindra 1.1 and earlier) what its label meant then: a sync
    /// with parent names is a tree sync, one without is linear, and every
    /// other operation replays branches.
    fn of(fields: &LegacyReplayFields) -> Self {
        fields.replay.unwrap_or(match fields.operation {
            Operation::Sync if fields.parent_name_map.is_empty() => LegacyReplay::SyncLinear,
            Operation::Sync => LegacyReplay::SyncTree,
            _ => LegacyReplay::Branches,
        })
    }

    /// Record in `state` what this replay meant, so the rebase loop runs and
    /// finishes it as the Kindra that saved it would have. The merged
    /// branches a sync deletes are already recorded in
    /// `cleanup_merged_branches`.
    fn apply(self, state: &mut RebaseState) {
        match self {
            LegacyReplay::Branches => {}
            LegacyReplay::SyncTree => {
                state.rebase_options = RebaseOptions {
                    keep_cherry_picks: true,
                    ..RebaseOptions::default()
                };
                // Every branch lands on its planned parent, the original one
                // included, which a replay of branches would put on
                // `target_branch` instead.
                for branch in &state.remaining_branches {
                    let base = state
                        .parent_name_map
                        .get(branch)
                        .or_else(|| state.parent_id_map.get(branch));
                    if let Some(base) = base {
                        state
                            .new_base_map
                            .entry(branch.clone())
                            .or_insert_with(|| base.clone());
                    }
                }
            }
            LegacyReplay::SyncLinear => {
                state.rebase_options = RebaseOptions {
                    keep_cherry_picks: true,
                    fast_forward: true,
                    // A sync of the trunk rebased the trunk alone, and is the
                    // one linear sync whose fallback checkout is the branch
                    // it rebases.
                    keep_other_refs: state.cleanup_checkout_fallback.as_ref()
                        == Some(&state.original_branch),
                    no_restart: true,
                };
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RebaseState {
    /// The command that paused; a label only (see [`Operation`]).
    pub operation: Operation,
    /// How each branch's `git rebase` runs.
    #[serde(default)]
    pub rebase_options: RebaseOptions,
    /// Branch that acts as the rebase-root for this operation.
    pub original_branch: String,
    /// Operation target branch (for move: onto branch, for commit: commit target).
    pub target_branch: String,
    /// Branch to restore at the end (set for commit --on from another branch).
    #[serde(default)]
    pub caller_branch: Option<String>,
    /// List of branches remaining to be moved
    pub remaining_branches: Vec<String>,
    /// The branch currently being rebased
    pub in_progress_branch: Option<String>,
    /// branch_name -> original_parent_id_str
    #[serde(default)]
    pub parent_id_map: HashMap<String, String>,
    /// branch_name -> original_parent_name (if it was a branch in the sub-stack)
    #[serde(default)]
    pub parent_name_map: HashMap<String, String>,
    /// branch_name -> explicit new base (branch name or commit id), for
    /// reorder-like flows and for every branch a tree sync replays
    #[serde(default)]
    pub new_base_map: HashMap<String, String>,
    /// branch_name -> number of first-parent commits originally in the branch delta
    #[serde(default)]
    pub original_commit_count_map: HashMap<String, usize>,
    /// branch_name -> original tip commit id before the operation started
    #[serde(default)]
    pub original_tip_map: HashMap<String, String>,
    /// branch_name -> tip commit id Kindra most recently left behind in a resumable state
    #[serde(default)]
    pub owned_tip_map: HashMap<String, String>,
    /// What the operation set aside and still has to restore, oldest first.
    /// A journal saved by Kindra 1.1 or earlier recorded these as `stash_ref`,
    /// `stash_apply_index` and `carry_stash_ref`; loading converts them.
    #[serde(default)]
    pub set_asides: SetAsides,
    /// Whether `kin abort` must preserve content the operation already
    /// committed (absorb: the fixup/folded commits). When set, abort checks
    /// out the original branch and applies the stash *before* restoring the
    /// branch tips: the worktree keeps the committed content while the ref
    /// moves back underneath it, so the discarded changes reappear as staged
    /// changes instead of being lost.
    #[serde(default)]
    pub preserve_content_on_abort: bool,
    /// Whether `kin continue` should pin GIT_EDITOR to `true` when resuming
    /// this operation's rebase (absorb: squash! folds must never open a
    /// commit-message editor).
    #[serde(default)]
    pub suppress_editor: bool,
    /// Set when the operation was already rolled back but could not finish
    /// restoring the working tree: only `kin abort` may complete it.
    #[serde(default)]
    pub abort_only: bool,
    /// Whether to run `git reset` when returning to the original branch.
    #[serde(default)]
    pub unstage_on_restore: bool,
    /// Saved as `autostash` by Kindra releases that let the rebase loop set
    /// the tree aside later: when set and nothing is set aside yet, the loop
    /// sets the tracked changes aside as a whole tree before its first rebase
    /// and clears it. Operations now take their set-asides before saving the
    /// journal, so this Kindra never sets it and saves it only while an older
    /// journal still holds it; Git's own autostash is never used.
    #[serde(
        default,
        rename = "autostash",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub legacy_autostash: bool,
    /// Merged branches the operation deletes once it completes (sync).
    #[serde(default)]
    pub cleanup_merged_branches: Vec<String>,
    /// Branch to check out before deleting a merged branch that is checked
    /// out when the operation completes; `target_branch` when unset.
    #[serde(default)]
    pub cleanup_checkout_fallback: Option<String>,
}

impl set_aside::Journal for RebaseState {
    fn set_asides(&mut self) -> &mut SetAsides {
        &mut self.set_asides
    }

    fn save(&self, repo: &Repository) -> Result<()> {
        save_state(repo, self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcileMode {
    Continue,
    Passive,
}

pub fn state_path(repo: &Repository) -> PathBuf {
    crate::operation_state::PersistedOperation::Rebase.path(repo)
}

pub fn save_state(repo: &Repository, state: &RebaseState) -> Result<()> {
    let mut persisted_state = state.clone();
    merge_persisted_original_tips(repo, &mut persisted_state)?;
    augment_original_tip_map(repo, &mut persisted_state)?;
    persisted_state.owned_tip_map = capture_owned_tip_map(repo, &persisted_state);
    let json = serde_json::to_string_pretty(&serde_json::json!({
        "version": JOURNAL_VERSION,
        "journal": persisted_state,
    }))?;
    crate::state_io::write_atomic(&state_path(repo), &json)?;
    Ok(())
}

pub fn load_state(repo: &Repository) -> Result<RebaseState> {
    let path = state_path(repo);
    if !path.exists() {
        return Err(anyhow!("No rebase operation in progress."));
    }
    let json = fs::read_to_string(&path)?;
    parse_state(&path, &json)
}

/// The format of the journal this Kindra saves.
///
/// The file holds `{"version": N, "journal": {...}}`. Kindra 1.1 and earlier
/// saved the journal's fields at the top level, so they cannot parse this
/// envelope (it lacks their required fields) and refuse it rather than
/// misread it. Only `version` is fixed: a later format may shape `journal`
/// differently.
///
/// Bump the version whenever a journal saved by this Kindra would be misread
/// by an older one: a field whose absence an older release would take as
/// something else, a field whose meaning changes, or a new value in an
/// existing field. Every Kindra reads every older format and refuses a newer
/// one with advice. A file without `version` is a flat journal saved by
/// Kindra 1.1 or earlier.
///
/// - 1: the first envelope.
/// - 2: an unstaged-only set-aside may record `restore: UnstagedDelta`, which
///   version 1 does not know. Later version 2 journals record every
///   set-aside when it is taken and omit `autostash`; a reader of version 2
///   takes its absence as `false`, which is what it means for them (nothing
///   left for the rebase loop to set aside), so this needed no new version.
/// - 3: `replay` is gone. How each rebase runs is recorded in
///   `rebase_options`, a tree sync records every branch's planned parent in
///   `new_base_map`, and every operation finishes in the rebase loop, which
///   deletes the `cleanup_merged_branches` it recorded. A version 2 reader
///   takes a journal without `replay` as a flat one and derives how it
///   replays from its label, not from what it records. Journals of versions 1
///   and 2, and flat ones, are converted when loaded (see [`LegacyReplay`]).
pub const JOURNAL_VERSION: u64 = 3;

/// Parse a saved journal. One saved in a newer format, or that does not parse
/// (for example because a newer Kindra saved an operation this one does not
/// know), is reported with how to get past it.
///
/// A file with a `version` but no `journal` object is refused as malformed:
/// no release wrote one, and reading it as a flat journal would guess.
fn parse_state(path: &Path, json: &str) -> Result<RebaseState> {
    let unreadable = |err: &dyn std::fmt::Display| {
        anyhow!(
            "Could not read the paused operation saved in {}: {err}. If a newer version of kin \
             saved it, finish it with that version, or run 'kin abort --clear-state' to discard \
             Kindra's record of it.",
            path.display()
        )
    };
    let mut saved: serde_json::Value =
        serde_json::from_str(json).map_err(|err| unreadable(&err))?;
    let (journal, version) = match saved.get("version").map(serde_json::Value::as_u64) {
        None => {
            convert_legacy_journal(&mut saved).map_err(|err| unreadable(&err))?;
            (saved, 0)
        }
        Some(Some(version)) if version <= JOURNAL_VERSION => {
            match saved.get_mut("journal").map(serde_json::Value::take) {
                Some(journal @ serde_json::Value::Object(_)) => (journal, version),
                _ => return Err(unreadable(&"it has a version but no journal")),
            }
        }
        Some(_) => {
            return Err(anyhow!(
                "The paused operation saved in {} was saved by a newer version of kin (journal \
                 version {}; this kin reads up to version {JOURNAL_VERSION}). Finish it with that \
                 version, or run 'kin abort --clear-state' to discard Kindra's record of it.",
                path.display(),
                saved["version"]
            ));
        }
    };
    // Before version 3 the journal said how it replays in `replay`, or only
    // through its label.
    let replay = if version < 3 {
        let fields: LegacyReplayFields =
            serde_json::from_value(journal.clone()).map_err(|err| unreadable(&err))?;
        Some(LegacyReplay::of(&fields))
    } else {
        None
    };
    let mut state: RebaseState = serde_json::from_value(journal).map_err(|err| unreadable(&err))?;
    if let Some(replay) = replay {
        replay.apply(&mut state);
    }
    Ok(state)
}

/// The set-aside fields of a journal saved by Kindra 1.1 or earlier.
#[derive(Deserialize)]
struct LegacySetAsides {
    #[serde(default)]
    stash_ref: Option<String>,
    #[serde(default)]
    stash_apply_index: bool,
    #[serde(default)]
    carry_stash_ref: Option<String>,
}

/// Rewrite a journal saved without a `version` into today's shape: its
/// `stash_ref`, `stash_apply_index` and `carry_stash_ref` become `set_asides`.
/// Every other field it has still means what it meant then.
fn convert_legacy_journal(journal: &mut serde_json::Value) -> serde_json::Result<()> {
    let legacy: LegacySetAsides = serde_json::from_value(journal.clone())?;
    let Some(fields) = journal.as_object_mut() else {
        return Ok(());
    };
    for key in ["stash_ref", "stash_apply_index", "carry_stash_ref"] {
        fields.remove(key);
    }
    let set_asides = SetAsides::from_legacy(
        legacy.stash_ref,
        legacy.stash_apply_index,
        legacy.carry_stash_ref,
    );
    if !set_asides.is_empty() {
        fields.insert("set_asides".to_string(), serde_json::to_value(set_asides)?);
    }
    Ok(())
}

pub fn checkout_branch(repo: &Repository, branch_name: &str) -> Result<()> {
    let status = git_command(repo)
        .arg("checkout")
        .arg(branch_name)
        .status()?;
    if !status.success() {
        return Err(anyhow!("git checkout failed for branch '{}'", branch_name));
    }
    Ok(())
}

/// Whether a native `git rebase` is in progress. A stopped `git am` also uses
/// `rebase-apply/` but is not a rebase: `git rebase --continue` cannot finish it.
pub fn git_rebase_in_progress(repo: &Repository) -> bool {
    crate::operation_state::native_operation(repo)
        == crate::operation_state::NativeOperation::Rebase
}

// Each absorb owns a unique namespace recorded in its worktree's saved state.
// Shared refs are used because libgit2 does not resolve refs/worktree consistently
// in linked worktrees. Cleanup removes only anchors listed in this operation.
pub const ABSORB_FORK_REF_PREFIX: &str = "refs/kindra/absorb/";

pub fn clear_state(repo: &Repository) -> Result<()> {
    if let Ok(state) = load_state(repo) {
        let references: HashSet<_> = state
            .new_base_map
            .values()
            .filter(|name| name.starts_with(ABSORB_FORK_REF_PREFIX))
            .collect();
        for name in references {
            match repo.find_reference(name) {
                Ok(mut reference) => reference.delete()?,
                Err(err) if err.code() == git2::ErrorCode::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
    }
    let path = state_path(repo);
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn reconcile_saved_rebase_state(
    repo: &Repository,
    mode: ReconcileMode,
) -> Result<Option<RebaseState>> {
    if !state_path(repo).exists() {
        return Ok(None);
    }

    let mut state = load_state(repo)?;
    if git_rebase_in_progress(repo) {
        if !active_git_rebase_matches_state(repo, &state)? {
            return Err(anyhow!(
                "Active git rebase does not match saved Kindra rebase state. Resolve or abort the active git rebase before continuing."
            ));
        }
        return Ok(Some(state));
    }

    let mut changed = false;
    while let Some(current_name) = state.remaining_branches.first().cloned() {
        if !branch_rebase_completed(repo, &state, &current_name)? {
            break;
        }

        if mode == ReconcileMode::Continue {
            println!("Branch {} already rebased.", current_name);
        }
        state.remaining_branches.remove(0);
        if state.in_progress_branch.as_ref() == Some(&current_name) {
            state.in_progress_branch = None;
        }
        changed = true;
    }

    if state.remaining_branches.is_empty()
        && state.in_progress_branch.is_none()
        && mode == ReconcileMode::Passive
        && can_passively_clear_completed_state(repo, &state)?
    {
        clear_state(repo)?;
        return Ok(None);
    }

    if changed {
        save_state(repo, &state)?;
    }

    Ok(Some(state))
}

/// `owned_tip_state_matches` treats an empty `state.owned_tip_map` as a deliberate
/// "no tracked branches" sentinel and also as the migration fallback for legacy
/// on-disk state loaded via `#[serde(default)]`. That means `abort` will skip
/// restoration when ownership cannot be proven. A secondary consequence is that if
/// `capture_owned_tip_map` ever returns an empty map and `save_state` persists it,
/// later `owned_tip_state_matches` checks will also report "not owned" and `abort`
/// will clear Kindra state without restoring refs.
pub fn owned_tip_state_matches(repo: &Repository, state: &RebaseState) -> Result<bool> {
    if state.owned_tip_map.is_empty() {
        return Ok(false);
    }

    let current_tip_map = capture_owned_tip_map(repo, state);
    Ok(current_tip_map == state.owned_tip_map)
}

fn capture_owned_tip_map(repo: &Repository, state: &RebaseState) -> HashMap<String, String> {
    let mut tip_map = HashMap::new();
    let tracked_branch_names = tracked_branch_names(state);
    let rebased_commit_set = collect_rebased_commit_set(repo, state);

    for branch_name in tracked_branch_names {
        if let Ok(branch) = repo.find_branch(&branch_name, git2::BranchType::Local)
            && let Some(oid) = branch.get().target()
        {
            tip_map.insert(branch_name, oid.to_string());
        }
    }

    if let Ok(branches) = repo.branches(Some(git2::BranchType::Local)) {
        for branch_result in branches.flatten() {
            let (branch, _) = branch_result;
            let Ok(Some(branch_name)) = branch.name() else {
                continue;
            };
            let Some(oid) = branch.get().target() else {
                continue;
            };
            if rebased_commit_set.contains(&oid) {
                tip_map
                    .entry(branch_name.to_string())
                    .or_insert(oid.to_string());
            }
        }
    }

    tip_map
}

fn augment_original_tip_map(repo: &Repository, state: &mut RebaseState) -> Result<()> {
    let rebased_commit_set = collect_rebased_commit_set(repo, state);
    if rebased_commit_set.is_empty() {
        return Ok(());
    }

    let branches = repo.branches(Some(git2::BranchType::Local))?;
    for branch_result in branches {
        let (branch, _) = branch_result?;
        let Some(oid) = branch.get().target() else {
            continue;
        };
        if !rebased_commit_set.contains(&oid) {
            continue;
        }

        let Ok(Some(branch_name)) = branch.name() else {
            continue;
        };
        state
            .original_tip_map
            .entry(branch_name.to_string())
            .or_insert_with(|| oid.to_string());
    }

    Ok(())
}

fn merge_persisted_original_tips(repo: &Repository, state: &mut RebaseState) -> Result<()> {
    let path = state_path(repo);
    if !path.exists() {
        return Ok(());
    }

    let json = fs::read_to_string(&path)?;
    let previous_state = parse_state(&path, &json)?;
    for (branch_name, original_tip) in previous_state.original_tip_map {
        state
            .original_tip_map
            .entry(branch_name)
            .or_insert(original_tip);
    }

    Ok(())
}

fn tracked_branch_names(state: &RebaseState) -> HashSet<String> {
    let mut branch_names = HashSet::new();

    branch_names.extend(state.original_tip_map.keys().cloned());
    branch_names.extend(state.remaining_branches.iter().cloned());
    branch_names.insert(state.original_branch.clone());
    if let Some(branch) = &state.caller_branch {
        branch_names.insert(branch.clone());
    }
    if let Some(branch) = &state.in_progress_branch {
        branch_names.insert(branch.clone());
    }

    branch_names
}

// collect_rebased_commit_set iterates state.original_tip_map while reading
// state.parent_id_map. This depends on save_state calling augment_original_tip_map
// before capture_owned_tip_map, so state.original_tip_map contains all branches
// present in state.parent_id_map. Callers must preserve that ordering and ensure
// state.original_tip_map contains the branches to inspect.
fn collect_rebased_commit_set(repo: &Repository, state: &RebaseState) -> HashSet<Oid> {
    let mut rebased_commits = HashSet::new();

    for (branch_name, original_tip_str) in &state.original_tip_map {
        let Some(old_parent_id_str) = state.parent_id_map.get(branch_name) else {
            continue;
        };
        let Ok(original_tip) = Oid::from_str(original_tip_str) else {
            continue;
        };
        let Ok(old_parent_id) = Oid::from_str(old_parent_id_str) else {
            continue;
        };
        if original_tip == old_parent_id {
            continue;
        }

        let Ok(mut walk) = repo.revwalk() else {
            continue;
        };
        if walk.push(original_tip).is_err() || walk.hide(old_parent_id).is_err() {
            continue;
        }

        rebased_commits.extend(walk.filter_map(|id| id.ok()));
    }

    rebased_commits
}

fn branch_rebase_target(state: &RebaseState, branch_name: &str) -> Result<(String, String)> {
    let old_parent_id_str = state
        .parent_id_map
        .get(branch_name)
        .ok_or_else(|| anyhow!("Parent ID not found for branch '{}'", branch_name))?
        .clone();

    // A tree sync records every branch's planned parent in `new_base_map`, so
    // its original branch lands there rather than on `target_branch`.
    let new_base = if let Some(explicit_base) = state.new_base_map.get(branch_name) {
        explicit_base.clone()
    } else if branch_name == state.original_branch {
        state.target_branch.clone()
    } else {
        match state.parent_name_map.get(branch_name) {
            Some(name) => name.clone(),
            None => old_parent_id_str.clone(),
        }
    };

    Ok((old_parent_id_str, new_base))
}

/// Checks rebase completion in three stages that each cover a different edge
/// case. First, `branch_rebase_target` identifies the expected base and the
/// branch tip must be a descendant of it, or equal to it, which handles branches
/// whose commits were fully replayed or intentionally emptied. Second, the
/// first-parent chain length is compared against `original_commit_count_map` so
/// a branch with hidden extra commits past the expected replay is not accepted
/// as complete. Finally, the revwalk from the current tip back to `new_base_id`
/// verifies that the first replayed commit's first parent is exactly the new
/// base, protecting against histories that contain the base but are attached
/// through an unexpected first-parent path.
fn branch_rebase_completed(
    repo: &Repository,
    state: &RebaseState,
    branch_name: &str,
) -> Result<bool> {
    let (_, new_base) = branch_rebase_target(state, branch_name)?;
    let current_id = repo.revparse_single(branch_name)?.id();
    let new_base_id = repo.revparse_single(&new_base)?.id();
    let mut is_done =
        repo.graph_descendant_of(current_id, new_base_id)? || current_id == new_base_id;

    if is_done
        && current_id != new_base_id
        && let Some(original_commit_count) = state.original_commit_count_map.get(branch_name)
    {
        let current_first_parent_chain = collect_first_parent_chain(repo, new_base_id, current_id)?;
        if current_first_parent_chain.len() > *original_commit_count {
            is_done = false;
        }
    }

    if is_done && current_id != new_base_id {
        let mut walk = repo.revwalk()?;
        walk.push(current_id)?;
        walk.hide(new_base_id)?;
        let mut commits: Vec<Oid> = walk.filter_map(|id| id.ok()).collect();
        commits.reverse();

        if let Some(&first_id) = commits.first() {
            let first_commit = repo.find_commit(first_id)?;
            if first_commit.parent_count() > 0 && first_commit.parent_id(0)? != new_base_id {
                is_done = false;
            }
        }
    }

    Ok(is_done)
}

fn active_git_rebase_matches_state(repo: &Repository, state: &RebaseState) -> Result<bool> {
    if let Some(active_branch) = active_git_rebase_branch(repo)? {
        return Ok(state.in_progress_branch.as_deref() == Some(active_branch.as_str()));
    }

    owned_tip_state_matches(repo, state)
}

fn active_git_rebase_branch(repo: &Repository) -> Result<Option<String>> {
    rebase_head_branch(repo.path())
}

/// The branch a native rebase in the worktree whose Git directory is `git_dir`
/// is rewriting, from the `head-name` Git records when the rebase starts.
fn rebase_head_branch(git_dir: &Path) -> Result<Option<String>> {
    for rebase_dir in ["rebase-merge", "rebase-apply"] {
        let head_name_path = git_dir.join(rebase_dir).join("head-name");
        if !head_name_path.exists() {
            continue;
        }

        let head_name = fs::read_to_string(head_name_path)?;
        let branch_name = head_name
            .trim()
            .strip_prefix("refs/heads/")
            .unwrap_or_else(|| head_name.trim())
            .to_string();
        if !branch_name.is_empty() {
            return Ok(Some(branch_name));
        }
    }

    Ok(None)
}

/// The branch a native bisect in the worktree whose Git directory is
/// `git_dir` returns to on `git bisect reset` (a commit id when it was
/// started detached, which never matches a branch name).
fn bisect_start_branch(git_dir: &Path) -> Option<String> {
    let start = fs::read_to_string(git_dir.join("BISECT_START")).ok()?;
    let start = start.trim();
    (!start.is_empty()).then(|| start.to_string())
}

/// The Git directory of the worktree checked out at `worktree`: `.git`
/// itself for the main worktree, or the directory a linked worktree's `.git`
/// file points at.
fn worktree_git_dir(worktree: &Path) -> Option<PathBuf> {
    let dot_git = worktree.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let pointer = fs::read_to_string(&dot_git).ok()?;
    let git_dir = Path::new(pointer.trim().strip_prefix("gitdir:")?.trim());
    Some(if git_dir.is_absolute() {
        git_dir.to_path_buf()
    } else {
        worktree.join(git_dir)
    })
}

fn can_passively_clear_completed_state(repo: &Repository, state: &RebaseState) -> Result<bool> {
    // A conflicted stash is removed from state to avoid applying it twice,
    // but recovery must remain available until its index conflicts are resolved.
    if unmerged_paths_exist(repo)? {
        return Ok(false);
    }
    // Anything still set aside, including staged changes caught mid-carry,
    // is restored only by finishing or aborting the operation.
    if !state.set_asides.is_empty()
        || state.unstage_on_restore
        || !state.cleanup_merged_branches.is_empty()
    {
        return Ok(false);
    }

    let restore_branch = state
        .caller_branch
        .as_deref()
        .unwrap_or(state.original_branch.as_str());
    if current_branch_name(repo)? != Some(restore_branch.to_string()) {
        return Ok(false);
    }

    Ok(true)
}

fn current_branch_name(repo: &Repository) -> Result<Option<String>> {
    if repo.head_detached()? {
        return Ok(None);
    }

    Ok(repo.head()?.shorthand().map(ToString::to_string))
}

pub fn check_worktrees(repo: &Repository, branches: &[String], force: bool) -> Result<()> {
    if force || branches.is_empty() {
        return Ok(());
    }

    let elsewhere = branches_checked_out_elsewhere(repo)?;
    for branch in branches {
        if let Some(held) = elsewhere.get(branch) {
            return Err(anyhow!(
                "{} is {}, aborting as a full rebase can not be completed. Use --force to ignore this check.",
                branch,
                held
            ));
        }
    }

    Ok(())
}

/// The first of `branches` another worktree holds, and how it holds it (for
/// example `checked out in <path>`).
pub fn first_held_elsewhere(
    repo: &Repository,
    branches: &[String],
) -> Result<Option<(String, String)>> {
    if branches.is_empty() {
        return Ok(None);
    }
    let elsewhere = branches_checked_out_elsewhere(repo)?;
    Ok(branches.iter().find_map(|branch| {
        elsewhere
            .get(branch)
            .map(|held| (branch.clone(), held.to_string()))
    }))
}

/// Removes the branches another worktree has checked out from `branches`,
/// which are about to be deleted as merged, and says which were kept. Git
/// refuses to delete a branch checked out elsewhere, and nothing else in a sync
/// touches a merged branch, so keeping them never needs to block the sync.
pub fn keep_merged_branches_checked_out_elsewhere(
    repo: &Repository,
    branches: &mut Vec<String>,
) -> Result<()> {
    if branches.is_empty() {
        return Ok(());
    }

    let elsewhere = branches_checked_out_elsewhere(repo)?;
    branches.retain(|branch| match elsewhere.get(branch) {
        Some(held) => {
            println!("Keeping merged branch {}: it is {}.", branch, held);
            false
        }
        None => true,
    });
    Ok(())
}

/// How another worktree holds a branch.
enum HeldBy {
    CheckedOut(String),
    Rebased(String),
    Bisected(String),
}

impl std::fmt::Display for HeldBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeldBy::CheckedOut(path) => write!(f, "checked out in {path}"),
            HeldBy::Rebased(path) => write!(f, "being rebased in {path}"),
            HeldBy::Bisected(path) => write!(f, "being bisected in {path}"),
        }
    }
}

/// Branches held by a worktree other than the current one: checked out
/// there, or detached from while a native rebase rewrites it or a bisect will
/// return to it. Git refuses to rewrite such a branch from another worktree.
fn branches_checked_out_elsewhere(repo: &Repository) -> Result<HashMap<String, HeldBy>> {
    let current_worktree_output = git_command(repo)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()?;
    if !current_worktree_output.status.success() {
        return Err(anyhow!("Failed to determine current worktree path."));
    }
    let current_worktree = String::from_utf8_lossy(&current_worktree_output.stdout)
        .trim()
        .to_string();

    let worktree_list_output = git_command(repo)
        .arg("worktree")
        .arg("list")
        .arg("--porcelain")
        .output()?;
    if !worktree_list_output.status.success() {
        return Err(anyhow!("Failed to list git worktrees."));
    }

    let stdout = String::from_utf8_lossy(&worktree_list_output.stdout);
    let mut worktree_map: HashMap<String, HeldBy> = HashMap::new();
    let mut current_path = String::new();

    for line in stdout.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = path.trim().to_string();
            if current_path == current_worktree {
                continue;
            }
            // A worktree mid-rebase or bisecting is listed as detached; its
            // own Git directory records the branch it is working on.
            let Some(git_dir) = worktree_git_dir(Path::new(&current_path)) else {
                continue;
            };
            if let Some(branch) = rebase_head_branch(&git_dir)? {
                worktree_map.insert(branch, HeldBy::Rebased(current_path.clone()));
            }
            if let Some(branch) = bisect_start_branch(&git_dir) {
                worktree_map
                    .entry(branch)
                    .or_insert_with(|| HeldBy::Bisected(current_path.clone()));
            }
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            if current_path == current_worktree {
                continue;
            }
            let branch_name = branch_ref
                .strip_prefix("refs/heads/")
                .unwrap_or(branch_ref)
                .trim()
                .to_string();
            worktree_map.insert(branch_name, HeldBy::CheckedOut(current_path.clone()));
        }
    }

    Ok(worktree_map)
}

/// True if the working tree has tracked changes that a rebase or checkout would
/// disturb, which an operation sets aside only with the autostash permission.
/// Untracked and ignored files are not counted: untracked files are set aside
/// without asking, and ignored files are left alone.
pub fn working_tree_dirty(repo: &Repository) -> Result<bool> {
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(false).include_ignored(false);
    let statuses = repo.statuses(Some(&mut opts))?;
    Ok(!statuses.is_empty())
}

/// The uniform error returned when a command needs a clean working tree and
/// autostash is off. Mirrors `git rebase`'s refusal but with Kindra guidance,
/// so every command speaks with one voice.
pub fn dirty_working_tree_error() -> anyhow::Error {
    anyhow!(
        "You have uncommitted changes.\n\
         Commit or stash them, or re-run with --autostash (or set rebase.autostash=true)."
    )
}

/// Pre-flight for rebase commands (sync, move, reorder, restack). Surfaces
/// Kindra's uniform message when the tree has tracked changes and autostash
/// (the permission to set them aside) is off. When permitted, the set-aside is
/// taken once the operation is ready to start.
pub fn ensure_rebase_working_tree(repo: &Repository, autostash: bool) -> Result<()> {
    if !autostash && working_tree_dirty(repo)? {
        return Err(dirty_working_tree_error());
    }
    Ok(())
}

pub fn unmerged_paths_exist(repo: &Repository) -> Result<bool> {
    // `:/` covers the whole tree: from a subdirectory Git would otherwise see
    // only the conflicts under it.
    let output = git_command(repo)
        .args(["ls-files", "--unmerged", "--", ":/"])
        .output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "Failed to inspect unmerged paths with git ls-files --unmerged: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(!output.stdout.is_empty())
}

pub fn has_staged_changes(repo: &Repository) -> Result<bool> {
    let output = git_command(repo)
        .args(["diff", "--cached", "--name-only"])
        .output()?;
    // Exit code 0 either way; presence of output is the signal.
    Ok(output.status.success() && !output.stdout.is_empty())
}

/// List every local branch whose tip lies inside the range `base..head`
/// (`head` included, `base` excluded; a `None` base hides nothing). These are
/// the refs a `git rebase --update-refs` over that range moves along with the
/// rewrite.
pub fn local_branch_tips_in_range(
    repo: &Repository,
    base: Option<Oid>,
    head: Oid,
) -> Result<Vec<(String, Oid)>> {
    let mut walk = repo.revwalk()?;
    walk.push(head)?;
    if let Some(base) = base {
        walk.hide(base)?;
    }
    let rewritten: HashSet<Oid> = walk.filter_map(|id| id.ok()).collect();

    let mut tips = Vec::new();
    for (branch, _) in repo.branches(Some(git2::BranchType::Local))?.flatten() {
        let Some(oid) = branch.get().target() else {
            continue;
        };
        if !rewritten.contains(&oid) {
            continue;
        }
        let Ok(Some(name)) = branch.name() else {
            continue;
        };
        tips.push((name.to_string(), oid));
    }
    Ok(tips)
}

/// Refuse a `git rebase --update-refs` of `base..head` onto `onto` that would
/// flatten a merge of another branch (see
/// [`crate::stack::find_merged_branch_in_replay`]). Commands call this while
/// planning, before they persist or change anything. `syncing` adds the
/// alternative of landing the merged branch upstream first, which sync
/// handles.
pub fn ensure_replay_keeps_merged_branches(
    repo: &Repository,
    base: Option<Oid>,
    head: Oid,
    onto: Option<Oid>,
    syncing: bool,
) -> Result<()> {
    let Some(found) = crate::stack::find_merged_branch_in_replay(repo, base, head, onto)? else {
        return Ok(());
    };
    let crate::stack::MergedBranch {
        merger,
        merged,
        merged_tip_replayed,
        merger_has_descendants,
    } = found;
    let consequence = if merged_tip_replayed {
        format!("move {merged} onto {merger}'s commits")
    } else {
        format!("copy {merged}'s commits into {merger}")
    };
    let restack = if merger_has_descendants {
        format!(", then run kin restack on {merger} to bring along the branches stacked on it")
    } else {
        String::new()
    };
    let upstream = if syncing {
        format!(", or merge {merged} upstream before syncing")
    } else {
        String::new()
    };
    Err(anyhow!(
        "{merger} merged {merged} instead of rebasing onto it, so replaying the stack would {consequence}. Rebase {merger} onto {merged} first (git rebase {merged} {merger}{restack}){upstream}."
    ))
}

/// Record, into `original_tip_map`, the pre-rewrite tip of every local branch
/// whose tip lies inside the range a `--update-refs` rebase over `base..head`
/// rewrites. Existing entries are preserved. This is what lets `kin abort`
/// roll a completed fold back off such a branch.
pub fn record_branch_tips_in_range(
    repo: &Repository,
    base: Option<Oid>,
    head: Oid,
    original_tip_map: &mut HashMap<String, String>,
) -> Result<()> {
    for (name, oid) in local_branch_tips_in_range(repo, base, head)? {
        original_tip_map.entry(name).or_insert(oid.to_string());
    }
    Ok(())
}

pub fn unstage_all(repo: &Repository) -> Result<()> {
    let status = git_command(repo).arg("reset").status()?;
    if !status.success() {
        return Err(anyhow!(
            "Failed to unstage files after returning to the original branch."
        ));
    }
    Ok(())
}

/// Everything a rebase state still checks out or replays, for local overrides.
pub fn override_plan(repo: &Repository, state: &RebaseState) -> Result<crate::overrides::Plan> {
    let mut plan = crate::overrides::Plan::default();
    for name in [&state.original_branch, &state.target_branch]
        .into_iter()
        .chain(&state.caller_branch)
        .chain(&state.cleanup_checkout_fallback)
    {
        plan.checkout_rev(repo, name);
    }
    for name in &state.remaining_branches {
        let (old_parent, new_base) = branch_rebase_target(state, name)?;
        plan.checkout_rev(repo, &new_base)
            .replay_revs(repo, &old_parent, name);
    }
    Ok(plan)
}

/// Set the working tree aside for the whole of `state`'s operation, as
/// [`set_aside::take_whole_tree`] does with the permission `allowed`, record
/// it in the journal and save the journal. The changes are restored on the
/// branch the operation returns to when it completes or is aborted, never on
/// a branch it replays. If saving fails, they are restored at once.
pub fn set_aside_working_tree(
    repo: &Repository,
    state: &mut RebaseState,
    allowed: bool,
) -> Result<()> {
    if let Some(taken) = set_aside::take_whole_tree(repo, allowed)? {
        state.set_asides.push(taken);
    }
    if let Err(err) = save_state(repo, state) {
        set_aside::unwind(repo, &mut state.set_asides);
        return Err(err);
    }
    Ok(())
}

/// Begin an operation that replays branches (move, restack, reorder, sync)
/// before handing `state` to [`run_rebase_loop`]: set the working tree aside
/// for it with the permission `allowed` and save the journal. Nothing is
/// saved when this fails.
pub fn begin_replay(repo: &Repository, state: &mut RebaseState, allowed: bool) -> Result<()> {
    ensure_git_supports(state.rebase_options)?;
    crate::overrides::prepare(repo, &override_plan(repo, state)?)?;
    if state.remaining_branches.is_empty() {
        return save_state(repo, state);
    }
    set_aside_working_tree(repo, state, allowed)
}

/// Check the installed Git has every `git rebase` flag `options` uses.
fn ensure_git_supports(options: RebaseOptions) -> Result<()> {
    if !options.keep_other_refs {
        ensure_git_supports_update_refs()
    } else if options.keep_cherry_picks {
        ensure_git_supports_reapply_cherry_picks()
    } else {
        Ok(())
    }
}

/// Replay each of `state`'s remaining branches onto its new base, then finish
/// the operation: return to the caller (or original) branch, restore what was
/// set aside, clear the journal, delete the merged branches it recorded and
/// record the operation for `kin undo`. A rebase that stops on a conflict
/// saves the journal and returns an error for `kin continue` or `kin abort`.
///
/// The command that starts an operation calls this; `kin continue` calls
/// [`resume_rebase_loop`].
pub fn run_rebase_loop(repo: &Repository, state: RebaseState) -> Result<()> {
    replay_branches(repo, state, false)
}

/// [`run_rebase_loop`] for `kin continue`, once the stopped rebase, if any,
/// has finished. A journal whose rebases run only when the operation starts
/// ([`RebaseOptions::no_restart`]) is refused here if one of them is still
/// incomplete.
pub fn resume_rebase_loop(repo: &Repository, state: RebaseState) -> Result<()> {
    replay_branches(repo, state, true)
}

fn replay_branches(repo: &Repository, mut state: RebaseState, resumed: bool) -> Result<()> {
    ensure_git_supports(state.rebase_options)?;
    crate::overrides::prepare(repo, &override_plan(repo, &state)?)?;

    // A journal saved by an older Kindra may still ask the loop to set the
    // tracked changes aside before its first rebase. Own them across the
    // entire restack, as every operation now does from the start, so
    // completion and abort restore them on the saved branch.
    if !state.remaining_branches.is_empty()
        && state.legacy_autostash
        && state.set_asides.changes().is_none()
    {
        let taken = set_aside::take_tracked(repo, true, set_aside::Restore::WithIndex)?;
        if let Some(taken) = taken {
            state.set_asides.push(taken);
        }
        state.legacy_autostash = false;
        if let Err(err) = save_state(repo, &state) {
            set_aside::unwind(repo, &mut state.set_asides);
            return Err(err);
        }
    }

    let mut started_any = false;
    while !state.remaining_branches.is_empty() {
        let current_name = state.remaining_branches[0].clone();

        // Check if we are resuming a rebase that was already in progress
        let is_resuming = state.in_progress_branch.as_ref() == Some(&current_name);

        let (old_parent_id_str, new_base) = branch_rebase_target(&state, &current_name)?;

        // Check if the branch is already rebased (e.g. by a previous --update-refs)
        let is_done = branch_rebase_completed(repo, &state, &current_name)?;

        if is_done && (is_resuming || started_any) && !git_rebase_in_progress(repo) {
            println!("Branch {} already rebased.", current_name);
            state.remaining_branches.remove(0);
            if is_resuming {
                state.in_progress_branch = None;
                started_any = true;
            }
            save_state(repo, &state)?;
            continue;
        }

        let options = state.rebase_options;
        if resumed && options.no_restart {
            let operation = state.operation;
            return Err(anyhow!(
                "{} did not complete: '{}' is not rebased onto '{}'. If the Git rebase was aborted manually, run 'kin abort' to clear the saved {} state or rerun 'kin {}'.",
                operation.title(),
                current_name,
                new_base,
                operation.command(),
                operation.command()
            ));
        }

        if !is_resuming {
            state.in_progress_branch = Some(current_name.clone());
            save_state(repo, &state)?;
        }

        println!("Rebasing {}...", current_name);
        let mut rebase = git_command(repo);
        rebase.arg("rebase");
        if options.keep_cherry_picks {
            rebase.args(["--reapply-cherry-picks", "--empty=keep"]);
        }
        if !options.fast_forward {
            rebase.arg("--no-ff");
        }
        // Kindra owns every set-aside: Git's autostash (or `rebase.autostash`)
        // would restore the changes on each replayed branch instead.
        rebase.arg("--no-autostash");
        if !options.keep_other_refs {
            rebase.arg("--update-refs");
        }
        let status = rebase
            .arg("--onto")
            .arg(&new_base)
            .arg(&old_parent_id_str)
            .arg(&current_name)
            .status()?;

        if status.success() {
            state.remaining_branches.remove(0);
            state.in_progress_branch = None;
            started_any = true;
            save_state(repo, &state)?;
        } else {
            // Check if a rebase is in progress (meaning it started but hit conflicts)
            if git_rebase_in_progress(repo) {
                // Persist that this branch is in progress, but do NOT remove it from remaining_branches
                save_state(repo, &state)?;
                return Err(anyhow!(
                    "Rebase failed for branch {}. Resolve conflicts and run 'kin continue'.",
                    current_name
                ));
            } else {
                state.in_progress_branch = None;
                save_state(repo, &state)?;
                if options.no_restart {
                    let command = state.operation.command();
                    return Err(anyhow!(
                        "git rebase failed before {command} could enter an in-progress state. Run 'kin abort' to clear the saved state, then run 'kin {command}' again (or otherwise fix the rebase)."
                    ));
                }
                return Err(anyhow!(
                    "Rebase failed for branch {}. It seems to have failed before starting (e.g., dirty working tree). Fix the issue and run 'kin continue'.",
                    current_name
                ));
            }
        }
    }

    let restore_branch = state
        .caller_branch
        .clone()
        .unwrap_or_else(|| state.original_branch.clone());
    println!(
        "Operation completed. Checking out original branch {}...",
        restore_branch
    );
    checkout_branch(repo, &restore_branch).map_err(|e| {
        anyhow!(
            "Failed to checkout back to original branch '{}'. State file preserved. {}",
            restore_branch,
            e
        )
    })?;

    set_aside::restore_all(repo, &mut state, set_aside::Phase::Completion)?;

    if state.unstage_on_restore {
        unstage_all(repo)?;
    }

    clear_state(repo)?;

    let checkout_fallback = state
        .cleanup_checkout_fallback
        .as_deref()
        .unwrap_or(state.target_branch.as_str());
    let deleted = delete_merged_branches(repo, &state.cleanup_merged_branches, checkout_fallback);
    // Record the operation for undo from the branches as they are now, even if
    // a deletion failed, so the pending snapshot is never orphaned once the
    // journal is gone; the deletion error is reported afterwards.
    crate::oplog::finalize(repo)?;
    deleted
}

/// Delete the merged `branches`, first checking out `checkout_fallback` if
/// one of them is checked out. A branch Git refuses to delete is reported and
/// kept. Each deleted branch's tip is printed so it can be restored.
pub fn delete_merged_branches(
    repo: &Repository,
    branches: &[String],
    checkout_fallback: &str,
) -> Result<()> {
    if branches.is_empty() {
        return Ok(());
    }

    if let Some(current) = current_branch_name(repo)?
        && branches.contains(&current)
    {
        println!(
            "Current branch '{}' is merged. Switching to '{}' before deletion.",
            current, checkout_fallback
        );
        let mut plan = crate::overrides::Plan::default();
        crate::overrides::prepare(repo, plan.checkout_rev(repo, checkout_fallback))?;
        checkout_branch(repo, checkout_fallback).map_err(|e| {
            anyhow!(
                "fallback git checkout failed for branch '{}': {}",
                checkout_fallback,
                e
            )
        })?;
    }

    for branch_name in branches {
        // Capture the tip before deletion so it is recoverable from the printed
        // SHA (and from `kin undo`) even after the branch ref is gone.
        let old_tip = repo
            .find_branch(branch_name, git2::BranchType::Local)
            .ok()
            .and_then(|b| b.get().target())
            .map(|oid| oid.to_string());

        let status = git_command(repo)
            .arg("branch")
            .arg("-D")
            .arg("--quiet")
            .arg(branch_name)
            .status()?;

        if !status.success() {
            println!(
                "Warning: Failed to delete merged branch: {}. It might be checked out in another worktree.",
                branch_name
            );
        } else if let Some(tip) = old_tip {
            println!(
                "Deleted merged branch: {} (was {}); run 'kin undo' or 'git branch {} {}' to restore",
                branch_name,
                &tip[..tip.len().min(12)],
                branch_name,
                tip,
            );
        } else {
            println!("Deleted merged branch: {}", branch_name);
        }
    }
    Ok(())
}

pub fn ensure_git_supports_update_refs() -> Result<()> {
    ensure_git_version_at_least(
        (2, 38, 0),
        "This operation requires Git >= 2.38.0 because '--update-refs' is used during rebase.",
        "This operation requires Git >= 2.38.0 because it uses '--update-refs'",
    )
}

pub fn ensure_git_supports_reapply_cherry_picks() -> Result<()> {
    ensure_git_version_at_least(
        (2, 34, 0),
        "This operation requires Git >= 2.34.0 because '--reapply-cherry-picks' and '--empty=keep' are used during rebase.",
        "This operation requires Git >= 2.34.0 because it uses '--reapply-cherry-picks' and '--empty=keep'",
    )
}

fn ensure_git_version_at_least(
    minimum: (u64, u64, u64),
    detected_message_prefix: &str,
    generic_message_prefix: &str,
) -> Result<()> {
    // Not `git_command`: the version is the installed git's, whatever the
    // repository.
    let output = Command::new("git").arg("--version").output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "{}, but 'git --version' failed.",
            generic_message_prefix
        ));
    }

    let version_output = String::from_utf8_lossy(&output.stdout);
    let version = parse_git_semver(&version_output).ok_or_else(|| {
        anyhow!(
            "{}, but could not parse `git --version` output: {}",
            generic_message_prefix,
            version_output.trim()
        )
    })?;

    if version < minimum {
        return Err(anyhow!(
            "{} Detected Git {}.{}.{}.",
            detected_message_prefix,
            version.0,
            version.1,
            version.2
        ));
    }

    Ok(())
}

fn parse_git_semver(version_output: &str) -> Option<(u64, u64, u64)> {
    let version_token = version_output
        .split_whitespace()
        .find(|part| part.as_bytes().first().is_some_and(u8::is_ascii_digit))?;

    let numbers = version_token
        .split('.')
        .filter_map(|segment| {
            let digits: String = segment
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect();
            (!digits.is_empty())
                .then_some(digits)
                .and_then(|d| d.parse::<u64>().ok())
        })
        .collect::<Vec<u64>>();

    if numbers.len() < 3 {
        return None;
    }

    Some((numbers[0], numbers[1], numbers[2]))
}

#[cfg(test)]
mod tests {
    use super::{
        JOURNAL_VERSION, Operation, RebaseOptions, RebaseState, parse_git_semver, parse_state,
    };
    use crate::set_aside::{Kind, Restore, SetAside, SetAsides};
    use std::collections::HashMap;
    use std::path::Path;

    fn saved(json: &str) -> RebaseState {
        serde_json::from_str(json).unwrap()
    }

    fn loaded(json: &str) -> anyhow::Result<RebaseState> {
        parse_state(Path::new("journal.json"), json)
    }

    const LEGACY_FIELDS: &str = r#""operation":"Commit","original_branch":"a",
        "target_branch":"a","remaining_branches":[],"in_progress_branch":"a""#;

    #[test]
    fn a_legacy_journal_with_autostash_keeps_it_and_its_unstaged_changes() {
        let state = loaded(&format!(
            r#"{{{LEGACY_FIELDS},"autostash":true,"stash_ref":"kin-commit-on-1-2",
                "stash_apply_index":false,"carry_stash_ref":null}}"#
        ))
        .unwrap();
        assert!(state.legacy_autostash);
        assert_eq!(
            state.set_asides,
            SetAsides::from(vec![SetAside {
                kind: Kind::UnstagedOnly,
                stash: "kin-commit-on-1-2".to_string(),
                oid: None,
                restore: Restore::Plain,
            }])
        );
    }

    /// A journal records what was set aside, not a permission to set things
    /// aside later: `autostash` is saved only while an older journal still
    /// asks the rebase loop to set the tree aside. Its absence reads as
    /// `false`, which is what it means to every Kindra that reads it.
    #[test]
    fn autostash_is_saved_only_while_an_older_journal_still_asks_for_it() {
        let mut state = saved(&format!("{{{LEGACY_FIELDS}}}"));
        assert!(!state.legacy_autostash);
        let journal = serde_json::to_value(&state).unwrap();
        assert!(journal.get("autostash").is_none(), "{journal}");

        state.legacy_autostash = true;
        let journal = serde_json::to_value(&state).unwrap();
        assert_eq!(journal["autostash"], serde_json::Value::Bool(true));
    }

    #[test]
    fn a_legacy_journal_restores_its_carry_before_its_other_changes() {
        let mut state = loaded(&format!(
            r#"{{{LEGACY_FIELDS},"stash_ref":"kin-absorb-1-2","stash_apply_index":true,
                "carry_stash_ref":"kin-commit-on-index-1-3"}}"#
        ))
        .unwrap();
        let carry = state.set_asides.pop().unwrap();
        assert_eq!(
            (carry.kind, carry.restore, carry.stash.as_str()),
            (Kind::Carry, Restore::WithIndex, "kin-commit-on-index-1-3")
        );
        let changes = state.set_asides.pop().unwrap();
        assert_eq!(
            (changes.kind, changes.restore, changes.stash.as_str()),
            (Kind::WholeTree, Restore::WithIndex, "kin-absorb-1-2")
        );
        assert!(state.set_asides.is_empty());
    }

    #[test]
    fn a_versioned_journal_ignores_fields_it_no_longer_has() {
        let state = loaded(&format!(
            r#"{{"version":{JOURNAL_VERSION},"journal":{{{LEGACY_FIELDS},
                "stash_ref":"kin-commit-on-1-2"}}}}"#
        ))
        .unwrap();
        assert!(state.set_asides.is_empty());
    }

    /// Every version up to this Kindra's reads the same way; a version 1
    /// journal keeps the set-asides it recorded.
    #[test]
    fn a_journal_of_an_older_or_the_current_version_is_read() {
        for version in 1..=JOURNAL_VERSION {
            let state = loaded(&format!(
                r#"{{"version":{version},"journal":{{{LEGACY_FIELDS},"set_asides":[
                    {{"kind":"UnstagedOnly","stash":"kin-commit-on-1-2","oid":"abc",
                      "restore":"Plain"}}]}}}}"#
            ))
            .unwrap();
            assert_eq!(
                state.set_asides,
                SetAsides::from(vec![SetAside {
                    kind: Kind::UnstagedOnly,
                    stash: "kin-commit-on-1-2".to_string(),
                    oid: Some("abc".to_string()),
                    restore: Restore::Plain,
                }]),
                "version {version}"
            );
        }
    }

    #[test]
    fn a_version_without_a_journal_is_malformed() {
        let err = loaded(&format!(
            r#"{{{LEGACY_FIELDS},"version":{JOURNAL_VERSION}}}"#
        ))
        .err()
        .unwrap()
        .to_string();
        assert!(err.contains("it has a version but no journal"), "{err}");
        assert!(err.contains("kin abort --clear-state"), "{err}");
    }

    #[test]
    fn a_journal_with_a_newer_version_is_refused_with_advice() {
        assert_eq!(JOURNAL_VERSION, 3);
        for version in ["4", "5", r#""3""#] {
            let err = loaded(&format!(r#"{{"version":{version},"journal":{{}}}}"#))
                .err()
                .unwrap()
                .to_string();
            assert!(err.contains("newer version of kin"), "{err}");
            assert!(err.contains("kin abort --clear-state"), "{err}");
        }
    }

    /// A journal's fields, with `remaining` left to rebase and `parents` as
    /// its `parent_name_map`.
    fn fields(operation: &str, remaining: &str, parents: &str, extra: &str) -> String {
        format!(
            r#""operation":"{operation}","original_branch":"a","target_branch":"main",
                "remaining_branches":{remaining},"in_progress_branch":null,
                "parent_id_map":{{"a":"1111","b":"2222"}},"parent_name_map":{parents},
                "cleanup_checkout_fallback":"main"{extra}"#
        )
    }

    const LINEAR: RebaseOptions = RebaseOptions {
        keep_cherry_picks: true,
        fast_forward: true,
        keep_other_refs: false,
        no_restart: true,
    };

    const TREE: RebaseOptions = RebaseOptions {
        keep_cherry_picks: true,
        fast_forward: false,
        keep_other_refs: false,
        no_restart: false,
    };

    /// A flat journal (Kindra 1.1 and earlier) replays as its label said: a
    /// sync with parent names is a tree sync, one without is linear, and
    /// every other operation replays branches.
    #[test]
    fn a_flat_journal_replays_as_its_label_said() {
        let linear = loaded(&format!("{{{}}}", fields("Sync", r#"["a"]"#, "{}", ""))).unwrap();
        assert_eq!(linear.rebase_options, LINEAR);
        assert!(linear.new_base_map.is_empty());

        let tree = loaded(&format!(
            "{{{}}}",
            fields("Sync", r#"["a","b"]"#, r#"{"b":"a"}"#, "")
        ))
        .unwrap();
        assert_eq!(tree.rebase_options, TREE);
        // The original branch lands on its old parent, as it did then, and
        // not on the target a replay of branches would put it on.
        assert_eq!(
            tree.new_base_map,
            HashMap::from([
                ("a".to_string(), "1111".to_string()),
                ("b".to_string(), "a".to_string()),
            ])
        );

        for operation in ["Move", "Reorder", "Commit"] {
            let state = loaded(&format!(
                "{{{}}}",
                fields(operation, r#"["a","b"]"#, r#"{"b":"a"}"#, "")
            ))
            .unwrap();
            assert_eq!(
                state.rebase_options,
                RebaseOptions::default(),
                "{operation}"
            );
            assert!(state.new_base_map.is_empty(), "{operation}");
        }
    }

    /// A version 1 or 2 journal replays as its `replay` says, whatever its
    /// label; a linear sync of the trunk leaves other branches alone.
    #[test]
    fn a_version_2_journal_replays_as_its_replay_says() {
        for version in 1..=2 {
            let journal = |operation: &str, replay: &str, parents: &str| {
                loaded(&format!(
                    r#"{{"version":{version},"journal":{{{}}}}}"#,
                    fields(
                        operation,
                        r#"["a"]"#,
                        parents,
                        &format!(r#","replay":"{replay}""#)
                    )
                ))
                .unwrap()
            };
            let tree = journal("Move", "SyncTree", "{}");
            assert_eq!(tree.operation, Operation::Move);
            assert_eq!(tree.rebase_options, TREE);
            assert_eq!(tree.new_base_map["a"], "1111");

            assert_eq!(journal("Move", "SyncLinear", "{}").rebase_options, LINEAR);
            assert_eq!(
                journal("Sync", "Branches", r#"{"b":"a"}"#).rebase_options,
                RebaseOptions::default()
            );

            // A sync of the trunk: the branch it rebases is its fallback.
            let trunk = loaded(
                &format!(
                    r#"{{"version":{version},"journal":{{{}}}}}"#,
                    fields("Sync", r#"["a"]"#, "{}", r#","replay":"SyncLinear""#)
                )
                .replace(
                    r#""cleanup_checkout_fallback":"main""#,
                    r#""cleanup_checkout_fallback":"a""#,
                ),
            )
            .unwrap();
            assert_eq!(
                trunk.rebase_options,
                RebaseOptions {
                    keep_other_refs: true,
                    ..LINEAR
                }
            );
        }
    }

    /// From version 3 the journal records how it replays, and neither its
    /// label nor a leftover `replay` changes that.
    #[test]
    fn a_version_3_journal_replays_as_it_records() {
        let state = loaded(&format!(
            r#"{{"version":3,"journal":{{{}}}}}"#,
            fields("Sync", r#"["a"]"#, "{}", r#","replay":"SyncLinear""#)
        ))
        .unwrap();
        assert_eq!(state.rebase_options, RebaseOptions::default());
        assert!(state.new_base_map.is_empty());

        let journal = serde_json::to_value(&state).unwrap();
        assert!(journal.get("replay").is_none(), "{journal}");
        assert_eq!(
            journal["rebase_options"],
            serde_json::json!({
                "keep_cherry_picks": false,
                "fast_forward": false,
                "keep_other_refs": false,
                "no_restart": false,
            })
        );
    }

    #[test]
    fn parse_git_semver_ignores_non_numeric_dot_segments() {
        let parsed = parse_git_semver("git version 2.44.0.windows.1");
        assert_eq!(parsed, Some((2, 44, 0)));
    }

    #[test]
    fn parse_git_semver_requires_three_components() {
        let parsed = parse_git_semver("git version 2.44");
        assert_eq!(parsed, None);
    }
}
