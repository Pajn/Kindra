use crate::commands::find_upstream;
use crate::rebase_utils::{
    Operation, RebaseState, Replay, checkout_branch, clear_state, git_rebase_in_progress,
    save_state,
};
use crate::stack::{
    collect_merged_local_branches, find_sync_boundary, get_stack_branches_from_merge_base,
    get_stack_tips, resolve_merge_base,
};
use anyhow::{Result, anyhow};
use clap::Args;
use git2::BranchType;
use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};

#[derive(Args, Default)]
pub struct SyncArgs {
    /// Force the sync even if branches are checked out in other worktrees
    #[arg(long)]
    pub force: bool,

    /// Do not delete merged branches
    #[arg(long)]
    pub no_delete: bool,

    /// Allow git rebase to autostash tracked worktree changes
    #[arg(long, overrides_with = "no_autostash")]
    pub autostash: bool,

    /// Disable git rebase autostash even if configured
    #[arg(long, overrides_with = "autostash")]
    pub no_autostash: bool,
}

pub fn sync(args: &SyncArgs) -> Result<()> {
    let repo = crate::open_repo()?;

    let lock = crate::state_io::RepoLock::acquire(&repo)?;
    sync_holding_lock(&repo, &lock, args)
}

/// `kin sync` for a caller that already holds the repository lock: the local
/// cascade of `kin pr merge`.
pub(crate) fn sync_holding_lock(
    repo: &git2::Repository,
    lock: &crate::state_io::RepoLock,
    args: &SyncArgs,
) -> Result<()> {
    crate::operation_state::ensure_idle(repo, lock, crate::operation_state::Allow::NOTHING)?;
    crate::overrides::with_planned(repo, false, || sync_locked(repo, args))
}

fn sync_locked(repo: &git2::Repository, args: &SyncArgs) -> Result<()> {
    let head = repo.head()?;
    let head_id = head.peel_to_commit()?.id();
    let current_branch_name = if !repo.head_detached()? {
        head.shorthand().map(|s| s.to_string())
    } else {
        None
    };

    let upstream_name = find_upstream(repo)?.ok_or_else(|| {
        anyhow!("Could not find a base branch (init.defaultBranch, main, master, or trunk)")
    })?;
    let local_upstream = upstream_name.clone();
    let (rebase_onto_name, fetch_remote) = resolve_sync_onto(repo, &upstream_name)?;
    if let Some(remote) = fetch_remote.as_deref() {
        // The stack is discovered again after the fetch, against the fetched
        // trunk. This earlier pass, against the trunk as last fetched, only
        // picks the remote branches to refresh in the same fetch; a branch it
        // misses or adds costs a stale or an extra remote-tracking ref, never a
        // wrong rebase. If the stack can't be found this early (the trunk as
        // last fetched may share no history with HEAD), fetch everything.
        let stack = if current_branch_name.as_deref() == Some(&upstream_name) {
            Some(Vec::new())
        } else {
            discover_stack(
                repo,
                head_id,
                current_branch_name.as_deref(),
                &rebase_onto_name,
            )
            .ok()
            .map(|(_, stack)| stack)
        };
        fetch_sync_remote(repo, remote, &rebase_onto_name, stack.as_deref())?;
    }

    // Snapshot for undo only after the preflight (upstream discovery, remote
    // fetch) has succeeded, so a failed preflight never leaves a stale pending
    // snapshot. The guard settles it on every exit below — the rebase path, the
    // delete-only path, the up-to-date no-op, and `sync_upstream_branch` — unless
    // a rebase is left in progress for `kin continue` / `kin abort` to settle.
    let _snapshot = crate::oplog::begin(repo, "sync")?;

    if current_branch_name.as_deref() == Some(&upstream_name) {
        return sync_upstream_branch(repo, args, &upstream_name, &rebase_onto_name);
    }

    let (merge_base, stack_branches) = discover_stack(
        repo,
        head_id,
        current_branch_name.as_deref(),
        &rebase_onto_name,
    )?;
    let distinct_tips: std::collections::HashSet<_> = get_stack_tips(repo, &stack_branches)?
        .iter()
        .map(|name| repo.revparse_single(name).map(|o| o.id()))
        .collect::<Result<_, _>>()?;
    if distinct_tips.len() > 1 {
        return sync_tree(
            repo,
            args,
            &stack_branches,
            merge_base,
            &rebase_onto_name,
            &local_upstream,
            current_branch_name.as_deref(),
        );
    }

    let mut tips = get_stack_tips(repo, &stack_branches)?;
    tips.sort();
    let top_branch = match tips.len() {
        0 => {
            if let Some(ref name) = current_branch_name {
                name.clone()
            } else {
                println!("No branches found in the current stack.");
                return Ok(());
            }
        }
        1 => tips[0].clone(),
        // Only co-located tips remain here; one rebase carries their refs together.
        _ => current_branch_name
            .as_ref()
            .filter(|current| tips.contains(current))
            .unwrap_or(&tips[0])
            .clone(),
    };

    let top_branch_tip = repo.revparse_single(&top_branch)?.id();

    let boundary = find_sync_boundary(repo, &top_branch, &rebase_onto_name, &stack_branches)?;
    if let Some(old_base) = boundary.old_base {
        crate::rebase_utils::ensure_replay_keeps_merged_branches(
            repo,
            Some(old_base),
            top_branch_tip,
            Some(upstream_id),
            true,
        )?;
    }

    let mut merged_branches = if args.no_delete {
        Vec::new()
    } else {
        boundary.merged_branches.clone()
    };
    crate::rebase_utils::keep_merged_branches_checked_out_elsewhere(&mut merged_branches)?;

    // Merged branches sit at or below the rebase's old base, so only the rest
    // of the stack is rewritten and must not be checked out elsewhere.
    let branches_to_check = stack_branches
        .iter()
        .filter(|sb| !boundary.merged_branches.contains(&sb.name))
        .map(|sb| sb.name.clone())
        .collect::<Vec<_>>();
    crate::rebase_utils::check_worktrees(&branches_to_check, args.force)?;

    if let Some(old_base) = boundary.old_base {
        crate::rebase_utils::ensure_git_supports_update_refs()?;
        let autostash =
            crate::commands::resolve_and_check_autostash(repo, args.autostash, args.no_autostash)?;

        let mut state = RebaseState {
            operation: Operation::Sync,
            replay: Some(Replay::SyncLinear),
            original_branch: top_branch.clone(),
            target_branch: rebase_onto_name.clone(),
            caller_branch: current_branch_name
                .clone()
                .filter(|branch| branch != &top_branch),
            remaining_branches: vec![top_branch.clone()],
            in_progress_branch: None,
            // For sync, parent_id_map stores the old rebase base for recovery/rollback,
            // not the normal branch-parent relationship used by move/reorder.
            parent_id_map: HashMap::from([(top_branch.clone(), old_base.to_string())]),
            parent_name_map: HashMap::new(),
            new_base_map: HashMap::new(),
            original_commit_count_map: HashMap::new(),
            original_tip_map: HashMap::from([(top_branch.clone(), top_branch_tip.to_string())]),
            owned_tip_map: HashMap::new(),
            set_asides: Default::default(),
            preserve_content_on_abort: false,
            suppress_editor: false,
            abort_only: false,
            unstage_on_restore: false,
            autostash,
            cleanup_merged_branches: merged_branches.clone(),
            cleanup_checkout_fallback: Some(local_upstream.clone()),
        };

        crate::overrides::prepare(repo, &crate::rebase_utils::override_plan(repo, &state)?)?;

        // Git's autostash runs too late to protect the checkout to the tip.
        // Keep these changes in Kindra's state until we return to the caller,
        // including across rebase conflicts and aborts.
        if state.caller_branch.is_some() {
            if let Some(taken) = crate::set_aside::take_tracked(
                repo,
                autostash,
                crate::set_aside::Restore::WithIndex,
            )? {
                state.set_asides.push(taken);
            }
            state.autostash = false;
        }
        if let Err(err) = save_state(repo, &state) {
            crate::set_aside::restore_or_warn(state.set_asides.take_changes());
            return Err(err);
        }

        if current_branch_name.as_deref() != Some(top_branch.as_str()) {
            checkout_branch(&top_branch)?;
        }

        let mut rebase = Command::new("git");
        rebase
            .arg("rebase")
            .arg("--reapply-cherry-picks")
            .arg("--empty=keep")
            .arg(if state.autostash {
                "--autostash"
            } else {
                "--no-autostash"
            })
            .arg("--update-refs")
            .arg("--onto")
            .arg(&rebase_onto_name)
            .arg(old_base.to_string())
            .arg(&top_branch);

        return run_sync_rebase(repo, state, rebase);
    } else {
        println!(
            "All commits in this stack appear to be integrated into {}.",
            rebase_onto_name
        );
    }

    delete_merged_branches(repo, &merged_branches, &local_upstream)?;

    // The undo guard settles the pending snapshot on return: it records the
    // merged-branch deletions (if any) or drops the snapshot when nothing
    // changed. The rebase path above defers settling to the resuming process.
    Ok(())
}

fn sync_upstream_branch(
    repo: &git2::Repository,
    args: &SyncArgs,
    upstream_name: &str,
    rebase_onto_name: &str,
) -> Result<()> {
    let mut merged_branches = if args.no_delete {
        Vec::new()
    } else {
        collect_merged_local_branches(repo, rebase_onto_name, &[upstream_name])?
    };
    crate::rebase_utils::keep_merged_branches_checked_out_elsewhere(&mut merged_branches)?;

    let upstream_id = repo.revparse_single(upstream_name)?.id();
    let rebase_onto_id = repo.revparse_single(rebase_onto_name)?.id();
    if upstream_id != rebase_onto_id {
        let rebase_root_id = repo.merge_base(upstream_id, rebase_onto_id)?;
        crate::rebase_utils::ensure_git_supports_reapply_cherry_picks()?;
        let autostash =
            crate::commands::resolve_and_check_autostash(repo, args.autostash, args.no_autostash)?;

        let state = RebaseState {
            operation: Operation::Sync,
            replay: Some(Replay::SyncLinear),
            original_branch: upstream_name.to_string(),
            target_branch: rebase_onto_name.to_string(),
            caller_branch: None,
            remaining_branches: vec![upstream_name.to_string()],
            in_progress_branch: None,
            // For sync, parent_id_map stores the old rebase base for recovery/rollback,
            // not the normal branch-parent relationship used by move/reorder.
            parent_id_map: HashMap::from([(upstream_name.to_string(), rebase_root_id.to_string())]),
            parent_name_map: HashMap::new(),
            new_base_map: HashMap::new(),
            original_commit_count_map: HashMap::new(),
            original_tip_map: HashMap::from([(upstream_name.to_string(), upstream_id.to_string())]),
            owned_tip_map: HashMap::new(),
            set_asides: Default::default(),
            preserve_content_on_abort: false,
            suppress_editor: false,
            abort_only: false,
            unstage_on_restore: false,
            autostash,
            cleanup_merged_branches: merged_branches.clone(),
            cleanup_checkout_fallback: Some(upstream_name.to_string()),
        };

        crate::overrides::prepare(repo, &crate::rebase_utils::override_plan(repo, &state)?)?;

        let mut rebase = Command::new("git");
        rebase
            .arg("rebase")
            .arg("--reapply-cherry-picks")
            .arg("--empty=keep")
            .arg(if autostash {
                "--autostash"
            } else {
                "--no-autostash"
            })
            .arg(rebase_onto_name);

        return run_sync_rebase(repo, state, rebase);
    } else {
        println!("{} is already up to date.", upstream_name);
    }

    if !args.no_delete {
        delete_merged_branches(repo, &merged_branches, upstream_name)?;
    }

    // The undo guard held by the calling `sync` settles the pending snapshot on
    // return (the rebase path defers to `finish_sync_after_rebase`).
    Ok(())
}

fn delete_merged_branches(
    repo: &git2::Repository,
    branches: &[String],
    checkout_fallback: &str,
) -> Result<()> {
    if branches.is_empty() {
        return Ok(());
    }

    let head = repo.head()?;
    let current_branch = if !repo.head_detached()? {
        head.shorthand()
    } else {
        None
    };

    if let Some(cb) = current_branch
        && branches.iter().any(|b| b == cb)
    {
        println!(
            "Current branch '{}' is merged. Switching to '{}' before deletion.",
            cb, checkout_fallback
        );
        let mut plan = crate::overrides::Plan::default();
        crate::overrides::prepare(repo, plan.checkout_rev(repo, checkout_fallback))?;
        checkout_branch(checkout_fallback).map_err(|e| {
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
            .find_branch(branch_name, BranchType::Local)
            .ok()
            .and_then(|b| b.get().target())
            .map(|oid| oid.to_string());

        let status = Command::new("git")
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

fn run_sync_rebase(
    repo: &git2::Repository,
    mut state: RebaseState,
    mut rebase: Command,
) -> Result<()> {
    state.in_progress_branch = Some(state.original_branch.clone());
    save_state(repo, &state)?;

    let status = rebase.status()?;
    if status.success() {
        return finish_sync_after_rebase(repo, state);
    }

    if git_rebase_in_progress(repo) {
        save_state(repo, &state)?;
        return Err(anyhow!(
            "git rebase failed during sync. Resolve conflicts and run 'kin continue' or 'kin abort'."
        ));
    }

    state.in_progress_branch = None;
    save_state(repo, &state)?;
    Err(anyhow!(
        "git rebase failed before sync could enter an in-progress state. Run 'kin abort' to clear the saved state, then run 'kin sync' again (or otherwise fix the rebase)."
    ))
}

pub(crate) fn finish_sync_after_rebase(
    repo: &git2::Repository,
    mut state: RebaseState,
) -> Result<()> {
    if state.replay() == Replay::SyncLinear {
        ensure_sync_rebase_completed(repo, &state)?;
    }
    // The tip drives --update-refs, but the caller should return to the branch
    // they synced from. Restore before cleanup so a merged caller uses the
    // normal checkout fallback, and keep recovery state if checkout fails.
    if let Some(caller) = &state.caller_branch {
        checkout_branch(caller)?;
    }
    crate::rebase_utils::restore_state_stash(repo, &mut state)?;
    clear_state(repo)?;

    let checkout_fallback = state
        .cleanup_checkout_fallback
        .as_deref()
        .unwrap_or(state.target_branch.as_str());
    let delete_result =
        delete_merged_branches(repo, &state.cleanup_merged_branches, checkout_fallback);
    // Finalize from the post-rebase branch state even if deletion failed, so the
    // pending snapshot is never orphaned after `clear_state`. Surface the deletion
    // error afterwards.
    crate::oplog::finalize(repo)?;
    delete_result
}

fn ensure_sync_rebase_completed(repo: &git2::Repository, state: &RebaseState) -> Result<()> {
    let original_tip = repo.revparse_single(&state.original_branch)?.id();
    let target_tip = repo.revparse_single(&state.target_branch)?.id();
    let completed =
        original_tip == target_tip || repo.graph_descendant_of(original_tip, target_tip)?;

    if completed {
        return Ok(());
    }

    Err(anyhow!(
        "Sync did not complete: '{}' is not rebased onto '{}'. If the Git rebase was aborted manually, run 'kin abort' to clear the saved sync state or rerun 'kin sync'.",
        state.original_branch,
        state.target_branch
    ))
}

/// The branches sync rebases, with their merge base with the trunk `onto`: the
/// stack between the trunk and HEAD, widened to the connected component when
/// HEAD is one of its branches, so cousins joined through private ancestor
/// branches are included even from a leaf.
fn discover_stack(
    repo: &git2::Repository,
    head_id: git2::Oid,
    current_branch: Option<&str>,
    onto: &str,
) -> Result<(git2::Oid, Vec<crate::stack::StackBranch>)> {
    let upstream_id = repo.revparse_single(onto)?.id();
    let merge_base = resolve_merge_base(repo, upstream_id, head_id)?;
    let stack_branches =
        get_stack_branches_from_merge_base(repo, merge_base, head_id, upstream_id, onto)?;
    let stack_branches = if let Some(current) = current_branch
        && stack_branches.iter().any(|b| b.name == current)
    {
        crate::stack::collect_stack_component(repo, current, merge_base, upstream_id, onto)?
    } else {
        stack_branches
    };
    Ok((merge_base, stack_branches))
}

fn resolve_sync_onto(
    repo: &git2::Repository,
    upstream_name: &str,
) -> Result<(String, Option<String>)> {
    if let Ok(branch) = repo.find_branch(upstream_name, BranchType::Local)
        && let Ok(upstream_branch) = branch.upstream()
        && let Some(upstream_ref) = upstream_branch.name()?
    {
        let remote_name = repo
            .branch_remote_name(upstream_branch.get().name().unwrap())
            .ok()
            .and_then(|buf| buf.as_str().map(|s| s.to_string()));
        return Ok((upstream_ref.to_string(), remote_name));
    }

    let remotes = repo.remotes()?;
    let remote_names: Vec<String> = remotes.iter().flatten().map(|s| s.to_string()).collect();
    if let Some((prefix, _)) = upstream_name.split_once('/')
        && remote_names.iter().any(|remote| remote == prefix)
    {
        return Ok((upstream_name.to_string(), Some(prefix.to_string())));
    }

    let origin_candidate = format!("origin/{upstream_name}");
    if repo.revparse_single(&origin_candidate).is_ok() {
        return Ok((origin_candidate, Some("origin".to_string())));
    }

    if remote_names.len() == 1 {
        let only_remote_candidate = format!("{}/{}", remote_names[0], upstream_name);
        if repo.revparse_single(&only_remote_candidate).is_ok() {
            return Ok((only_remote_candidate, Some(remote_names[0].clone())));
        }
    }

    Ok((upstream_name.to_string(), None))
}

/// A remote ref and the remote-tracking ref the remote's fetch refspec maps
/// it to.
struct TrackedRef {
    source: String,
    tracking: String,
    force: bool,
}

impl TrackedRef {
    /// The command-line refspec that fetches exactly this ref, forcing the
    /// update only when the remote's configured refspec does.
    fn refspec(&self) -> String {
        let force = if self.force { "+" } else { "" };
        format!("{force}{}:{}", self.source, self.tracking)
    }
}

/// Refresh, from `remote_name`, the trunk's remote-tracking ref `onto` and the
/// remote-tracking refs of the `stack` branches that track that remote.
///
/// The trunk's ref is the only remote state sync itself reads: the rebase goes
/// onto it, and merged or squash-merged branches are detected by comparing
/// their content with it. The stack branches' refs are refreshed so that
/// `kin tree` and `kin push` compare against the remote as it is after the
/// sync. Nothing else is fetched: no other remote branches and no tags. On a
/// remote with thousands of branches, advertising and negotiating every ref
/// is most of what a full fetch costs.
///
/// The refs come from the remote's own fetch refspecs (for the default
/// layout, `+refs/heads/main:refs/remotes/origin/main`) and are fetched
/// together in one `git fetch`; see [`targeted_fetch`] for how a stack branch
/// deleted on the remote is skipped.
///
/// The full `git fetch <remote>` remains the fallback when the targeted fetch
/// cannot be built safely (the stack could not be found before fetching, no
/// fetch refspec of the remote maps to `onto`, or the remote has negative
/// refspecs whose exclusions this does not evaluate) and when it fails for any
/// reason other than a missing stack branch, for instance because the trunk no
/// longer exists on the remote. The full fetch then behaves as sync always
/// has, keeping the ref's last-fetched value.
fn fetch_sync_remote(
    repo: &git2::Repository,
    remote_name: &str,
    onto: &str,
    stack: Option<&[crate::stack::StackBranch]>,
) -> Result<()> {
    if let Some(stack) = stack
        && let Some(trunk) = trunk_tracked_ref(repo, remote_name, onto)
    {
        let branches = stack_tracked_refs(repo, remote_name, &trunk.tracking, stack);
        if targeted_fetch(repo, remote_name, &trunk, branches)? {
            return Ok(());
        }
    }

    let status = Command::new("git").arg("fetch").arg(remote_name).status()?;
    if !status.success() {
        return Err(anyhow!(
            "git fetch failed for remote '{}' while preparing sync.",
            remote_name
        ));
    }

    Ok(())
}

/// Fetch `trunk` and `branches` from `remote_name` with exact refspecs,
/// returning whether it succeeded.
///
/// Git fails the whole fetch when a ref named on the command line is missing
/// on the remote, which is normal for a stack branch whose pull request was
/// merged and its branch deleted. It names the first missing ref and stops,
/// so the fetch is retried without each stack branch it reports, one retry
/// per deleted branch. The branch's remote-tracking ref is left as it was:
/// nothing is pruned. A missing trunk, or any other failure, returns `false`
/// so the caller falls back to the full fetch.
///
/// The fetch runs quietly with its stderr captured, so a failure the retry or
/// the full fetch recovers from is not reported; the refs it updated are
/// summarised instead. It runs in the C locale so the missing-ref message can
/// be recognised.
fn targeted_fetch(
    repo: &git2::Repository,
    remote_name: &str,
    trunk: &TrackedRef,
    mut branches: Vec<TrackedRef>,
) -> Result<bool> {
    let before: HashMap<String, Option<git2::Oid>> = std::iter::once(trunk)
        .chain(&branches)
        .map(|r| (r.tracking.clone(), ref_target(repo, &r.tracking)))
        .collect();

    loop {
        let output = Command::new("git")
            .args(["fetch", "--quiet", "--no-tags", remote_name])
            .args(
                std::iter::once(trunk)
                    .chain(&branches)
                    .map(TrackedRef::refspec),
            )
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;
        if output.status.success() {
            break;
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Only stack branches are ever dropped: a missing trunk leaves this
        // empty, and so does any other failure.
        let missing = missing_remote_refs(&stderr, &branches);
        if missing.is_empty() {
            return Ok(false);
        }
        branches.retain(|branch| !missing.contains(&branch.source));
    }

    for r in std::iter::once(trunk).chain(&branches) {
        let old = before.get(&r.tracking).copied().flatten();
        let Some(new) = ref_target(repo, &r.tracking) else {
            continue;
        };
        if old == Some(new) {
            continue;
        }
        let short = r
            .tracking
            .strip_prefix("refs/remotes/")
            .unwrap_or(&r.tracking);
        let new = &new.to_string()[..7];
        match old {
            Some(old) => println!("Fetched {short}: {}..{new}", &old.to_string()[..7]),
            None => println!("Fetched {short}: {new}"),
        }
    }
    Ok(true)
}

/// The sources among `branches` that a failed `git fetch` reported missing on
/// the remote. Git reports each as `couldn't find remote ref <ref>`; the ref
/// is matched as a whole word on any line mentioning a remote ref, ignoring
/// surrounding quotes and punctuation, so small wording changes still match.
fn missing_remote_refs(stderr: &str, branches: &[TrackedRef]) -> HashSet<String> {
    stderr
        .lines()
        .filter(|line| line.contains("remote ref"))
        .flat_map(str::split_whitespace)
        .map(|word| word.trim_matches(|c: char| "'\"`:;,.()".contains(c)))
        .filter(|word| branches.iter().any(|branch| branch.source == *word))
        .map(str::to_string)
        .collect()
}

fn ref_target(repo: &git2::Repository, name: &str) -> Option<git2::Oid> {
    repo.find_reference(name).ok().and_then(|r| r.target())
}

/// The remote ref behind the trunk's remote-tracking ref `onto`, found by
/// running `remote_name`'s fetch refspecs in reverse so a custom layout is
/// respected. `None` when it cannot be derived safely.
fn trunk_tracked_ref(repo: &git2::Repository, remote_name: &str, onto: &str) -> Option<TrackedRef> {
    let tracking = repo
        .find_branch(onto, BranchType::Remote)
        .ok()?
        .get()
        .name()?
        .to_string();
    let remote = repo.find_remote(remote_name).ok()?;
    let mut found = None;
    for spec in remote.refspecs() {
        if spec.direction() != git2::Direction::Fetch {
            continue;
        }
        if spec.str()?.starts_with('^') {
            return None;
        }
        if found.is_none() && spec.dst_matches(&tracking) {
            let source = spec.rtransform(&tracking).ok()?.as_str()?.to_string();
            found = Some(TrackedRef {
                source,
                tracking: tracking.clone(),
                force: spec.is_force(),
            });
        }
    }
    found
}

/// For each `stack` branch whose configured upstream is on `remote_name`, the
/// remote ref it tracks (`branch.<name>.merge`) and the remote-tracking ref the
/// remote's fetch refspecs map it to. The trunk's ref and duplicates are left
/// out, as are branches tracking another remote or a ref no refspec maps.
fn stack_tracked_refs(
    repo: &git2::Repository,
    remote_name: &str,
    trunk_tracking: &str,
    stack: &[crate::stack::StackBranch],
) -> Vec<TrackedRef> {
    let Ok(remote) = repo.find_remote(remote_name) else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::from([trunk_tracking.to_string()]);
    let mut refs = Vec::new();
    for branch in stack {
        let local = format!("refs/heads/{}", branch.name);
        let tracks_remote = repo
            .branch_upstream_remote(&local)
            .ok()
            .is_some_and(|name| name.as_str() == Some(remote_name));
        if !tracks_remote {
            continue;
        }
        let Some(source) = repo
            .branch_upstream_merge(&local)
            .ok()
            .and_then(|buf| buf.as_str().map(str::to_string))
        else {
            continue;
        };
        let mapped = remote.refspecs().find_map(|spec| {
            if spec.direction() != git2::Direction::Fetch || !spec.src_matches(&source) {
                return None;
            }
            let tracking = spec.transform(&source).ok()?.as_str()?.to_string();
            Some((tracking, spec.is_force()))
        });
        if let Some((tracking, force)) = mapped
            && seen.insert(tracking.clone())
        {
            refs.push(TrackedRef {
                source,
                tracking,
                force,
            });
        }
    }
    refs
}

fn sync_tree(
    repo: &git2::Repository,
    args: &SyncArgs,
    branches: &[crate::stack::StackBranch],
    merge_base: git2::Oid,
    upstream: &str,
    local_upstream: &str,
    caller: Option<&str>,
) -> Result<()> {
    let crate::stack::TreeSyncPlan {
        remaining,
        bases,
        parents,
        merged,
    } = crate::stack::plan_tree_sync(repo, branches, upstream, merge_base)?;
    // The plan never rebases a merged branch, so only the others are checked.
    let check: Vec<_> = branches
        .iter()
        .filter(|b| !merged.contains(&b.name))
        .map(|b| b.name.clone())
        .collect();
    crate::rebase_utils::check_worktrees(&check, args.force)?;
    let mut merged = if args.no_delete { Vec::new() } else { merged };
    crate::rebase_utils::keep_merged_branches_checked_out_elsewhere(&mut merged)?;
    if remaining.is_empty() {
        return delete_merged_branches(repo, &merged, local_upstream);
    }
    let original = caller.unwrap_or(&remaining[0]).to_string();
    let autostash =
        crate::commands::resolve_and_check_autostash(repo, args.autostash, args.no_autostash)?;
    let mut state = RebaseState {
        operation: Operation::Sync,
        replay: Some(Replay::SyncTree),
        original_branch: original.clone(),
        target_branch: upstream.to_string(),
        caller_branch: Some(original.clone()),
        remaining_branches: remaining,
        in_progress_branch: None,
        parent_id_map: bases,
        parent_name_map: parents,
        new_base_map: HashMap::new(),
        original_commit_count_map: HashMap::new(),
        original_tip_map: branches
            .iter()
            .map(|b| (b.name.clone(), b.id.to_string()))
            .collect(),
        owned_tip_map: HashMap::new(),
        set_asides: Default::default(),
        preserve_content_on_abort: false,
        suppress_editor: false,
        abort_only: false,
        unstage_on_restore: false,
        autostash,
        cleanup_merged_branches: merged,
        cleanup_checkout_fallback: Some(local_upstream.to_string()),
    };
    if let Some(taken) =
        crate::set_aside::take_tracked(repo, autostash, crate::set_aside::Restore::WithIndex)?
    {
        state.set_asides.push(taken);
    }
    state.autostash = false;
    if let Err(err) = save_state(repo, &state) {
        crate::set_aside::restore_or_warn(state.set_asides.take_changes());
        return Err(err);
    }
    crate::rebase_utils::run_rebase_loop(repo, state)
}
