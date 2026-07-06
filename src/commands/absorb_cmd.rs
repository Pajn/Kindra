use crate::commands::find_upstream;
use crate::rebase_utils::{
    RebaseState, check_worktrees, clear_state, ensure_git_supports_update_refs,
    git_rebase_in_progress, local_branch_tips_in_range, passively_reconcile_rebase_state,
    restore_set_aside_changes, run_rebase_loop, save_state, stash_push_changes,
};
use crate::stack::{collect_descendants, get_stack_branches_from_merge_base};
use anyhow::{Context, Result, anyhow};
use clap::Args;
use git2::{BranchType, Oid, Repository};
use slog::Drain;
use std::collections::{HashMap, HashSet};
use std::process::Command;

#[derive(Args)]
pub struct AbsorbArgs {
    /// Don't make any actual changes
    #[arg(long, short = 'n')]
    pub dry_run: bool,
    /// Use this commit as the base of the absorb stack instead of the stack
    /// parent (must be on the current branch, at or above the stack parent)
    #[arg(long, short)]
    pub base: Option<String>,
    /// Generate fixups to commits not made by you
    #[arg(long)]
    pub force_author: bool,
    /// Match the change against the complete file
    #[arg(long, short)]
    pub whole_file: bool,
    /// Only generate one fixup per commit
    #[arg(long, short = 'F')]
    pub one_fixup_per_commit: bool,
    /// Create squash commits instead of fixup commits
    #[arg(long, short)]
    pub squash: bool,
    /// Commit message body that is given to all fixup commits
    #[arg(long, short)]
    pub message: Option<String>,
    /// Display more output from the absorb engine
    #[arg(long, short)]
    pub verbose: bool,
    /// Skip the checked-out-in-another-worktree safety check for affected branches
    #[arg(long)]
    pub force: bool,
}

/// Absorb staged changes into the current branch's commits (via the git-absorb
/// engine), fold the generated `fixup!` commits with an autosquash rebase, and
/// restack dependent branches — so an absorb never sets the stack adrift.
pub fn absorb(args: &AbsorbArgs) -> Result<()> {
    let repo = crate::open_repo()?;
    let _lock = crate::state_io::RepoLock::acquire(&repo)?;

    if passively_reconcile_rebase_state(&repo)? || crate::commands::run::run_state_exists(&repo) {
        return Err(anyhow!(
            "A Kindra operation is already in progress. Use 'kin continue' or 'kin abort'."
        ));
    }

    let head = repo.head()?;
    let current_branch_name = if !repo.head_detached()? {
        head.shorthand().map(|s| s.to_string())
    } else {
        None
    }
    .ok_or_else(|| anyhow!("You must be on a branch to use 'absorb'"))?;
    let head_before = head.peel_to_commit()?.id();

    let upstream_name = find_upstream(&repo)?.ok_or_else(|| {
        anyhow!("Could not find a base branch (init.defaultBranch, main, master, or trunk)")
    })?;
    let upstream_id = repo.revparse_single(&upstream_name)?.id();
    let merge_base = repo.merge_base(upstream_id, head_before)?;
    let stack_branches = get_stack_branches_from_merge_base(
        &repo,
        merge_base,
        head_before,
        upstream_id,
        &upstream_name,
    )
    .with_context(|| {
        format!(
            "Failed to discover stack context for current branch '{}'.",
            current_branch_name
        )
    })?;

    let mut sub_stack = Vec::new();
    collect_descendants(&repo, &current_branch_name, &stack_branches, &mut sub_stack)?;
    crate::stack::sort_branches_topologically(&repo, &mut sub_stack)?;
    let remaining_branches: Vec<String> = sub_stack
        .iter()
        .filter(|sb| sb.name != current_branch_name)
        .map(|sb| sb.name.clone())
        .collect();

    // Scope the absorb to the current branch's own commits: everything below the
    // stack parent (or the merge base for a stack root) is out of range, so a
    // fixup can never target another branch's commit. An explicit --base must
    // stay inside that range — an ancestor of HEAD at or above the stack parent
    // — because the fold rebases onto it: a base outside the branch's history
    // would relocate the branch, and one below the stack parent would rewrite
    // ancestor branches whose siblings this command does not restack.
    let stack_parent_id = crate::stack::find_parent_in_stack(
        &repo,
        &current_branch_name,
        &stack_branches,
        merge_base,
    )?;
    let base_id = match &args.base {
        Some(base) => {
            let base_id = repo
                .revparse_single(base)
                .with_context(|| format!("Could not resolve --base '{}' to a commit.", base))?
                .peel_to_commit()?
                .id();
            if base_id != head_before && !repo.graph_descendant_of(head_before, base_id)? {
                return Err(anyhow!(
                    "--base '{}' is not an ancestor of the current branch '{}'.",
                    base,
                    current_branch_name
                ));
            }
            if base_id != stack_parent_id && !repo.graph_descendant_of(base_id, stack_parent_id)? {
                return Err(anyhow!(
                    "--base '{}' is below the current branch's own commits (its stack parent is {}). \
                     To absorb into an ancestor branch's commits, run 'kin absorb' on that branch.",
                    base,
                    stack_parent_id
                ));
            }
            base_id
        }
        None => stack_parent_id,
    };
    if base_id == head_before {
        println!("No commits on '{}' to absorb into.", current_branch_name);
        return Ok(());
    }

    // The fold below rewrites base..HEAD with `--update-refs` (git >= 2.38),
    // moving every branch tip inside that range along with it — not just the
    // dependents restacked afterwards. Both sets must pass the worktree safety
    // check, or a branch checked out elsewhere would be skipped by
    // `--update-refs` and silently left on pre-fold history. A branch that
    // *forks* from inside the range would be moved by neither mechanism, so
    // that is refused up front.
    ensure_git_supports_update_refs()?;
    let in_range_tips: Vec<(String, Oid)> =
        local_branch_tips_in_range(&repo, Some(base_id), head_before)?
            .into_iter()
            .filter(|(name, _)| name != &current_branch_name)
            .collect();
    ensure_no_forks_from_rewritten_range(
        &repo,
        &current_branch_name,
        &sub_stack,
        &in_range_tips,
        base_id,
        head_before,
    )?;
    let mut guarded_branches = remaining_branches.clone();
    for (name, _) in &in_range_tips {
        if !guarded_branches.contains(name) {
            guarded_branches.push(name.clone());
        }
    }
    if !guarded_branches.is_empty() {
        check_worktrees(&guarded_branches, args.force)?;
    }

    // Snapshot for undo before the absorb engine commits anything, so `kin undo`
    // rolls the whole operation back to the pre-fixup tips. The guard settles the
    // snapshot on every exit; a no-change exit leaves no oplog entry.
    let _snapshot = crate::oplog::begin(&repo, "absorb")?;

    if let Err(err) = run_absorb_engine(args, base_id) {
        // The engine may have created some fixup commits before failing; roll
        // them back so a partial absorb doesn't linger at HEAD.
        let repo = crate::open_repo()?;
        if repo.revparse_single("HEAD")?.id() != head_before {
            return Err(rollback_fixups(head_before, None, err));
        }
        return Err(err);
    }

    if args.dry_run {
        return Ok(());
    }

    // The engine moved HEAD for every fixup it created; re-open so the handle
    // can't serve any stale state.
    let repo = crate::open_repo()?;
    let head_after = repo.revparse_single("HEAD")?.id();
    if head_after == head_before {
        // The engine already explained why nothing was absorbable.
        return Ok(());
    }
    let fixup_count =
        count_commits(&repo, base_id, head_after)? - count_commits(&repo, base_id, head_before)?;
    println!(
        "Absorbed staged changes into {} {} commit{}. Folding...",
        fixup_count,
        if args.squash { "squash" } else { "fixup" },
        if fixup_count == 1 { "" } else { "s" }
    );

    let (parent_id_map, parent_name_map) = if remaining_branches.is_empty() {
        (HashMap::new(), HashMap::new())
    } else {
        crate::stack::build_parent_maps(
            &repo,
            &sub_stack,
            &stack_branches,
            merge_base,
            head_before,
            &current_branch_name,
        )?
    };

    // Record the pre-fold tip of every branch this operation may move — the
    // current branch, its dependents, and the in-range tips moved by the
    // fold's `--update-refs` (e.g. a sibling branch sharing HEAD's commit) —
    // so `kin abort` can restore all of them.
    let mut original_tip_map = HashMap::new();
    original_tip_map.insert(current_branch_name.clone(), head_before.to_string());
    original_tip_map.extend(
        sub_stack
            .iter()
            .map(|branch| (branch.name.clone(), branch.id.to_string())),
    );
    original_tip_map.extend(
        in_range_tips
            .iter()
            .map(|(name, oid)| (name.clone(), oid.to_string())),
    );

    let mut state = RebaseState {
        operation: crate::rebase_utils::Operation::Commit,
        original_branch: current_branch_name.clone(),
        target_branch: current_branch_name.clone(),
        caller_branch: None,
        remaining_branches,
        in_progress_branch: None,
        parent_id_map,
        parent_name_map,
        new_base_map: HashMap::new(),
        original_commit_count_map: HashMap::new(),
        original_tip_map,
        owned_tip_map: HashMap::new(),
        stash_ref: None,
        // The stash below is a full stash (no --keep-index), so restoring it
        // with --index brings unabsorbable staged hunks back *staged*.
        stash_apply_index: true,
        // The absorbed changes live only in the fixup/folded commits; if `kin
        // abort` discards that history, it must first let the worktree keep
        // the content so it reappears as staged changes instead of being lost.
        preserve_content_on_abort: true,
        // squash! folds must never open a commit-message editor, including
        // when `kin continue` resumes one after a conflict.
        suppress_editor: true,
        unstage_on_restore: false,
        // The stash below empties the working tree, so the rebases never have
        // anything to autostash.
        autostash: false,
        cleanup_merged_branches: Vec::new(),
        cleanup_checkout_fallback: None,
    };

    // The absorb engine consumed the staged changes it could place; anything
    // left (unabsorbable hunks, unstaged edits, untracked files) must be set
    // aside for the rebases below. Stash it all and restore at the end via the
    // saved state, so `kin continue`/`abort` recover it after a conflict stop.
    state.stash_ref = match stash_push_changes(false, "kin-absorb") {
        Ok(stash_ref) => {
            if stash_ref.is_some() {
                println!(
                    "Set aside remaining changes; they will be restored when the operation completes."
                );
            }
            stash_ref
        }
        Err(err) => {
            return Err(rollback_fixups(head_before, None, err));
        }
    };
    if let Err(err) = save_state(&repo, &state) {
        // Persisting failed, so no later `kin continue`/`abort` knows about the
        // stash; roll the fixups back and pop it rather than stranding the
        // user's changes.
        return Err(rollback_fixups(head_before, state.stash_ref.take(), err));
    }

    // Fold the fixup commits. `--update-refs` moves every branch tip inside the
    // rewritten range with the fold; branches stacked above are restacked
    // afterwards by the rebase loop, which skips any this already moved.
    // GIT_EDITOR is pinned so `--squash` folds don't open a commit-message
    // editor per squash (the combined message is accepted as-is); the
    // suppress_editor flag in the saved state makes `kin continue` do the same
    // when resuming after a conflict.
    let status = Command::new("git")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .env("GIT_EDITOR", "true")
        .arg("rebase")
        .arg("-i")
        .arg("--autosquash")
        .arg("--update-refs")
        .arg(base_id.to_string())
        .status()?;
    if !status.success() {
        if git_rebase_in_progress(&repo) {
            // The autosquash paused on a conflict. Record which branch is
            // mid-rebase so `kin continue` matches the saved state.
            state.in_progress_branch = Some(current_branch_name.clone());
            save_state(&repo, &state)?;
            return Err(anyhow!(
                "git rebase --autosquash failed. Resolve conflicts and run 'kin continue', or run 'kin abort'."
            ));
        }
        // The fold failed without starting a rebase (e.g. a pre-rebase hook
        // rejected it). Nothing was folded, so leaving the saved state behind
        // would only invite `kin continue` to restack dependents onto the raw
        // fixup commits. Roll the fixups back and restore the set-aside
        // changes instead.
        let _ = clear_state(&repo);
        return Err(rollback_fixups(
            head_before,
            state.stash_ref.take(),
            anyhow!("git rebase --autosquash failed before starting. The absorb was rolled back."),
        ));
    }

    // Restack dependents; also restores the stash, clears the saved state, and
    // finalizes the undo snapshot (a no-dependents run only does the latter).
    run_rebase_loop(&repo, state)
}

/// Error-path rollback: pop the set-aside stash back (its base is the current
/// fixup tip, so it applies cleanly and its staged hunks come back staged),
/// then soft-reset the fixup commits off HEAD — the index and working tree
/// keep everything, so the absorbed content returns to the index and the
/// failure leaves the repository as it was before the absorb. If the reset
/// itself fails, that is surfaced on the returned error instead of guessing.
fn rollback_fixups(
    head_before: Oid,
    stash_ref: Option<String>,
    err: anyhow::Error,
) -> anyhow::Error {
    restore_set_aside_changes(stash_ref);
    let reset_ok = matches!(
        Command::new("git")
            .args(["reset", "--soft", &head_before.to_string()])
            .status(),
        Ok(status) if status.success()
    );
    if !reset_ok {
        return err.context(format!(
            "Additionally, failed to roll back the fixup commits; they remain at HEAD. Remove them with 'git reset --soft {}'.",
            head_before
        ));
    }
    err
}

/// Refuse the absorb when any local branch forks from a commit strictly inside
/// the rewritten range (base..HEAD). Such a branch would be moved by neither
/// the fold's `--update-refs` (its tip is outside the range) nor the dependent
/// restack (it does not descend from the current branch's tip), silently
/// stranding it on pre-fold history.
///
/// No branch can *point at* the fork point: any branch tip that is an ancestor
/// of HEAD is in `stack_branches`, so the closest one becomes the stack parent
/// and the base never lands below it — which is also why branches forking at
/// the base boundary carry nothing that the fold rewrites.
fn ensure_no_forks_from_rewritten_range(
    repo: &Repository,
    current_branch_name: &str,
    sub_stack: &[crate::stack::StackBranch],
    in_range_tips: &[(String, Oid)],
    base_id: Oid,
    head_before: Oid,
) -> Result<()> {
    let covered: HashSet<&str> = sub_stack
        .iter()
        .map(|branch| branch.name.as_str())
        .chain(in_range_tips.iter().map(|(name, _)| name.as_str()))
        .chain(std::iter::once(current_branch_name))
        .collect();

    for (branch, _) in repo.branches(Some(BranchType::Local))?.flatten() {
        let Ok(Some(name)) = branch.name() else {
            continue;
        };
        if covered.contains(name) {
            continue;
        }
        let Some(tip) = branch.get().target() else {
            continue;
        };
        if tip == base_id || !repo.graph_descendant_of(tip, base_id)? {
            continue;
        }
        let fork_point = repo.merge_base(tip, head_before)?;
        if fork_point == base_id {
            // Forks at the range boundary: nothing it carries is rewritten.
            continue;
        }
        return Err(anyhow!(
            "Branch '{}' forks from commit {} inside the absorbed range, so its commits cannot follow the fold. Rebase '{}' onto a branch first, or pass --base to keep the fork point out of the absorb range.",
            name,
            fork_point,
            name
        ));
    }
    Ok(())
}

/// Run the git-absorb engine with `and_rebase` disabled: it only creates
/// `fixup!` commits on HEAD; the fold and the restack stay under Kindra's
/// control. The engine reads `absorb.*` git config for anything not passed.
fn run_absorb_engine(args: &AbsorbArgs, base_id: Oid) -> Result<()> {
    let decorator = slog_term::TermDecorator::new().stderr().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = std::sync::Mutex::new(drain).fuse();
    let drain = slog::LevelFilter::new(
        drain,
        if args.verbose {
            slog::Level::Debug
        } else {
            slog::Level::Info
        },
    )
    .fuse();
    let logger = slog::Logger::root(drain, slog::o!());

    let base_str = base_id.to_string();
    let rebase_options: Vec<&str> = Vec::new();
    git_absorb::run(
        &logger,
        &git_absorb::Config {
            dry_run: args.dry_run,
            no_limit: false,
            force_author: args.force_author,
            force_detach: false,
            base: Some(base_str.as_str()),
            and_rebase: false,
            rebase_options: &rebase_options,
            whole_file: args.whole_file,
            one_fixup_per_commit: args.one_fixup_per_commit,
            squash: args.squash,
            message: args.message.as_deref(),
        },
    )
}

fn count_commits(repo: &Repository, base: Oid, tip: Oid) -> Result<usize> {
    let mut walk = repo.revwalk()?;
    walk.push(tip)?;
    walk.hide(base)?;
    Ok(walk.count())
}
