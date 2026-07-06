use crate::commands::{find_upstream, resolve_rebase_autostash};
use crate::rebase_utils::{
    RebaseState, check_worktrees, ensure_git_supports_update_refs, git_rebase_in_progress,
    passively_reconcile_rebase_state, run_rebase_loop, save_state,
};
use crate::stack::{collect_descendants, get_stack_branches_from_merge_base};
use anyhow::{Context, Result, anyhow};
use clap::Args;
use git2::{BranchType, Oid, Repository};
use slog::Drain;
use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Args)]
pub struct AbsorbArgs {
    /// Don't make any actual changes
    #[arg(long, short = 'n')]
    pub dry_run: bool,
    /// Use this commit as the base of the absorb stack instead of the stack parent
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
    /// Skip the checked-out-in-another-worktree safety check for dependent branches
    #[arg(long)]
    pub force: bool,
    /// Allow git rebase to autostash tracked worktree changes
    #[arg(long, overrides_with = "no_autostash")]
    pub autostash: bool,
    /// Disable git rebase autostash even if configured
    #[arg(long, overrides_with = "autostash")]
    pub no_autostash: bool,
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

    // The fold below rewrites branch tips with `--update-refs` (git >= 2.38).
    // Verify support up front, before the absorb engine creates any `fixup!`
    // commits, so an unsupported git fails cleanly with nothing to undo.
    ensure_git_supports_update_refs()?;
    if !remaining_branches.is_empty() {
        check_worktrees(&remaining_branches, args.force)?;
    }
    let autostash = resolve_rebase_autostash(
        &repo,
        crate::commands::autostash_override(args.autostash, args.no_autostash),
    )?;

    // Scope the absorb to the current branch's own commits: everything below the
    // stack parent (or the merge base for a stack root) is out of range, so a
    // fixup can never target another branch's commit.
    let base_id = match &args.base {
        Some(base) => repo
            .revparse_single(base)
            .with_context(|| format!("Could not resolve --base '{}' to a commit.", base))?
            .peel_to_commit()?
            .id(),
        None => crate::stack::find_parent_in_stack(
            &repo,
            &current_branch_name,
            &stack_branches,
            merge_base,
        )?,
    };
    if base_id == head_before {
        println!("No commits on '{}' to absorb into.", current_branch_name);
        return Ok(());
    }

    // Snapshot for undo before the absorb engine commits anything, so `kin undo`
    // rolls the whole operation back to the pre-fixup tips. The guard settles the
    // snapshot on every exit; a no-change exit leaves no oplog entry.
    let _snapshot = crate::oplog::begin(&repo, "absorb")?;

    run_absorb_engine(args, base_id)?;

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
        "Absorbed staged changes into {} fixup commit{}. Folding...",
        fixup_count,
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

    // Record the pre-fold tip of every branch the autosquash `--update-refs`
    // may move: the current branch, its dependents, and any other branch whose
    // tip sits inside the rewritten range (base..HEAD], e.g. a branch created
    // by `kin split` pointing at a mid-branch commit. Without these, `kin
    // abort` could strand such a branch on the folded history.
    let mut original_tip_map = HashMap::new();
    original_tip_map.insert(current_branch_name.clone(), head_before.to_string());
    original_tip_map.extend(
        sub_stack
            .iter()
            .map(|branch| (branch.name.clone(), branch.id.to_string())),
    );
    record_tips_in_range(&repo, base_id, head_before, &mut original_tip_map)?;

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
        unstage_on_restore: false,
        autostash,
        cleanup_merged_branches: Vec::new(),
        cleanup_checkout_fallback: None,
    };

    // The absorb engine consumed the staged changes it could place; anything
    // left (unabsorbable hunks, unstaged edits, untracked files) must be set
    // aside for the rebases below. Stash it all and restore at the end via the
    // saved state, so `kin continue`/`abort` recover it after a conflict stop.
    state.stash_ref = match stash_all_remaining_changes() {
        Ok(stash_ref) => stash_ref,
        Err(err) => {
            // Roll the fixup commits back off HEAD; a soft reset returns their
            // content to the index, so nothing is lost.
            match Command::new("git")
                .args(["reset", "--soft", &head_before.to_string()])
                .status()
            {
                Ok(status) if status.success() => return Err(err),
                _ => {
                    return Err(err.context(format!(
                        "Additionally, failed to roll back the fixup commits; they remain at HEAD. Remove them with 'git reset --soft {}'.",
                        head_before
                    )));
                }
            }
        }
    };
    if let Err(err) = save_state(&repo, &state) {
        // Persisting failed, so no later `kin continue`/`abort` knows about the
        // stash; pop it back rather than stranding the user's changes.
        if let Some(stash_ref) = state.stash_ref.take()
            && crate::rebase_utils::apply_stash(&stash_ref).is_ok()
        {
            let _ = crate::rebase_utils::drop_stash(&stash_ref);
        }
        return Err(err);
    }

    // Fold the fixup commits. `--update-refs` moves every branch tip inside the
    // rewritten range with the fold; branches stacked above are restacked
    // afterwards by the rebase loop, which skips any this already moved.
    let status = Command::new("git")
        .env("GIT_SEQUENCE_EDITOR", "true")
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
        }
        return Err(anyhow!(
            "git rebase --autosquash failed. Resolve conflicts and run 'kin continue', or run 'kin abort'."
        ));
    }

    // Restack dependents; also restores the stash, clears the saved state, and
    // finalizes the undo snapshot (a no-dependents run only does the latter).
    run_rebase_loop(&repo, state)
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

/// Record, into `original_tip_map`, the pre-fold tip of every local branch whose
/// tip lies in the range the autosquash rewrites with `--update-refs`
/// (base..head). Existing entries are preserved.
fn record_tips_in_range(
    repo: &Repository,
    base: Oid,
    head: Oid,
    original_tip_map: &mut HashMap<String, String>,
) -> Result<()> {
    let mut walk = repo.revwalk()?;
    walk.push(head)?;
    walk.hide(base)?;
    let rewritten: HashSet<Oid> = walk.filter_map(|id| id.ok()).collect();

    for (branch, _) in repo.branches(Some(BranchType::Local))?.flatten() {
        let Some(oid) = branch.get().target() else {
            continue;
        };
        if !rewritten.contains(&oid) {
            continue;
        }
        let Ok(Some(name)) = branch.name() else {
            continue;
        };
        original_tip_map
            .entry(name.to_string())
            .or_insert_with(|| oid.to_string());
    }
    Ok(())
}

/// Stash everything left in the working tree and index (the absorb engine's
/// leftovers) so the rebases below run on a clean tree. Returns the stash
/// message used as its handle, or `None` when there was nothing to stash.
fn stash_all_remaining_changes() -> Result<Option<String>> {
    let before = stash_head_ref()?;
    let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let message = format!("kin-absorb-{}-{}", std::process::id(), ts);
    let status = Command::new("git")
        .arg("stash")
        .arg("push")
        .arg("--quiet")
        .arg("--include-untracked")
        .arg("-m")
        .arg(&message)
        .status()?;
    if !status.success() {
        return Err(anyhow!("Failed to stash remaining changes."));
    }
    let after = stash_head_ref()?;
    if after != before {
        println!(
            "Set aside remaining changes; they will be restored when the operation completes."
        );
        Ok(Some(message))
    } else {
        Ok(None)
    }
}

fn stash_head_ref() -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("--verify")
        .arg("-q")
        .arg("refs/stash")
        .output()?;
    if output.status.success() {
        let ref_name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if ref_name.is_empty() {
            Ok(None)
        } else {
            Ok(Some(ref_name))
        }
    } else {
        Ok(None)
    }
}
