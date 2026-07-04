//! An operation log that makes destructive Kindra commands reversible.
//!
//! Kindra rewrites branch refs during `sync`, `reorder`, `move`, `restack` and
//! `split`. Once such an operation completes, the previous topology used to be
//! recoverable only by hand from per-branch reflogs, and deleted branches left
//! no trace at all. This module records, for every completed operation, the set
//! of branch tips before and after it ran, so `kin undo` / `kin redo` can move
//! the stack backwards and forwards through recent history.
//!
//! # How it stays true to Kindra's zero-metadata design
//!
//! The pre- and post-images are anchored as ordinary git refs under
//! `refs/kindra/undo/<id>/`. Those refs are reachability roots, so `git gc`
//! cannot reclaim the old commits even after a branch is deleted or rebased
//! away — no bespoke object store, and `git log refs/kindra/undo/...` works with
//! stock git. Nothing a branch depends on is stored; the log is purely additive
//! recovery information that can be deleted at any time without affecting the
//! stacks themselves.
//!
//! # Lifecycle
//!
//! 1. A mutating command calls [`begin`] right after taking the repo lock. This
//!    snapshots every local branch tip plus HEAD into a *pending* file and returns
//!    a [`PendingSnapshot`] RAII guard the command holds for the whole operation.
//! 2. When the guard drops — on success, an early `return`, a `?` error, or an
//!    unwind — it [`settle`]s the snapshot: [`finalize`] records an [`Entry`] if
//!    branch tips moved (anchoring the changed pre/post OIDs as refs) or drops the
//!    pending file if nothing changed. Commands never call [`finalize`]
//!    themselves; dropping the guard is the single settle path.
//! 3. The snapshot survives the guard only while an operation is still in progress
//!    (a conflict-paused rebase). The resuming `kin continue` settles it on
//!    completion; `kin abort` either [`discard`]s it (pre-operation refs were
//!    restored, so there is nothing to undo) or, when it clears divergent state
//!    *without* restoring refs, [`finalize`]s it (the effects are live and must
//!    stay undoable).
//!
//! Because a conflict-interrupted operation finishes in a *later* `kin continue`
//! process, the pre-image lives on disk in the pending file rather than in
//! memory. [`begin`] also reconciles any orphaned pending file (finalizing it)
//! so a crashed or errored operation still turns into an undoable entry rather
//! than lingering forever.
//!
//! Every function here is best-effort: recording history must never make a
//! successful git operation fail, so internal errors are downgraded to warnings.

use anyhow::{Context, Result, anyhow};
use git2::{BranchType, ErrorCode, Oid, Repository};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Version stamp for the on-disk log, so a future format change can be detected.
const OPLOG_VERSION: u32 = 1;

/// How many operations to keep. Older entries (and their anchor refs) are
/// dropped once this many newer entries exist, letting `git gc` eventually
/// reclaim ancient pre-images.
const MAX_ENTRIES: usize = 25;

/// The tip of one branch before and after an operation.
///
/// `None` on either side means the branch did not exist then: `pre: None` marks
/// a branch the operation created, `post: None` a branch it deleted.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
struct Change {
    pre: Option<String>,
    post: Option<String>,
}

/// One recorded operation: what changed, and where HEAD was pointing.
#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    /// Unique id (nanoseconds since the epoch). Also names this entry's anchor
    /// ref namespace, `refs/kindra/undo/<id>/`.
    id: String,
    /// Wall-clock seconds since the epoch, for display only.
    time: u64,
    /// Command that produced the entry (e.g. `"sync"`).
    op: String,
    /// One-line human summary, computed from `changes`.
    summary: String,
    /// HEAD before the operation: a branch name, or an OID if detached.
    head_before: String,
    head_before_detached: bool,
    /// HEAD after the operation.
    head_after: String,
    head_after_detached: bool,
    /// Per-branch tip changes, keyed by branch name.
    changes: BTreeMap<String, Change>,
}

/// The persisted log: an append-ordered list of entries plus a cursor.
///
/// `cursor` is how many entries, counting from the front, are currently
/// *applied*. `entries[cursor..]` are operations that have been undone and can
/// be redone. Running a fresh operation truncates that redo tail — the standard
/// editor undo-stack model.
#[derive(Serialize, Deserialize)]
struct Log {
    version: u32,
    cursor: usize,
    entries: Vec<Entry>,
}

impl Default for Log {
    fn default() -> Self {
        Log {
            version: OPLOG_VERSION,
            cursor: 0,
            entries: Vec::new(),
        }
    }
}

/// The snapshot captured by [`begin`], persisted until [`finalize`] consumes it.
#[derive(Serialize, Deserialize)]
struct Pending {
    id: String,
    time: u64,
    op: String,
    head_before: String,
    head_before_detached: bool,
    /// Every local branch tip at the moment the operation started.
    branches: BTreeMap<String, String>,
}

fn log_path(repo: &Repository) -> PathBuf {
    repo.path().join("kindra_oplog.json")
}

fn pending_path(repo: &Repository) -> PathBuf {
    repo.path().join("kindra_oplog_pending.json")
}

fn anchor_namespace(id: &str) -> String {
    format!("refs/kindra/undo/{id}")
}

// ---------------------------------------------------------------------------
// Public API used by the mutating commands.
// ---------------------------------------------------------------------------

/// Snapshot the repository before a mutating operation runs, returning a guard
/// that settles the snapshot when it drops.
///
/// The returned [`PendingSnapshot`] must be held for the whole operation. When it
/// drops — on success, an early `return`, a `?` error, or an unwind — it
/// [`settle`]s the pending snapshot, so a mutating command cannot leak one on any
/// non-panic exit. The only case where the snapshot deliberately survives the
/// guard is when a Kindra operation is still in progress (a conflict-paused
/// rebase), so the resuming `kin continue` / `kin abort` can settle it instead.
///
/// Best-effort: on any snapshot failure it warns and still returns a guard, so
/// the operation itself is never blocked. Also reconciles an orphaned pending
/// file left by a crashed or errored earlier operation.
pub fn begin<'repo>(repo: &'repo Repository, op: &str) -> Result<PendingSnapshot<'repo>> {
    if let Err(err) = begin_inner(repo, op) {
        eprintln!("Warning: could not record undo state for '{op}': {err:#}");
    }
    Ok(PendingSnapshot { repo })
}

/// RAII guard returned by [`begin`]: while held, a pending undo snapshot may
/// exist on disk; when dropped it is [`settle`]d. Dropping it is the *only* way a
/// command settles its snapshot, which makes leaking one impossible short of a
/// process abort.
#[must_use = "hold the guard for the whole operation; an unheld guard settles the snapshot immediately"]
pub struct PendingSnapshot<'repo> {
    repo: &'repo Repository,
}

impl Drop for PendingSnapshot<'_> {
    fn drop(&mut self) {
        settle(self.repo);
    }
}

/// Settle a pending snapshot at the end of a mutating command.
///
/// Enforces the invariant that a pending snapshot outlives a command *only* while
/// an operation is genuinely in progress (a conflict-paused rebase resumed later
/// by `kin continue` / `kin abort`). On every other outcome — success, no-op, or
/// an error that stopped short of an in-progress rebase — it finalizes: recording
/// an undo entry if branch tips moved, or dropping the snapshot if nothing did.
///
/// Best-effort, mirroring [`finalize`]: any error is already downgraded to a
/// warning there, so this can run from `Drop`.
fn settle(repo: &Repository) {
    if operation_in_progress(repo) {
        return;
    }
    let _ = finalize(repo);
}

fn begin_inner(repo: &Repository, op: &str) -> Result<()> {
    // Flush any orphaned pending snapshot into the log first, so it becomes a
    // real (undoable) entry instead of being silently overwritten.
    finalize_inner(repo)?;

    let (head_before, head_before_detached) = current_head(repo)?;
    let (nanos, secs) = now();
    let pending = Pending {
        id: nanos.to_string(),
        time: secs,
        op: op.to_string(),
        head_before,
        head_before_detached,
        branches: all_local_branches(repo)?,
    };
    write_json(&pending_path(repo), &pending)
}

/// Record the result of a completed operation, if one is pending.
///
/// Diffs current branch tips against the [`begin`] snapshot, anchors the
/// changed OIDs, and appends a log entry. A no-op if nothing changed or no
/// operation is pending. Best-effort: never fails a successful command.
pub fn finalize(repo: &Repository) -> Result<()> {
    if let Err(err) = finalize_inner(repo) {
        eprintln!("Warning: could not finalize undo state: {err:#}");
    }
    Ok(())
}

fn finalize_inner(repo: &Repository) -> Result<()> {
    let pending_file = pending_path(repo);
    let Some(pending) = read_json::<Pending>(&pending_file)? else {
        return Ok(());
    };

    let after = all_local_branches(repo)?;
    let changes = diff_branches(&pending.branches, &after);

    if changes.is_empty() {
        // The operation touched no branch tips (e.g. an early return). Drop the
        // pending snapshot without polluting the log.
        remove_file(&pending_file)?;
        return Ok(());
    }

    let (head_after, head_after_detached) = current_head(repo)?;
    let entry = Entry {
        id: pending.id,
        time: pending.time,
        op: pending.op.clone(),
        summary: summarize(&pending.op, &changes),
        head_before: pending.head_before,
        head_before_detached: pending.head_before_detached,
        head_after,
        head_after_detached,
        changes,
    };

    let mut log = load_log(repo)?;

    // Idempotency guard: if a previous finalize saved the log but then failed to
    // remove the pending file, this entry.id is already recorded. Re-appending it
    // would create a duplicate undo step, so just clear the pending marker.
    if log.entries.iter().any(|existing| existing.id == entry.id) {
        remove_file(&pending_file)?;
        return Ok(());
    }

    anchor_entry(repo, &entry)?;

    // A new operation supersedes any undone entries, and retention evicts the
    // oldest ones. Collect all the now-unreachable anchor ids, but delete them
    // only *after* the log is durably saved: if `save_log` failed first, the
    // old on-disk log would still reference those entries with their
    // gc-protecting anchors already gone.
    let mut stale_ids: Vec<String> = log.entries.drain(log.cursor..).map(|e| e.id).collect();
    log.entries.push(entry);
    log.cursor = log.entries.len();
    stale_ids.extend(enforce_retention(&mut log));
    save_log(repo, &log)?;

    for id in &stale_ids {
        remove_anchors(repo, id);
    }

    remove_file(&pending_file)?;
    Ok(())
}

/// Drop a pending snapshot without recording it — used by `kin abort`, which
/// already restores the pre-operation refs itself.
pub fn discard(repo: &Repository) -> Result<()> {
    if let Err(err) = remove_file(&pending_path(repo)) {
        eprintln!("Warning: could not clear pending undo state: {err:#}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// User-facing commands.
// ---------------------------------------------------------------------------

/// Revert the most recent recorded operation.
pub fn undo(force: bool) -> Result<()> {
    let repo = crate::open_repo()?;
    let _lock = crate::state_io::RepoLock::acquire(&repo)?;
    ensure_no_operation_in_progress(&repo)?;
    finalize_inner(&repo)?;

    let mut log = load_log(&repo)?;
    if log.cursor == 0 {
        println!("Nothing to undo.");
        return Ok(());
    }

    let entry = log.entries[log.cursor - 1].clone();
    restore(&repo, &entry, Direction::Undo, force)?;
    log.cursor -= 1;
    save_log(&repo, &log)?;

    println!("Undid {}: {}", entry.op, entry.summary);
    println!("Run 'kin redo' to reapply it.");
    Ok(())
}

/// Reapply the most recently undone operation.
pub fn redo(force: bool) -> Result<()> {
    let repo = crate::open_repo()?;
    let _lock = crate::state_io::RepoLock::acquire(&repo)?;
    ensure_no_operation_in_progress(&repo)?;
    finalize_inner(&repo)?;

    let mut log = load_log(&repo)?;
    if log.cursor >= log.entries.len() {
        println!("Nothing to redo.");
        return Ok(());
    }

    let entry = log.entries[log.cursor].clone();
    restore(&repo, &entry, Direction::Redo, force)?;
    log.cursor += 1;
    save_log(&repo, &log)?;

    println!("Redid {}: {}", entry.op, entry.summary);
    Ok(())
}

/// Print recent operations, newest first, marking the current position.
pub fn reflog() -> Result<()> {
    let repo = crate::open_repo()?;
    let _lock = crate::state_io::RepoLock::acquire(&repo)?;
    // Guard before finalizing, exactly like undo/redo: finalizing while an
    // operation is mid-flight (e.g. a sync paused on a conflict) would snapshot
    // the half-applied state as a bogus entry and delete the pending marker,
    // corrupting the log.
    ensure_no_operation_in_progress(&repo)?;
    finalize_inner(&repo)?;

    let log = load_log(&repo)?;
    if log.entries.is_empty() {
        println!("No operations recorded yet.");
        return Ok(());
    }

    let (_, now_secs) = now();

    println!("Kindra operation log (newest first):");
    for (idx, entry) in log.entries.iter().enumerate().rev() {
        // Entries before the cursor are applied; at/after it are undone.
        let marker = if idx == log.cursor.wrapping_sub(1) {
            "* " // current tip of history
        } else if idx >= log.cursor {
            "↑ " // undone, available to redo
        } else {
            "  "
        };
        println!(
            "{marker}{} ago  {:<8} {}",
            format_age(now_secs.saturating_sub(entry.time)),
            entry.op,
            entry.summary,
        );
    }

    if log.cursor == 0 {
        println!(
            "\nAll recorded operations have been undone. 'kin redo' reapplies the oldest undone one."
        );
    } else if log.cursor < log.entries.len() {
        println!("\n* = current state   ↑ = undone (redoable with 'kin redo')");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Restore engine.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// Move backwards: restore each branch to its `pre` tip.
    Undo,
    /// Move forwards: restore each branch to its `post` tip.
    Redo,
}

impl Direction {
    /// The tip this direction restores a branch to.
    fn target(self, change: &Change) -> &Option<String> {
        match self {
            Direction::Undo => &change.pre,
            Direction::Redo => &change.post,
        }
    }

    /// The tip a branch is expected to currently hold for this move to be safe.
    fn expected(self, change: &Change) -> &Option<String> {
        match self {
            Direction::Undo => &change.post,
            Direction::Redo => &change.pre,
        }
    }
}

fn restore(repo: &Repository, entry: &Entry, dir: Direction, force: bool) -> Result<()> {
    // A ref move that also rewinds the working tree must not silently discard
    // uncommitted work.
    if !force && crate::rebase_utils::working_tree_dirty(repo)? {
        return Err(anyhow!(
            "Working tree has uncommitted changes; refusing to move branches. \
             Commit or stash them, or rerun with --force to discard them."
        ));
    }

    // Refuse if the stack has drifted since the operation, so we never clobber
    // work done after it. `--force` overrides.
    if !force {
        let mut drifted = Vec::new();
        for (branch, change) in &entry.changes {
            let expected = dir.expected(change);
            let current = branch_oid(repo, branch);
            if &current != expected {
                drifted.push(branch.clone());
            }
        }
        if !drifted.is_empty() {
            drifted.sort();
            return Err(anyhow!(
                "These branches changed since the operation: {}. \
                 Rerun with --force to overwrite them anyway.",
                drifted.join(", ")
            ));
        }
    }

    let (head_str, head_detached) = match dir {
        Direction::Undo => (&entry.head_before, entry.head_before_detached),
        Direction::Redo => (&entry.head_after, entry.head_after_detached),
    };

    // Where HEAD's content should end up once refs are moved.
    let head_oid = resolve_head_oid(repo, entry, dir, head_str, head_detached)?;

    // Detach onto the target commit first. This frees every branch name (so we
    // can delete or recreate any of them) and moves the working tree exactly
    // once, to its final position. Only force the checkout when the caller opted
    // in with `--force`: a plain checkout still refuses to clobber untracked
    // files obstructing the target, which the dirty-tree guard above misses
    // because `working_tree_dirty` ignores untracked paths.
    let head_oid_str = head_oid.to_string();
    let mut detach_args = vec!["checkout", "--quiet"];
    if force {
        detach_args.push("--force");
    }
    detach_args.push("--detach");
    detach_args.push(&head_oid_str);
    git(&detach_args).context("Failed to reposition HEAD while restoring")?;

    // Move the branch refs with `git update-ref --stdin`. Prefer a *single* atomic
    // transaction (deletions and creates together) so a mid-restore failure can
    // never leave some branches moved and others not. (Branch names can't contain
    // spaces or newlines, so the line-oriented stdin format needs no quoting.)
    let mut deletions = String::new();
    let mut updates = String::new();
    let mut deleted_names: Vec<&str> = Vec::new();
    let mut updated_names: Vec<&str> = Vec::new();
    for (branch, change) in &entry.changes {
        match dir.target(change) {
            Some(oid) => {
                updates.push_str(&format!("update refs/heads/{branch} {oid}\n"));
                updated_names.push(branch);
            }
            None if branch_oid(repo, branch).is_some() => {
                deletions.push_str(&format!("delete refs/heads/{branch}\n"));
                deleted_names.push(branch);
            }
            None => {}
        }
    }

    // The one case that can't share a transaction is a delete that
    // directory/file-conflicts with a create (e.g. deleting `foo` while creating
    // `foo/bar`): git stores refs as files, locks all refs up front, and would
    // deadlock. Those rare restores fall back to delete-then-create passes (atomic
    // per pass, but not across the two).
    let df_conflict = deleted_names
        .iter()
        .any(|d| updated_names.iter().any(|u| ref_prefix_conflict(d, u)));

    if df_conflict {
        if !deletions.is_empty() {
            git_update_refs(&deletions).context("Failed to delete branches while restoring")?;
        }
        if !updates.is_empty() {
            git_update_refs(&updates).context("Failed to restore branches")?;
        }
    } else if !deletions.is_empty() || !updates.is_empty() {
        git_update_refs(&format!("{deletions}{updates}")).context("Failed to restore branches")?;
    }

    // Reattach HEAD to the branch it was on (if any); the working tree is
    // already at the right commit from the detach above.
    if !head_detached {
        let mut reattach_args = vec!["checkout", "--quiet"];
        if force {
            reattach_args.push("--force");
        }
        reattach_args.push(head_str);
        git(&reattach_args).with_context(|| format!("Failed to switch back to '{head_str}'"))?;
    }

    Ok(())
}

/// Resolve the commit HEAD should sit on after a restore.
fn resolve_head_oid(
    repo: &Repository,
    entry: &Entry,
    dir: Direction,
    head_str: &str,
    head_detached: bool,
) -> Result<Oid> {
    if head_detached {
        return Oid::from_str(head_str)
            .with_context(|| format!("Invalid saved detached HEAD '{head_str}'"));
    }

    // HEAD was on a branch. Its tip after the restore is that branch's target
    // in this direction, or — if the operation never touched it — its current
    // tip.
    let hex = match entry.changes.get(head_str) {
        Some(change) => dir.target(change).clone(),
        None => branch_oid(repo, head_str),
    };
    let hex = hex.ok_or_else(|| {
        anyhow!("Cannot restore HEAD: branch '{head_str}' has no known commit to return to")
    })?;
    Oid::from_str(&hex).with_context(|| format!("Invalid saved tip '{hex}' for '{head_str}'"))
}

// ---------------------------------------------------------------------------
// Anchoring: keep pre/post commits alive against git gc.
// ---------------------------------------------------------------------------

fn anchor_entry(repo: &Repository, entry: &Entry) -> Result<()> {
    let namespace = anchor_namespace(&entry.id);
    let mut oids = BTreeSet::new();
    for change in entry.changes.values() {
        if let Some(oid) = &change.pre {
            oids.insert(oid.clone());
        }
        if let Some(oid) = &change.post {
            oids.insert(oid.clone());
        }
    }
    // A detached HEAD records its tip as an OID (not a branch name); anchor it so
    // the commit survives gc until a restore reattaches to it. Skip the empty
    // unborn-branch sentinel (`current_head` returns "" + detached for that).
    if entry.head_before_detached && !entry.head_before.is_empty() {
        oids.insert(entry.head_before.clone());
    }
    if entry.head_after_detached && !entry.head_after.is_empty() {
        oids.insert(entry.head_after.clone());
    }

    for hex in oids {
        let oid = Oid::from_str(&hex)
            .with_context(|| format!("Refusing to anchor invalid OID '{hex}'"))?;
        // Name the anchor after the OID itself: unique, collision-free, and no
        // branch-name sanitization needed.
        repo.reference(
            &format!("{namespace}/{hex}"),
            oid,
            true,
            "kindra undo anchor",
        )
        .with_context(|| format!("Failed to anchor commit {hex}"))?;
    }
    Ok(())
}

fn remove_anchors(repo: &Repository, id: &str) {
    let glob = format!("{}/*", anchor_namespace(id));
    let Ok(refs) = repo.references_glob(&glob) else {
        return;
    };
    for mut reference in refs.flatten() {
        // Best-effort: a leftover anchor is harmless, only mild gc pressure.
        let _ = reference.delete();
    }
}

// ---------------------------------------------------------------------------
// Snapshot / diff helpers.
// ---------------------------------------------------------------------------

fn all_local_branches(repo: &Repository) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for branch in repo.branches(Some(BranchType::Local))? {
        let (branch, _) = branch?;
        let Some(name) = branch.name()? else {
            continue;
        };
        if let Some(oid) = branch.get().target() {
            map.insert(name.to_string(), oid.to_string());
        }
    }
    Ok(map)
}

fn branch_oid(repo: &Repository, name: &str) -> Option<String> {
    match repo.find_branch(name, BranchType::Local) {
        Ok(branch) => branch.get().target().map(|oid| oid.to_string()),
        Err(_) => None,
    }
}

fn diff_branches(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> BTreeMap<String, Change> {
    let mut changes = BTreeMap::new();
    let names: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    for name in names {
        let pre = before.get(name).cloned();
        let post = after.get(name).cloned();
        if pre != post {
            changes.insert(name.clone(), Change { pre, post });
        }
    }
    changes
}

fn current_head(repo: &Repository) -> Result<(String, bool)> {
    if repo.head_detached()? {
        let oid = repo.head()?.peel_to_commit()?.id();
        Ok((oid.to_string(), true))
    } else {
        match repo.head() {
            Ok(head) => {
                let name = head
                    .shorthand()
                    .ok_or_else(|| anyhow!("Could not read current branch name"))?;
                Ok((name.to_string(), false))
            }
            // Unborn branch (fresh repo, no commits): treat as detached-at-nothing.
            Err(e) if e.code() == ErrorCode::UnbornBranch => Ok((String::new(), true)),
            Err(e) => Err(anyhow!(e).context("Could not read HEAD")),
        }
    }
}

fn summarize(op: &str, changes: &BTreeMap<String, Change>) -> String {
    let mut created = 0usize;
    let mut deleted = 0usize;
    let mut modified = Vec::new();
    for (name, change) in changes {
        match (&change.pre, &change.post) {
            (None, Some(_)) => created += 1,
            (Some(_), None) => deleted += 1,
            _ => modified.push(name.clone()),
        }
    }

    let mut parts = Vec::new();
    if !modified.is_empty() {
        let shown = modified
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let extra = modified.len().saturating_sub(3);
        if extra > 0 {
            parts.push(format!("{shown} +{extra} more rebased"));
        } else {
            parts.push(format!("{shown} rebased"));
        }
    }
    if created > 0 {
        parts.push(format!("{created} created"));
    }
    if deleted > 0 {
        parts.push(format!("{deleted} deleted"));
    }

    if parts.is_empty() {
        op.to_string()
    } else {
        parts.join(", ")
    }
}

// ---------------------------------------------------------------------------
// Repository state helpers.
// ---------------------------------------------------------------------------

/// True when a Kindra-managed operation (or a raw git rebase) is mid-flight, so
/// its saved state — and any pending undo snapshot — must survive for the
/// resuming `kin continue` / `kin abort` process rather than being settled now.
fn operation_in_progress(repo: &Repository) -> bool {
    crate::rebase_utils::state_path(repo).exists()
        || crate::commands::run::run_state_exists(repo)
        || crate::rebase_utils::git_rebase_in_progress(repo)
}

/// Reject undo/redo while a rebase/split/run operation is mid-flight, since its
/// half-applied refs are not a coherent state to move away from.
fn ensure_no_operation_in_progress(repo: &Repository) -> Result<()> {
    if operation_in_progress(repo) {
        return Err(anyhow!(
            "A Kindra operation is in progress. Finish it with 'kin continue' or 'kin abort' first."
        ));
    }
    Ok(())
}

fn git(args: &[&str]) -> Result<()> {
    let status = Command::new("git").args(args).status()?;
    if !status.success() {
        return Err(anyhow!("git {} failed", args.join(" ")));
    }
    Ok(())
}

/// Apply a batch of ref changes atomically via `git update-ref --stdin`. `input`
/// is the newline-oriented command list (`update <ref> <oid>` / `delete <ref>`);
/// either every command applies or none do.
fn git_update_refs(input: &str) -> Result<()> {
    use std::io::Write;
    let mut child = Command::new("git")
        .args(["update-ref", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open git update-ref stdin"))?
        .write_all(input.as_bytes())?;
    if !child.wait()?.success() {
        return Err(anyhow!("git update-ref --stdin failed"));
    }
    Ok(())
}

/// Whether two ref names collide as a file vs a directory in git's ref store
/// (e.g. `foo` and `foo/bar`), which cannot be locked in one transaction.
fn ref_prefix_conflict(a: &str, b: &str) -> bool {
    a == b
        || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
        || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
}

// ---------------------------------------------------------------------------
// Persistence.
// ---------------------------------------------------------------------------

fn load_log(repo: &Repository) -> Result<Log> {
    match read_json::<Log>(&log_path(repo))? {
        Some(mut log) => {
            // Reject an unrecognized format rather than reusing (and later
            // overwriting) a log written by an incompatible version.
            if log.version != OPLOG_VERSION {
                return Err(anyhow!(
                    "Unsupported Kindra oplog version {} in {} (expected {}). \
                     Remove the file to reset undo history.",
                    log.version,
                    log_path(repo).display(),
                    OPLOG_VERSION
                ));
            }
            // Defend against a hand-edited or partially migrated cursor.
            if log.cursor > log.entries.len() {
                log.cursor = log.entries.len();
            }
            Ok(log)
        }
        None => Ok(Log::default()),
    }
}

fn save_log(repo: &Repository, log: &Log) -> Result<()> {
    write_json(&log_path(repo), log)
}

/// Evict the oldest entries beyond `MAX_ENTRIES`, returning their ids so the
/// caller can drop their anchors *after* the trimmed log is durably saved.
fn enforce_retention(log: &mut Log) -> Vec<String> {
    let mut dropped_ids = Vec::new();
    while log.entries.len() > MAX_ENTRIES {
        let dropped = log.entries.remove(0);
        dropped_ids.push(dropped.id);
        log.cursor = log.cursor.saturating_sub(1);
    }
    dropped_ids
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let value = serde_json::from_str(&raw)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    Ok(Some(value))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let json = serde_json::to_string_pretty(value)?;
    crate::state_io::write_atomic(path, &json)
}

fn remove_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(anyhow!("Failed to remove {}: {err}", path.display())),
    }
}

fn now() -> (u128, u64) {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (dur.as_nanos(), dur.as_secs())
}

fn format_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::process::Command;

    fn init_repo_with_commit(dir: &std::path::Path) -> (Repository, String) {
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@e")
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?} failed");
        };
        run(&["init", "-q"]);
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "c"]);
        let repo = Repository::open(dir).unwrap();
        let oid = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();
        (repo, oid)
    }

    #[test]
    fn anchor_entry_anchors_detached_head_and_skips_unborn_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, oid) = init_repo_with_commit(dir.path());

        // Entry with a detached HEAD tip (an OID) plus the empty unborn-branch
        // sentinel on the after-side, and no per-branch changes.
        let entry = Entry {
            id: "123".to_string(),
            time: 0,
            op: "test".to_string(),
            summary: String::new(),
            head_before: oid.clone(),
            head_before_detached: true,
            head_after: String::new(),
            head_after_detached: true,
            changes: BTreeMap::new(),
        };

        anchor_entry(&repo, &entry).unwrap();

        // The detached tip is anchored so gc can't prune it before restore...
        let ns = anchor_namespace("123");
        assert!(
            repo.find_reference(&format!("{ns}/{oid}")).is_ok(),
            "detached HEAD tip should be anchored"
        );
        // ...and the empty unborn sentinel produced no anchor.
        let count = repo.references_glob(&format!("{ns}/*")).unwrap().count();
        assert_eq!(count, 1, "only the real detached tip should be anchored");
    }

    #[test]
    fn finalize_is_idempotent_when_pending_already_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, _oid) = init_repo_with_commit(dir.path());

        // Simulate a prior finalize that saved the log but failed to remove the
        // pending file: the log already holds this id, yet the pending file still
        // describes a real branch change (empty snapshot vs. the live `main`).
        let id = "42".to_string();
        let existing = Entry {
            id: id.clone(),
            time: 0,
            op: "sync".to_string(),
            summary: "prior".to_string(),
            head_before: "main".to_string(),
            head_before_detached: false,
            head_after: "main".to_string(),
            head_after_detached: false,
            changes: BTreeMap::new(),
        };
        save_log(
            &repo,
            &Log {
                version: OPLOG_VERSION,
                cursor: 1,
                entries: vec![existing],
            },
        )
        .unwrap();
        write_json(
            &pending_path(&repo),
            &Pending {
                id,
                time: 0,
                op: "sync".to_string(),
                head_before: "main".to_string(),
                head_before_detached: false,
                branches: BTreeMap::new(),
            },
        )
        .unwrap();

        finalize_inner(&repo).unwrap();

        // The duplicate was not appended, and the stale pending was cleared.
        let log = load_log(&repo).unwrap();
        assert_eq!(
            log.entries.len(),
            1,
            "finalize must not duplicate the entry"
        );
        assert!(!pending_path(&repo).exists(), "pending must be cleared");
    }

    #[test]
    fn enforce_retention_evicts_oldest_and_returns_their_ids() {
        fn dummy_entry(id: usize) -> Entry {
            Entry {
                id: id.to_string(),
                time: 0,
                op: "sync".to_string(),
                summary: String::new(),
                head_before: "main".to_string(),
                head_before_detached: false,
                head_after: "main".to_string(),
                head_after_detached: false,
                changes: BTreeMap::new(),
            }
        }

        // Two entries over the cap, with the cursor at the tip.
        let over = MAX_ENTRIES + 2;
        let mut log = Log {
            version: OPLOG_VERSION,
            cursor: over,
            entries: (0..over).map(dummy_entry).collect(),
        };

        let dropped = enforce_retention(&mut log);

        // The two oldest ids are returned (so the caller can drop their anchors
        // after saving), the log is trimmed to the cap, and the cursor follows.
        assert_eq!(dropped, vec!["0".to_string(), "1".to_string()]);
        assert_eq!(log.entries.len(), MAX_ENTRIES);
        assert_eq!(log.cursor, MAX_ENTRIES);
        assert_eq!(log.entries[0].id, "2", "oldest surviving entry");
    }
}
