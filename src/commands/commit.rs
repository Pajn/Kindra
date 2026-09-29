use crate::commands::find_upstream;
use crate::rebase_utils::{
    RebaseState, check_worktrees, checkout_branch, clear_state, git_rebase_in_progress,
    local_branch_tips_in_range, record_branch_tips_in_range, run_rebase_loop, save_state,
};
use crate::set_aside::{self, Outcome, Phase, SetAside};
use crate::stack::{
    StackBranch, StackCommit, build_parent_maps, collect_descendants, collect_descendants_of_id,
    collect_sub_stack_from_id, enumerate_fixup_commits, enumerate_stack_commits,
    get_stack_branches_from_merge_base, is_on_history_of, sort_branches_topologically,
};
use anyhow::{Context, Result, anyhow};
use git2::{BranchType, Oid, Repository};
use std::collections::{HashMap, HashSet};
use std::process::Command;

pub fn commit(args: &[String]) -> Result<()> {
    let repo = crate::open_repo()?;
    let lock = crate::state_io::RepoLock::acquire(&repo)?;
    let active =
        crate::operation_state::ensure_idle(&repo, &lock, crate::operation_state::Allow::COMMIT)?;
    let parsed = parse_commit_args(args)?;
    ensure_commit_can_finish(&repo, active.native, &parsed)?;
    crate::overrides::with_planned_during(&repo, false, active.native, || {
        commit_locked(&repo, parsed)
    })
}

/// `kin commit` may make the final commit of a resolved native merge (a merge
/// commit) or single-commit cherry-pick, exactly as `git commit` would, and
/// then restack dependents. Anything else would rewrite or switch branches
/// underneath the native operation, so it is refused before any state exists.
fn ensure_commit_can_finish(
    repo: &Repository,
    native: crate::operation_state::NativeOperation,
    parsed: &ParsedCommitArgs,
) -> Result<()> {
    if native == crate::operation_state::NativeOperation::None {
        return Ok(());
    }
    let advice = native.advice();
    if repo.index()?.has_conflicts() {
        return Err(anyhow!(
            "{advice} It has unmerged paths: resolve and stage them, then run 'kin commit' to finish it."
        ));
    }
    if repo.state() == git2::RepositoryState::CherryPickSequence {
        return Err(anyhow!(
            "{advice} 'kin commit' finishes only a single-commit cherry-pick."
        ));
    }
    let rewrites = parsed.on_target.is_some()
        || parsed.interactive
        || parsed.fixup_target.is_some()
        || parsed.new_branch.is_some()
        || forwards_non_resolution_commit_flag(&parsed.git_commit_args)
        || has_forwarded_pathspec(&parsed.git_commit_args);
    if rewrites {
        return Err(anyhow!(
            "{advice} 'kin commit' can also finish it, but only by committing the staged resolution on the current branch (without --on, --fixup, --interactive, --new-branch, --amend, --all, --patch, --dry-run or pathspecs)."
        ));
    }
    Ok(())
}

fn commit_locked(repo: &git2::Repository, mut parsed: ParsedCommitArgs) -> Result<()> {
    let head = match repo.head() {
        Ok(head) => head,
        Err(err) if err.code() == git2::ErrorCode::UnbornBranch => {
            // Before the initial commit there is no base or stack to discover.
            // Keep argument parsing and operation guards, then let Git create HEAD.
            if parsed.on_target.is_some()
                || parsed.interactive
                || parsed.fixup_target.is_some()
                || parsed.new_branch.is_some()
            {
                return Err(anyhow!(
                    "Create an initial commit before using --on, --interactive, --fixup, or --new-branch."
                ));
            }
            let status = Command::new("git")
                .arg("commit")
                .args(&parsed.git_commit_args)
                .status()?;
            if !status.success() {
                return Err(anyhow!("git commit failed"));
            }
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };
    let current_branch_name = if !repo.head_detached()? {
        head.shorthand().map(|s| s.to_string())
    } else {
        None
    }
    .ok_or_else(|| anyhow!("You must be on a branch to use 'commit'"))?;

    let upstream_name = find_upstream(repo)?.ok_or_else(|| {
        anyhow!("Could not find a base branch (init.defaultBranch, main, master, or trunk)")
    })?;
    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let upstream_id = upstream_obj.id();
    let head_id = head.peel_to_commit()?.id();
    let on_flag = parsed.on_target.is_some();

    // `-b`/`--new-branch` commits onto a freshly created branch rather than an
    // existing one, so it has its own flow (branch creation, name slugging, and
    // the optional `--insert` restack) separate from the target-resolution logic
    // below.
    if parsed.new_branch.is_some() {
        return commit_on_new_branch(
            repo,
            &current_branch_name,
            &upstream_name,
            upstream_id,
            head_id,
            &parsed,
        );
    }

    let current_stack = build_stack_context(repo, head_id, upstream_id, &upstream_name)
        .with_context(|| {
            format!(
                "Failed to discover stack context for current branch '{}'.",
                current_branch_name
            )
        })?;

    let interactive_selection = if parsed.interactive {
        let commits = enumerate_stack_commits(repo, &current_stack.stack_branches, &upstream_name)?;
        Some(select_commit_interactive(&commits)?)
    } else if let Some(fixup_target) = &parsed.fixup_target {
        let commits = enumerate_fixup_commits(
            repo,
            &current_stack.stack_branches,
            &upstream_name,
            &current_branch_name,
            head_id,
        )?;
        Some(resolve_fixup_commit(repo, &commits, fixup_target)?)
    } else {
        None
    };

    let mut is_fixup = false;
    let mut fixup_commit_id = String::new();
    // When true, fold the staged changes into the target *in place* on the current
    // branch (commit a `fixup!` here, then autosquash the range with
    // `--update-refs`) instead of checking out the target's branch and carrying
    // staged changes across a possibly-diverged tree. See the autosquash below.
    let mut inline_fixup = false;

    // Every picked commit is treated the same: fold the staged changes into it,
    // never reword. The current tip is folded with an in-place amend; a commit
    // below HEAD is folded via fixup + autosquash without checking out its branch;
    // any other commit (a sibling stack, or another branch sharing HEAD's commit)
    // is folded via the checkout path, which rewrites the selected branch and then
    // restacks its dependents onto the folded commit.
    if let Some(sel) = &interactive_selection {
        if sel.commit_id == head_id && sel.branch_name == current_branch_name {
            if !parsed.git_commit_args.iter().any(|arg| arg == "--amend") {
                insert_generated_commit_arg(&mut parsed.git_commit_args, "--amend".to_string());
            }
            if !parsed.git_commit_args.iter().any(|arg| arg == "--no-edit") {
                insert_generated_commit_arg(&mut parsed.git_commit_args, "--no-edit".to_string());
            }
        } else {
            is_fixup = true;
            fixup_commit_id = sel.commit_id.to_string();
            // A commit *strictly* below HEAD is folded in place; `--update-refs` on
            // the autosquash moves the branch tips at/below HEAD, and branches
            // stacked above HEAD are restacked afterwards by the rebase loop. A
            // commit that is HEAD but on another branch (a shared-head sibling)
            // isn't below HEAD, so it takes the checkout path: the selected branch
            // is rewritten and its dependents are restacked to follow it.
            inline_fixup =
                sel.commit_id != head_id && repo.graph_descendant_of(head_id, sel.commit_id)?;
            insert_generated_commit_arg(
                &mut parsed.git_commit_args,
                format!("--fixup={fixup_commit_id}"),
            );
        }
    }

    // A picked commit folds the staged changes into the target, so a non-empty
    // index is required unless `-a`/`-p`/a pathspec supplies the content instead.
    if interactive_selection.is_some()
        && requires_staged_changes(&parsed.git_commit_args)
        && !has_staged_changes(repo)?
    {
        return Err(anyhow!("nothing to commit, working tree clean"));
    }

    let requested_target = match &interactive_selection {
        // An inline fixup stays on the current branch: it folds into an ancestor
        // and restacks descendants, never switching to the target's branch.
        Some(_) if inline_fixup => current_branch_name.clone(),
        Some(sel) => sel.branch_name.clone(),
        None => match parsed.on_target {
            None => current_branch_name.clone(),
            Some(Some(ref branch_name)) => branch_name.clone(),
            Some(None) => select_target_branch(
                repo,
                &current_branch_name,
                head_id,
                &current_stack.stack_branches,
            )?,
        },
    };

    repo.find_branch(&requested_target, BranchType::Local)
        .with_context(|| format!("Target branch '{}' not found.", requested_target))?;
    let requested_target_old_head_id = repo.revparse_single(&requested_target)?.id();

    // `--on` an ancestor branch needs no branch switch at all: commit here, then
    // let an `--update-refs` rebase replay `<target>..HEAD` with the new commit
    // moved to the bottom, which claims the target's ref for it. Same in-place
    // technique as an inline fixup, and for the same reason — carrying staged
    // changes across a diverged tree is what `git checkout` refuses to do.
    let ancestor_on_target = ancestor_move_target(
        repo,
        &parsed,
        interactive_selection.is_some(),
        &requested_target,
        requested_target_old_head_id,
        &current_branch_name,
        head_id,
        &upstream_name,
        upstream_id,
        &current_stack.stack_branches,
    )?;
    let moving_onto_ancestor = ancestor_on_target.is_some();

    // On the no-checkout path the operation's shape is an in-place rewrite of the
    // current branch, so all the bookkeeping below (sub-stack, parent maps, the
    // rebase loop that restacks what sits above HEAD) is that of the current
    // branch. The requested target only re-enters when the rebase todo claims it.
    let target_branch = if moving_onto_ancestor {
        current_branch_name.clone()
    } else {
        requested_target.clone()
    };
    let target_old_head_id = if moving_onto_ancestor {
        head_id
    } else {
        requested_target_old_head_id
    };
    let target_in_current_context = target_branch == upstream_name
        || current_stack
            .stack_branches
            .iter()
            .any(|b| b.name == target_branch);

    // An inline fold rewrites ancestors of HEAD too. Discover from the folded
    // commit so side branches of those ancestors participate in the restack.
    let discovery_id = if inline_fixup && target_branch != upstream_name {
        Oid::from_str(&fixup_commit_id)?
    } else {
        target_old_head_id
    };
    let target_stack = build_stack_context(repo, discovery_id, upstream_id, &upstream_name)?;
    let target_sub_stack = if inline_fixup {
        collect_sub_stack_from_id(repo, discovery_id, &target_stack.stack_branches)?
    } else {
        collect_target_sub_stack(
            repo,
            &target_branch,
            target_old_head_id,
            &upstream_name,
            &target_stack.stack_branches,
        )?
    };
    let target_has_dependents =
        has_dependents_to_rebase(&target_branch, &upstream_name, &target_sub_stack);

    let should_rebase = if !target_in_current_context && on_flag && target_has_dependents {
        crate::commands::prompt_confirm(
            &format!(
                "Branch '{}' has dependent branches in another stack. Rebase that stack as well?",
                target_branch
            ),
            crate::commands::Fallback::Default(false),
        )?
    } else {
        true
    };

    let switching_branches = target_branch != current_branch_name;
    let mut sub_stack = target_sub_stack;
    crate::stack::sort_branches_topologically(repo, &mut sub_stack)?;

    let mut remaining_branches = Vec::new();
    for branch in &sub_stack {
        // Ancestor refs move during autosquash; only off-path branches need replay.
        if branch.name == target_branch
            || (inline_fixup && is_on_history_of(repo, branch.id, head_id)?)
        {
            continue;
        }
        remaining_branches.push(branch.name.clone());
    }

    let will_rebase = should_rebase && target_has_dependents && !remaining_branches.is_empty();
    let needs_autosquash = is_fixup;
    let autosquash_state_required = needs_autosquash && !switching_branches && !will_rebase;
    // The move rebase always needs saved state (it stashes and rewrites branch
    // tips); this is the flag for the case where no dependent restack follows, so
    // the tail work the rebase loop would otherwise do falls to us.
    let move_state_required = moving_onto_ancestor && !will_rebase;

    // The check_worktrees call must run before the code path that performs the commit and
    // mutates target_branch so failures don't leave state unpersisted.
    if will_rebase || needs_autosquash {
        let mut guarded_branches = remaining_branches.clone();
        // An inline fold rewrites the fixup target's parent..HEAD with
        // `--update-refs`, moving every branch tip inside that range — not just
        // the off-path dependents replayed afterwards. Those tips must pass the
        // worktree check too, or one checked out elsewhere would be skipped by
        // `--update-refs` and silently left on pre-fold history.
        if inline_fixup {
            let fixup_commit = repo.find_commit(Oid::from_str(&fixup_commit_id)?)?;
            for (name, _) in
                local_branch_tips_in_range(repo, autosquash_base(&fixup_commit)?, head_id)?
            {
                if name != current_branch_name && !guarded_branches.contains(&name) {
                    guarded_branches.push(name);
                }
            }
        }
        check_worktrees(&guarded_branches, parsed.force)?;
    }
    // The move rebase rewrites the branches between the target and HEAD as well,
    // which the dependent list above does not cover.
    if let Some(ancestor_target) = &ancestor_on_target {
        let in_range: Vec<String> = crate::rebase_utils::local_branch_tips_in_range(
            repo,
            Some(requested_target_old_head_id),
            head_id,
        )?
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| name != &current_branch_name)
        .chain(std::iter::once(ancestor_target.clone()))
        .collect();
        check_worktrees(&in_range, parsed.force)?;
    }
    // Both in-place rebases replay HEAD's history from below the current
    // branch, where a merge of another branch would be flattened.
    let in_place_replay_base = if inline_fixup {
        Some(autosquash_base(
            &repo.find_commit(Oid::from_str(&fixup_commit_id)?)?,
        )?)
    } else if moving_onto_ancestor {
        Some(Some(requested_target_old_head_id))
    } else {
        None
    };
    if let Some(base) = in_place_replay_base {
        crate::rebase_utils::ensure_replay_keeps_merged_branches(
            repo,
            base,
            head_id,
            Some(upstream_id),
            false,
        )?;
    }

    // The autosquash and move rebases below rewrite branch tips with
    // `--update-refs` (git >= 2.38). Verify support up front, before `git commit`
    // creates a commit or any state is stashed/saved, so an unsupported git fails
    // cleanly with nothing left to undo.
    if needs_autosquash || moving_onto_ancestor {
        crate::rebase_utils::ensure_git_supports_update_refs()?;
    }

    let pre_commit_state_required = switching_branches || will_rebase;
    if pre_commit_state_required || needs_autosquash || moving_onto_ancestor {
        // Dependent restacks are declared by the rebase loop itself.
        let mut plan = crate::overrides::Plan::default();
        plan.checkout(target_old_head_id)
            .checkout(requested_target_old_head_id);
        if moving_onto_ancestor {
            plan.replay(Some(requested_target_old_head_id), head_id);
        }
        if needs_autosquash {
            let base = autosquash_base(&repo.find_commit(Oid::from_str(&fixup_commit_id)?)?)?;
            plan.replay(base, head_id).replay(base, target_old_head_id);
        }
        crate::overrides::prepare(repo, &plan)?;
        let (parent_id_map, parent_name_map) = if will_rebase {
            crate::stack::build_parent_maps(
                repo,
                &sub_stack,
                &target_stack.stack_branches,
                target_stack.merge_base,
                target_old_head_id,
                &target_branch,
            )?
        } else {
            (HashMap::new(), HashMap::new())
        };
        let mut original_tip_map = HashMap::new();
        original_tip_map.insert(target_branch.clone(), target_old_head_id.to_string());
        if will_rebase {
            original_tip_map.extend(
                sub_stack
                    .iter()
                    .map(|branch| (branch.name.clone(), branch.id.to_string())),
            );
        }
        // An inline fixup autosquashes with `--update-refs`, which rewrites every
        // branch tip between the fixup target's parent and HEAD — including
        // branches *below* HEAD that own the target commit. Those are not in
        // `sub_stack` (which is HEAD's branch plus its descendants), so record
        // their pre-fold tips here too; otherwise `kin abort` would restore
        // HEAD's branch and its descendants but leave a below-HEAD branch
        // stranded at the folded commit.
        if inline_fixup {
            record_below_head_rewritten_tips(
                repo,
                &fixup_commit_id,
                target_old_head_id,
                &mut original_tip_map,
            )?;
        }

        let mut state = RebaseState {
            operation: crate::rebase_utils::Operation::Commit,
            replay: Some(crate::rebase_utils::Replay::Branches),
            original_branch: target_branch.clone(),
            // The branch the commit lands on. Moving onto an ancestor rewrites
            // the current branch in place, but the commit goes to the ancestor.
            target_branch: ancestor_on_target
                .clone()
                .unwrap_or_else(|| target_branch.clone()),
            caller_branch: if switching_branches {
                Some(current_branch_name.clone())
            } else {
                None
            },
            remaining_branches: if will_rebase {
                remaining_branches
            } else {
                Vec::new()
            },
            in_progress_branch: None,
            parent_id_map,
            parent_name_map,
            new_base_map: HashMap::new(),
            original_commit_count_map: HashMap::new(),
            original_tip_map,
            owned_tip_map: HashMap::new(),
            set_asides: Default::default(),
            // The move path commits before it rewrites anything, so an abort at
            // any point after that must hand the content back (as staged changes)
            // rather than drop it with the commit.
            preserve_content_on_abort: moving_onto_ancestor,
            suppress_editor: false,
            abort_only: false,
            unstage_on_restore: switching_branches,
            legacy_autostash: false,
            cleanup_merged_branches: Vec::new(),
            cleanup_checkout_fallback: None,
        };

        // `kin commit` always sets the unstaged changes aside when it rewrites
        // or switches branches; the autostash flags are no-ops for it. When
        // committing onto another branch (`--on`), keep the *staged* changes
        // (what we're committing there) while setting the *unstaged* ones aside
        // via `git stash --keep-index --include-untracked`. Take it only now —
        // after the fallible planning above — so a failure there can't strand
        // the user's changes, and record it in the saved state right away. The
        // in-place paths take theirs once the commit exists (below).
        if switching_branches {
            state.set_asides.extend(stash_non_staged_changes(repo)?);
        }

        // The move path has nothing to recover until its commit exists, so it
        // saves its state after committing (below) rather than here — a `git
        // commit` that fails must not leave a state file behind.
        if pre_commit_state_required && let Err(err) = save_state(repo, &state) {
            // Persisting failed, so no later `kin continue`/`abort` knows about
            // the stash; pop it back rather than stranding the user's changes.
            set_aside::unwind(repo, &mut state.set_asides);
            return Err(err);
        }

        if switching_branches {
            // The index still holds what we are about to commit, and `git
            // checkout` refuses to carry it whenever one of those paths merely
            // differs on the target branch. Set the staged content aside too, so
            // the switch happens on a clean tree, and merge it back on the other
            // side — a three-way apply, which only fails on a real conflict.
            carry_staged_changes_onto(repo, &mut state, &target_branch)?;
        }

        // Run the actual git commit
        let status = Command::new("git")
            .arg("commit")
            .args(&parsed.git_commit_args)
            .status()?;
        if !status.success() {
            if switching_branches {
                // The changes were set aside and the target checked out, so
                // the journal is what `kin abort` needs to bring them back.
                return Err(anyhow!(
                    "git commit failed. Resolve and run 'kin continue', or run 'kin abort'."
                ));
            }
            // In place, nothing has changed yet: nothing was set aside (that
            // happens once the commit exists) and no branch moved. Like a
            // rebase that never started, leave no journal to continue.
            if pre_commit_state_required {
                clear_state(repo)?;
            }
            return Err(anyhow!("git commit failed; nothing was changed."));
        }

        if !switching_branches && !needs_autosquash && !moving_onto_ancestor {
            // The commit (including an amend) has succeeded. Abort should undo
            // only the dependent restack, keeping the user's committed work.
            // Parent maps still refer to the old tip so descendants replay the
            // correct range; only the rollback checkpoint advances here.
            let committed_tip = head_commit_id()?;
            state
                .original_tip_map
                .insert(target_branch.clone(), committed_tip.to_string());
            if let Err(err) = save_state(repo, &state) {
                // Move only the ref, retaining the committed index and unstaged
                // work. Unlike HEAD^, the captured tip also restores an amend.
                // Compare the current tip before replacing it, so unexpected
                // concurrent ref changes are not overwritten.
                return match repo.reference_matching(
                    &format!("refs/heads/{target_branch}"),
                    target_old_head_id,
                    true,
                    committed_tip,
                    "kin commit: restore tip after checkpoint failure",
                ) {
                    Ok(_) => Err(err.context(format!(
                        "Failed to save the post-commit checkpoint. Restored the pre-commit tip; any committed file changes remain staged. Inspect 'git show {committed_tip}' to recover the commit message or metadata, including a metadata-only amend. Fix the state persistence problem, then run 'kin abort --clear-state' before retrying."
                    ))),
                    Err(rollback_err) => Err(err.context(format!(
                        "Failed to save the post-commit checkpoint and restore '{target_branch}': {rollback_err}. Your commit is {committed_tip}; dependent branches have not been restacked."
                    ))),
                };
            }
            // The dependents are restacked in place, on a clean tree: set the
            // unstaged changes aside now that the staged ones are committed.
            if will_rebase {
                set_aside_after_commit(repo, &mut state, target_old_head_id)?;
            }
        }

        if let Some(ancestor_target) = &ancestor_on_target {
            let moved_commit_id = head_commit_id()?;

            // Take the stash only *after* the commit, for the same reason the
            // autosquash below does: the staged content is now committed, so a
            // `--keep-index` stash captures genuinely unstaged leftovers instead
            // of re-capturing what we just committed. If stashing fails, undo the
            // commit so the failure can't strand it outside a recoverable state.
            match stash_non_staged_changes(repo) {
                Ok(changes) => state.set_asides.extend(changes),
                Err(err) => {
                    match Command::new("git")
                        .args(["reset", "--soft", "HEAD^"])
                        .status()
                    {
                        Ok(status) if status.success() => return Err(err),
                        _ => {
                            return Err(err.context(
                                "Additionally, failed to roll back the commit; it remains at HEAD. Remove it with 'git reset --soft HEAD^'.",
                            ));
                        }
                    }
                }
            };

            // The replay rewrites `<target>..HEAD` with `--update-refs`, which
            // moves the target's own ref and every branch tip in between. None of
            // those are in `sub_stack` (the current branch and its descendants),
            // so record their pre-move tips here or `kin abort` would leave them
            // on rewritten history.
            state
                .original_tip_map
                .entry(ancestor_target.clone())
                .or_insert_with(|| requested_target_old_head_id.to_string());
            record_branch_tips_in_range(
                repo,
                Some(requested_target_old_head_id),
                moved_commit_id,
                &mut state.original_tip_map,
            )?;
            if let Err(err) = save_state(repo, &state) {
                set_aside::unwind(repo, &mut state.set_asides);
                return Err(err.context(
                    "The commit was created on this branch but the move could not be started; it is still at HEAD.",
                ));
            }

            println!("Moving the new commit onto '{}'...", ancestor_target);
            let mut cmd = Command::new("git");
            cmd.env(
                "GIT_SEQUENCE_EDITOR",
                crate::rebase_todo::sequence_editor_command(
                    &moved_commit_id.to_string(),
                    &format!("refs/heads/{}", ancestor_target),
                )?,
            )
            .arg("rebase")
            .arg("-i")
            // The replay must move exactly the one commit and change nothing
            // else, so neutralize the ambient config that would otherwise edit
            // the todo underneath us: `rebase.autosquash` would fold any
            // `fixup!`/`squash!` commit already in the range, and
            // `rebase.rebaseMerges` would rewrite the todo into labels and
            // merges.
            .arg("--no-autosquash")
            .arg("--no-rebase-merges")
            .arg("--update-refs")
            // The unstaged changes are set aside above; `rebase.autostash`
            // must not have Git set anything aside itself.
            .arg("--no-autostash");
            cmd.arg(ancestor_target);

            let failure = match cmd.status() {
                Ok(status) if status.success() => None,
                Ok(_) => Some(anyhow!(
                    "Moving the commit onto '{}' failed before the rebase started.",
                    ancestor_target
                )),
                Err(err) => Some(anyhow::Error::from(err).context(format!(
                    "failed to run the rebase moving the commit onto '{ancestor_target}'."
                ))),
            };
            if let Some(err) = failure {
                if git_rebase_in_progress(repo) {
                    // Record which branch is mid-rebase so `kin continue` matches
                    // the saved state, exactly as the autosquash path does.
                    state.in_progress_branch = Some(current_branch_name.clone());
                    save_state(repo, &state)?;
                    return Err(anyhow!(
                        "Moving the commit onto '{}' hit conflicts. Resolve them and run 'kin continue', or run 'kin abort' to undo the commit and get the changes back staged.",
                        ancestor_target
                    ));
                }
                // Nothing moved, so `kin continue` must not find state to
                // finish: it would restack the dependents onto the commit
                // left on this branch and report the move as completed.
                return Err(unwind_unstarted_rebase(
                    repo,
                    &mut state,
                    target_old_head_id,
                    err,
                ));
            }

            if move_state_required {
                set_aside::restore_all(repo, &mut state, Phase::Completion)?;
                clear_state(repo)?;
                return Ok(());
            }
        }

        if needs_autosquash {
            // An in-place fold (with or without dependents to restack after it)
            // sets the unstaged changes aside only *after* the fixup commit: the
            // staged content we're folding in is now committed, so a
            // `--keep-index` stash captures genuinely unstaged leftovers rather
            // than re-capturing (and later re-applying) the fixup content. The
            // checkout path set them aside before switching branches.
            if !switching_branches {
                set_aside_after_commit(repo, &mut state, target_old_head_id)?;
            }

            let fixup_commit = repo.find_commit(Oid::from_str(&fixup_commit_id)?)?;
            let autosquash_base_arg = match autosquash_base(&fixup_commit)? {
                Some(base) => base.to_string(),
                None => "--root".to_string(),
            };

            let mut cmd = Command::new("git");
            cmd.env("GIT_SEQUENCE_EDITOR", "true")
                .arg("rebase")
                .arg("-i")
                .arg("--autosquash")
                // Everything is set aside by Kindra; `rebase.autostash` must not
                // have Git set anything aside itself.
                .arg("--no-autostash");
            // Always move the branch tips inside the rewritten range with the fold
            // rather than relying on the ambient `rebase.updateRefs` git config
            // (off by default): this moves an inline fixup's below-HEAD ancestor
            // branches and any sibling branch sharing the folded commit (e.g. a
            // shared-head interactive pick). Branches stacked *above* the range are
            // restacked afterwards by the rebase loop, which skips any this moved.
            // (Support for `--update-refs` is verified up front in the validation
            // path above, before any state is mutated.)
            cmd.arg("--update-refs");
            cmd.arg(&autosquash_base_arg);

            // A spawn error means no rebase started at all, so it takes the same
            // unwind as a pre-start rejection rather than `?`-returning past it
            // with the fixup commit and saved state left behind.
            let failure = match cmd.status() {
                Ok(status) if status.success() => None,
                Ok(_) => Some(anyhow!("git rebase --autosquash failed before starting.")),
                Err(err) => {
                    Some(anyhow::Error::from(err).context("failed to run git rebase --autosquash."))
                }
            };

            if let Some(err) = failure {
                if git_rebase_in_progress(repo) {
                    // The autosquash rebase paused on a conflict. Record which
                    // branch is mid-rebase so `kin continue` matches the saved
                    // state — required whether or not the target has dependents
                    // (a missing in_progress_branch makes `kin continue` refuse).
                    state.in_progress_branch = Some(target_branch.clone());
                    save_state(repo, &state)?;
                    return Err(anyhow!(
                        "git rebase --autosquash failed. Resolve conflicts and run 'kin continue', or run 'kin abort'."
                    ));
                }
                return Err(unwind_unstarted_rebase(
                    repo,
                    &mut state,
                    target_old_head_id,
                    err,
                ));
            }

            if autosquash_state_required {
                set_aside::restore_all(repo, &mut state, Phase::Completion)?;
                clear_state(repo)?;
            }
        }

        if !pre_commit_state_required {
            return Ok(());
        }

        // Refresh repo state after commit
        let repo = crate::open_repo()?;
        let _new_target_head_id = repo.revparse_single(&target_branch)?.id();

        run_rebase_loop(&repo, state)
    } else {
        // Run the actual git commit
        let status = Command::new("git")
            .arg("commit")
            .args(&parsed.git_commit_args)
            .status()?;
        if !status.success() {
            return Err(anyhow!("git commit failed"));
        }
        Ok(())
    }
}

/// Commit staged changes onto a newly created branch forked from HEAD.
///
/// Without `--insert` this is a fork: the new branch is a sibling created off the
/// current commit and the current branch's existing children stay put — the
/// equivalent of `git checkout -b <name> && git commit`, plus name slugging. With
/// `--insert` the new branch is spliced into the stack: after the commit, every
/// branch that descended from HEAD is restacked onto the new branch, so the chain
/// becomes `<current> -> <new> -> <children>`.
fn commit_on_new_branch(
    repo: &Repository,
    current_branch_name: &str,
    upstream_name: &str,
    upstream_id: Oid,
    head_id: Oid,
    parsed: &ParsedCommitArgs,
) -> Result<()> {
    // Creating a branch for an empty commit is never the intent, so require
    // content up front unless `-a`/`-p`/a pathspec will supply it — mirroring the
    // interactive path's guard.
    if requires_staged_changes(&parsed.git_commit_args) && !has_staged_changes(repo)? {
        return Err(anyhow!("nothing to commit, working tree clean"));
    }

    let branch_name = resolve_new_branch_name(repo, parsed)?;

    // For `--insert`, snapshot the children (branches descending from HEAD) and
    // preflight the restack *before* creating the branch or committing, so a
    // failure leaves nothing half-done.
    let insert_plan = if parsed.insert {
        let merge_base = repo.merge_base(upstream_id, head_id)?;
        let all_branches = get_stack_branches_from_merge_base(
            repo,
            merge_base,
            head_id,
            upstream_id,
            upstream_name,
        )?;
        let mut children = Vec::new();
        collect_descendants_of_id(repo, head_id, &all_branches, &mut children)?;
        sort_branches_topologically(repo, &mut children)?;

        if !children.is_empty() {
            crate::rebase_utils::ensure_git_supports_update_refs()?;
            let names: Vec<String> = children.iter().map(|c| c.name.clone()).collect();
            check_worktrees(&names, parsed.force)?;
        }
        Some((merge_base, all_branches, children))
    } else {
        None
    };

    // Create the branch at HEAD and switch to it, then commit. This is the only
    // failure-prone mutation, and it rolls back cleanly (return to the original
    // branch, delete the new one) because no stash or state file exists yet.
    let head_commit = repo.find_commit(head_id)?;
    repo.branch(&branch_name, &head_commit, false)
        .with_context(|| format!("Failed to create branch '{branch_name}'."))?;

    if let Err(err) = checkout_branch(&branch_name) {
        // Checkout failed, so we are still on the original branch; drop the
        // branch we just created. Surface a rollback failure rather than hiding it.
        let err = err.context(format!("Failed to check out new branch '{branch_name}'."));
        if let Err(delete_err) = delete_local_branch(repo, &branch_name) {
            return Err(err.context(format!(
                "Additionally, failed to delete '{branch_name}' during rollback ({delete_err}); remove it manually with 'git branch -D {branch_name}'."
            )));
        }
        return Err(err);
    }

    let status = Command::new("git")
        .arg("commit")
        .args(&parsed.git_commit_args)
        .status()?;
    if !status.success() {
        // Nothing else has mutated yet, so undo the branch creation entirely.
        // Return to the original branch *first* — only then is the new branch
        // safe to delete. If a rollback step fails, surface it (with the commit
        // failure as context) instead of swallowing it and reporting a clean
        // failure while stranded on the new branch.
        if let Err(checkout_err) = checkout_branch(current_branch_name) {
            return Err(checkout_err.context(format!(
                "git commit failed, and returning to '{current_branch_name}' also failed; you are left on '{branch_name}'. Switch back and delete it manually."
            )));
        }
        if let Err(delete_err) = delete_local_branch(repo, &branch_name) {
            return Err(delete_err.context(format!(
                "git commit failed; additionally, the created branch '{branch_name}' could not be deleted. Remove it manually with 'git branch -D {branch_name}'."
            )));
        }
        return Err(anyhow!(
            "git commit failed; branch '{branch_name}' was not created."
        ));
    }

    // Fork, or `--insert` with no children to move: the commit landed on the new
    // branch and any existing children stay on the original branch. Done.
    let Some((merge_base, all_branches, children)) = insert_plan else {
        println!("Created branch '{branch_name}' with your commit.");
        return Ok(());
    };
    if children.is_empty() {
        println!("Created branch '{branch_name}' with your commit.");
        return Ok(());
    }

    // `--insert`: restack the captured children onto the new branch. Build the
    // rebase state the way `--on` does — parents resolved against the pre-insert
    // HEAD, with the new branch standing in as the parent of anything that hung
    // directly off it — then drive it through the shared rebase loop.
    //
    // The branch and its commit already exist at this point, so if preparing the
    // restack fails, say so: nothing is lost — the user is on the new branch with
    // the commit applied and can reattach the dependents with `kin restack`.
    let inserted_note = format!(
        "Created branch '{branch_name}' with your commit and switched to it, but starting the restack of its dependents failed. The commit is applied; reattach the dependent branches with 'kin restack'."
    );

    let (parent_id_map, parent_name_map) = build_parent_maps(
        repo,
        &children,
        &all_branches,
        merge_base,
        head_id,
        &branch_name,
    )
    .with_context(|| inserted_note.clone())?;

    let new_branch_tip = repo
        .revparse_single(&branch_name)
        .with_context(|| inserted_note.clone())?
        .id();
    let mut original_tip_map = HashMap::new();
    original_tip_map.insert(branch_name.clone(), new_branch_tip.to_string());
    original_tip_map.extend(children.iter().map(|c| (c.name.clone(), c.id.to_string())));

    let mut state = RebaseState {
        operation: crate::rebase_utils::Operation::Commit,
        replay: Some(crate::rebase_utils::Replay::Branches),
        original_branch: branch_name.clone(),
        target_branch: branch_name.clone(),
        // End on the new branch, not the branch we started on.
        caller_branch: None,
        remaining_branches: children.iter().map(|c| c.name.clone()).collect(),
        in_progress_branch: None,
        parent_id_map,
        parent_name_map,
        new_base_map: HashMap::new(),
        original_commit_count_map: HashMap::new(),
        original_tip_map,
        owned_tip_map: HashMap::new(),
        set_asides: Default::default(),
        preserve_content_on_abort: false,
        suppress_editor: false,
        abort_only: false,
        unstage_on_restore: false,
        legacy_autostash: false,
        cleanup_merged_branches: Vec::new(),
        cleanup_checkout_fallback: None,
    };

    // Set aside unstaged changes so the child rebases run on a clean tree; the
    // rebase loop restores them and returns us to the new branch when it finishes.
    state
        .set_asides
        .extend(stash_non_staged_changes(repo).with_context(|| inserted_note.clone())?);
    if let Err(err) = save_state(repo, &state) {
        set_aside::unwind(repo, &mut state.set_asides);
        return Err(err.context(inserted_note));
    }

    println!(
        "Created branch '{branch_name}' and inserting it into the stack; restacking dependents..."
    );
    run_rebase_loop(repo, state)
}

/// Resolve the branch name for `-b`/`--new-branch`: an explicit name is validated
/// and required to be free; an omitted name is slugified from the commit message
/// subject (which must therefore be supplied via `-m`).
fn resolve_new_branch_name(repo: &Repository, parsed: &ParsedCommitArgs) -> Result<String> {
    match parsed.new_branch.clone().flatten() {
        Some(name) => {
            if !git2::Branch::name_is_valid(&name)? {
                return Err(anyhow!("'{name}' is not a valid git branch name."));
            }
            if repo.find_branch(&name, BranchType::Local).is_ok() {
                return Err(anyhow!("Branch '{name}' already exists."));
            }
            Ok(name)
        }
        None => {
            let subject = message_subject_from_args(&parsed.git_commit_args).ok_or_else(|| {
                anyhow!(
                    "kin commit -b needs a branch name, or a commit message (via -m) to derive one from."
                )
            })?;
            let base = crate::commands::slugify_subject(&subject).ok_or_else(|| {
                anyhow!(
                    "Could not derive a branch name from the commit message '{subject}'; pass a name explicitly."
                )
            })?;
            crate::commands::disambiguate_branch_name(repo, &base, &HashSet::new())
        }
    }
}

/// Extract the subject (first line) of the commit message from forwarded git
/// args, reading the first `-m`/`--message` value. Returns `None` when no inline
/// message is present (e.g. the message would come from an editor or `-F` file),
/// in which case the caller requires an explicit branch name.
fn message_subject_from_args(args: &[String]) -> Option<String> {
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--" {
            break;
        }
        let message = if arg == "-m" || arg == "--message" {
            idx += 1;
            args.get(idx).cloned()
        } else if let Some(value) = arg.strip_prefix("--message=") {
            Some(value.to_string())
        } else if let Some((value, consumed_next)) = short_cluster_message(arg, args.get(idx + 1)) {
            // A short-option cluster containing `-m` (e.g. `-m`, `-mFix`, `-am`,
            // `-amFix`), where the value is glued after `m` or is the next arg.
            if consumed_next {
                idx += 1;
            }
            Some(value)
        } else {
            None
        };
        if let Some(message) = message {
            return message.lines().next().map(|line| line.to_string());
        }
        idx += 1;
    }
    None
}

/// Extract the `-m` value from a short-option cluster like `-m`, `-mFix`, `-am`,
/// or `-amFix`. Short flags parse left to right; `-m` takes the rest of the token
/// as its value, or the following arg when it is the last character. A
/// value-taking flag before `m` (`-C`/`-c`/`-F`) would consume the remainder, so
/// there is no message in that case. Returns `(value, consumed_next_arg)`.
fn short_cluster_message(arg: &str, next: Option<&String>) -> Option<(String, bool)> {
    if arg.starts_with("--") {
        return None;
    }
    let body = arg.strip_prefix('-').filter(|body| !body.is_empty())?;
    for (offset, ch) in body.char_indices() {
        match ch {
            'm' => {
                let rest = &body[offset + ch.len_utf8()..];
                return if rest.is_empty() {
                    next.map(|value| (value.clone(), true))
                } else {
                    Some((rest.to_string(), false))
                };
            }
            // These short options take a value, which swallows the rest of the
            // token (or the next arg); an `m` after them is part of that value.
            'C' | 'c' | 'F' => return None,
            // Any other char is a value-less boolean flag; keep scanning.
            _ => {}
        }
    }
    None
}

fn delete_local_branch(repo: &Repository, name: &str) -> Result<()> {
    if let Ok(mut branch) = repo.find_branch(name, BranchType::Local) {
        branch.delete()?;
    }
    Ok(())
}

struct StackContext {
    merge_base: Oid,
    stack_branches: Vec<StackBranch>,
}

#[derive(Default, Debug)]
struct ParsedCommitArgs {
    on_target: Option<Option<String>>,
    interactive: bool,
    fixup_target: Option<String>,
    force: bool,
    // `-b`/`--new-branch`: `Some(Some(name))` is an explicit name, `Some(None)`
    // means derive one by slugifying the commit message.
    new_branch: Option<Option<String>>,
    // `--insert`: restack the current branch's children onto the new branch
    // instead of forking a sibling. Only meaningful with `new_branch`.
    insert: bool,
    git_commit_args: Vec<String>,
}

/// Global Kindra flags (`--yes` / `--no-interactive`) that clap's
/// `trailing_var_arg` on the `commit` subcommand swallows into the pass-through
/// args when they appear after `commit`. [`recover_interaction_flags`] folds
/// them back into the interaction mode and [`parse_commit_args`] strips them
/// from what is forwarded to `git commit`; keeping the list here keeps those two
/// in sync.
fn is_global_interaction_flag(arg: &str) -> bool {
    matches!(arg, "--yes" | "--no-interactive")
}

/// The args a commit invocation forwards to `git`, i.e. those before the first
/// `--` separator (everything after `--` is a literal git pathspec). Shared so
/// [`recover_interaction_flags`] and [`parse_commit_args`] agree on where the
/// global flags stop being meaningful (`kin commit -- --yes` leaves `--yes` as a
/// pathspec).
fn args_before_separator(args: &[String]) -> impl Iterator<Item = &String> {
    args.iter().take_while(|a| a.as_str() != "--")
}

/// Recover the global `--yes` / `--no-interactive` flags that clap's
/// `trailing_var_arg` on the `commit` subcommand captures into the pass-through
/// args when they appear after `commit`. Returns `(no_interactive, yes)` OR'd
/// with the values clap already bound to `Cli`. Only args before a `--`
/// separator are considered, so `kin commit -- --yes` leaves `--yes` as a
/// literal pathspec for `git`. `parse_commit_args` strips these same flags from
/// what is forwarded to `git commit`.
pub fn recover_interaction_flags(args: &[String], no_interactive: bool, yes: bool) -> (bool, bool) {
    let mut no_interactive = no_interactive;
    let mut yes = yes;
    for arg in args_before_separator(args) {
        match arg.as_str() {
            "--no-interactive" => no_interactive = true,
            "--yes" => yes = true,
            _ => {}
        }
    }
    (no_interactive, yes)
}

fn parse_commit_args(args: &[String]) -> Result<ParsedCommitArgs> {
    let mut parsed = ParsedCommitArgs::default();
    let mut idx = 0;

    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--" {
            parsed.git_commit_args.extend(args[idx..].iter().cloned());
            break;
        }

        // Global Kindra flags swallowed here by clap's `trailing_var_arg`. Drop
        // them so they are not forwarded to `git commit` (which rejects them);
        // `main` folds them back into the resolved interaction mode via
        // `recover_interaction_flags`.
        if is_global_interaction_flag(arg) {
            idx += 1;
            continue;
        }

        if arg == "--interactive" {
            parsed.interactive = true;
            idx += 1;
            continue;
        }

        if arg == "-b" || arg == "--new-branch" {
            if parsed.new_branch.is_some() {
                return Err(anyhow!("--new-branch can only be specified once."));
            }
            // The name is optional: consume the next token only when it is a
            // plain value (not another flag or the `--` pathspec separator);
            // otherwise the name is derived from the commit message.
            if idx + 1 < args.len() && args[idx + 1] != "--" && !args[idx + 1].starts_with('-') {
                parsed.new_branch = Some(Some(args[idx + 1].clone()));
                idx += 2;
            } else {
                parsed.new_branch = Some(None);
                idx += 1;
            }
            continue;
        }

        if let Some(value) = arg.strip_prefix("--new-branch=") {
            if parsed.new_branch.is_some() {
                return Err(anyhow!("--new-branch can only be specified once."));
            }
            parsed.new_branch = Some(if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            });
            idx += 1;
            continue;
        }

        if arg == "--insert" {
            parsed.insert = true;
            idx += 1;
            continue;
        }

        if arg == "--force" {
            parsed.force = true;
            idx += 1;
            continue;
        }

        // Accepted for symmetry with the other commands, but no-ops: kin commit
        // always sets aside the unstaged changes it has to move out of the way.
        if arg == "--autostash" || arg == "--no-autostash" {
            idx += 1;
            continue;
        }

        if arg == "--fixup" {
            if parsed.fixup_target.is_some() {
                return Err(anyhow!("--fixup can only be specified once."));
            }
            if idx + 1 == args.len() || args[idx + 1].is_empty() || args[idx + 1].starts_with('-') {
                return Err(anyhow!(
                    "--fixup requires a commit to fix up (e.g. 'kin commit --fixup <sha>')."
                ));
            }
            parsed.fixup_target = Some(args[idx + 1].clone());
            idx += 2;
            continue;
        }

        if let Some(value) = arg.strip_prefix("--fixup=") {
            if parsed.fixup_target.is_some() {
                return Err(anyhow!("--fixup can only be specified once."));
            }
            if value.is_empty() {
                return Err(anyhow!(
                    "--fixup requires a commit to fix up (e.g. 'kin commit --fixup=<sha>')."
                ));
            }
            parsed.fixup_target = Some(value.to_string());
            idx += 1;
            continue;
        }

        if arg == "--on" {
            if parsed.on_target.is_some() {
                return Err(anyhow!("--on can only be specified once."));
            }
            if idx + 1 == args.len() {
                parsed.on_target = Some(None);
                idx += 1;
                continue;
            }
            if args[idx + 1].starts_with('-') {
                return Err(anyhow!(
                    "When using '--on', provide a branch name or use '--on=' for interactive selection."
                ));
            }
            parsed.on_target = Some(Some(args[idx + 1].clone()));
            idx += 2;
            continue;
        }

        if let Some(value) = arg.strip_prefix("--on=") {
            if parsed.on_target.is_some() {
                return Err(anyhow!("--on can only be specified once."));
            }
            if value.is_empty() {
                parsed.on_target = Some(None);
            } else {
                parsed.on_target = Some(Some(value.to_string()));
            }
            idx += 1;
            continue;
        }

        parsed.git_commit_args.push(arg.clone());
        idx += 1;
    }

    if parsed.interactive && parsed.on_target.is_some() {
        return Err(anyhow!(
            "--interactive and --on are mutually exclusive. Use one or the other."
        ));
    }

    if parsed.fixup_target.is_some() && parsed.interactive {
        return Err(anyhow!(
            "--fixup and --interactive are mutually exclusive. Use one or the other."
        ));
    }

    if parsed.fixup_target.is_some() && parsed.on_target.is_some() {
        return Err(anyhow!(
            "--fixup and --on are mutually exclusive. --fixup determines the target branch from the commit."
        ));
    }

    if parsed.new_branch.is_some() {
        if parsed.on_target.is_some() {
            return Err(anyhow!(
                "--new-branch and --on are mutually exclusive: one commits onto a new branch, the other onto an existing one."
            ));
        }
        if parsed.interactive {
            return Err(anyhow!(
                "--new-branch and --interactive are mutually exclusive."
            ));
        }
        if parsed.fixup_target.is_some() {
            return Err(anyhow!("--new-branch and --fixup are mutually exclusive."));
        }
    } else if parsed.insert {
        return Err(anyhow!(
            "--insert requires --new-branch: it inserts a newly created branch into the stack."
        ));
    }

    Ok(parsed)
}

fn build_stack_context(
    repo: &Repository,
    head_id: Oid,
    upstream_id: Oid,
    upstream_name: &str,
) -> Result<StackContext> {
    let merge_base = repo.merge_base(upstream_id, head_id)?;
    let stack_branches =
        get_stack_branches_from_merge_base(repo, merge_base, head_id, upstream_id, upstream_name)?;
    Ok(StackContext {
        merge_base,
        stack_branches,
    })
}

fn select_target_branch(
    repo: &Repository,
    current_branch_name: &str,
    current_head_id: Oid,
    stack_branches: &[StackBranch],
) -> Result<String> {
    let mut options = stack_branches.to_vec();
    if !options.iter().any(|b| b.name == current_branch_name) {
        options.push(StackBranch {
            name: current_branch_name.to_string(),
            id: current_head_id,
        });
    }

    if options.is_empty() {
        return Err(anyhow!(
            "No branches found in the current stack to commit onto."
        ));
    }

    crate::stack::sort_branches_topologically(repo, &mut options)?;
    let display: Vec<String> = options
        .iter()
        .map(|b| {
            if b.name == current_branch_name {
                format!("* {}", b.name)
            } else {
                format!("  {}", b.name)
            }
        })
        .collect();
    let selected_display = crate::commands::prompt_select(
        "Select branch to commit onto:",
        display,
        crate::commands::Fallback::Require("Pass the target with --on <branch>."),
    )?;
    options
        .iter()
        .find(|b| {
            let rendered = if b.name == current_branch_name {
                format!("* {}", b.name)
            } else {
                format!("  {}", b.name)
            };
            rendered == selected_display
        })
        .map(|b| b.name.clone())
        .ok_or_else(|| anyhow!("Failed to resolve selected branch '{}'.", selected_display))
}

/// Whether `kin commit --on <branch>` can land the commit on `requested_target`
/// without checking it out, and if so which branch to claim in the rebase todo.
///
/// `git checkout` refuses to carry staged changes onto a branch whenever a staged
/// path merely *differs* there — the common case when the target touched the same
/// file — so when the target is an ancestor of HEAD the commit is made here and
/// replayed down onto it instead. The rewritten range is `<target>..HEAD`, which
/// bounds when that is safe:
///
/// - The target must be a branch of this stack, strictly below HEAD, and never
///   the upstream: trunk is not ours to rewrite.
/// - The content must come from the index. With `-a`/`-p`/a pathspec, `git
///   commit` reads the working tree, which on this path has not been set aside
///   yet, so it would commit more than the checkout path does.
/// - `--amend` on another branch means "amend that branch's tip", which is a
///   fold into an existing commit, not a commit that can be moved.
/// - Every branch descending from the target must be either inside the rewritten
///   range (moved by `--update-refs`) or a descendant of HEAD (restacked by the
///   rebase loop afterwards). A branch that forks inside the range but tips
///   outside it is moved by neither, and a sibling parked on HEAD's commit would
///   be claimed by the replay it is not part of. The check runs against the
///   *target's* sub-stack, not the current branch's: a branch hanging off the
///   middle of the range is exactly the case the current branch's stack context
///   cannot see.
///
/// Anything else falls back to the checkout path, which carries the staged
/// content across the switch with a stash instead.
#[allow(clippy::too_many_arguments)]
fn ancestor_move_target(
    repo: &Repository,
    parsed: &ParsedCommitArgs,
    has_interactive_selection: bool,
    requested_target: &str,
    requested_target_old_head_id: Oid,
    current_branch_name: &str,
    head_id: Oid,
    upstream_name: &str,
    upstream_id: Oid,
    stack_branches: &[StackBranch],
) -> Result<Option<String>> {
    if has_interactive_selection
        || parsed.on_target.is_none()
        || requested_target == current_branch_name
        || requested_target == upstream_name
    {
        return Ok(None);
    }

    if !stack_branches.iter().any(|b| b.name == requested_target)
        || !repo.graph_descendant_of(head_id, requested_target_old_head_id)?
    {
        return Ok(None);
    }

    if !requires_staged_changes(&parsed.git_commit_args)
        || !has_staged_changes(repo)?
        || parsed.git_commit_args.iter().any(|arg| arg == "--amend")
    {
        return Ok(None);
    }

    let target_stack = build_stack_context(
        repo,
        requested_target_old_head_id,
        upstream_id,
        upstream_name,
    )?;
    let descendants = collect_target_sub_stack(
        repo,
        requested_target,
        requested_target_old_head_id,
        upstream_name,
        &target_stack.stack_branches,
    )?;
    for branch in &descendants {
        if branch.name == requested_target || branch.name == current_branch_name {
            continue;
        }
        let in_rewritten_range = branch.id != head_id
            && repo.graph_descendant_of(head_id, branch.id)?
            && repo.graph_descendant_of(branch.id, requested_target_old_head_id)?;
        let restacked_afterwards = repo.graph_descendant_of(branch.id, head_id)?;
        if !in_rewritten_range && !restacked_afterwards {
            return Ok(None);
        }
    }

    Ok(Some(requested_target.to_string()))
}

fn collect_target_sub_stack(
    repo: &Repository,
    target_branch: &str,
    target_head_id: Oid,
    upstream_name: &str,
    all_branches_in_stack: &[StackBranch],
) -> Result<Vec<StackBranch>> {
    let mut sub_stack = Vec::new();
    if target_branch == upstream_name {
        crate::stack::collect_descendants_of_id(
            repo,
            target_head_id,
            all_branches_in_stack,
            &mut sub_stack,
        )?;
    } else if all_branches_in_stack
        .iter()
        .any(|b| b.name == target_branch)
    {
        collect_descendants(repo, target_branch, all_branches_in_stack, &mut sub_stack)?;
    }
    Ok(sub_stack)
}

/// The base of the autosquash range for a fixup: the fixup target commit's first
/// parent, or `None` for a root commit (which rewrites the whole history). Both
/// the rebase invocation and the abort-tip bookkeeping derive the rewritten range
/// (`base..HEAD`) from this single place so they can't disagree on what
/// `--update-refs` will move.
fn autosquash_base(fixup_commit: &git2::Commit) -> Result<Option<Oid>> {
    if fixup_commit.parent_count() > 0 {
        Ok(Some(fixup_commit.parent_id(0)?))
    } else {
        Ok(None)
    }
}

/// Record, into `original_tip_map`, the pre-rewrite tip of every local branch
/// whose tip lies in the range an inline-fixup autosquash rewrites with
/// `--update-refs`: from the fixup target commit's parent up to (and including)
/// the current HEAD. Existing entries are preserved. This is what lets `kin
/// abort` roll a completed fold back off a below-HEAD ancestor branch.
fn record_below_head_rewritten_tips(
    repo: &Repository,
    fixup_commit_id: &str,
    head_id: Oid,
    original_tip_map: &mut HashMap<String, String>,
) -> Result<()> {
    let fixup_commit = repo.find_commit(Oid::from_str(fixup_commit_id)?)?;
    // The rewritten range is base..HEAD; a root fixup has no base, so the
    // whole history is in range.
    record_branch_tips_in_range(
        repo,
        autosquash_base(&fixup_commit)?,
        head_id,
        original_tip_map,
    )
}

fn has_dependents_to_rebase(
    target_branch: &str,
    upstream_name: &str,
    sub_stack: &[StackBranch],
) -> bool {
    if target_branch == upstream_name {
        !sub_stack.is_empty()
    } else {
        sub_stack.iter().any(|b| b.name != target_branch)
    }
}

fn insert_generated_commit_arg(args: &mut Vec<String>, value: String) {
    let insert_at = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    args.insert(insert_at, value);
}

fn has_staged_changes(_repo: &Repository) -> Result<bool> {
    crate::rebase_utils::has_staged_changes()
}

/// Whether a commit needs a non-empty index: true unless `-a`/`--all`/`-p`/
/// `--patch` or a forwarded pathspec will supply the content instead. Shared by
/// the interactive-fold guard and the new-branch guard so both stay in sync.
fn requires_staged_changes(git_commit_args: &[String]) -> bool {
    !git_commit_args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-a" | "--all" | "-p" | "--patch"))
        && !has_forwarded_pathspec(git_commit_args)
}

fn has_forwarded_pathspec(args: &[String]) -> bool {
    if let Some(separator_index) = args.iter().position(|arg| arg == "--") {
        return separator_index + 1 < args.len();
    }

    let mut expects_value_for_option = false;
    for arg in args {
        if expects_value_for_option {
            expects_value_for_option = false;
            continue;
        }

        if arg == "--" {
            return true;
        }

        if option_takes_value(arg) {
            expects_value_for_option = true;
            continue;
        }

        if !arg.starts_with('-') {
            return true;
        }
    }

    false
}

/// Whether the forwarded `git commit` arguments amend, commit anything other
/// than exactly what is staged, or commit nothing at all (`--dry-run`). Short flags may be bundled (`-am`); a bundle
/// ends at the first short option that takes a value.
fn forwards_non_resolution_commit_flag(args: &[String]) -> bool {
    let mut expects_value_for_option = false;
    for arg in args {
        if expects_value_for_option {
            expects_value_for_option = false;
            continue;
        }
        if arg == "--" {
            return false;
        }
        if matches!(
            arg.as_str(),
            "--amend" | "--all" | "--patch" | "--only" | "--include" | "--dry-run"
        ) {
            return true;
        }
        if let Some(bundle) = arg.strip_prefix('-').filter(|rest| !rest.starts_with('-')) {
            for (index, flag) in bundle.char_indices() {
                if matches!(flag, 'a' | 'p' | 'o' | 'i') {
                    return true;
                }
                if matches!(flag, 'm' | 'C' | 'c' | 'F' | 'S' | 't' | 'u') {
                    // The rest of the bundle is this option's value; a bare
                    // option takes the next argument instead.
                    expects_value_for_option =
                        index + flag.len_utf8() == bundle.len() && option_takes_value(arg);
                    break;
                }
            }
            continue;
        }
        expects_value_for_option = option_takes_value(arg);
    }
    false
}

fn option_takes_value(arg: &str) -> bool {
    if arg.starts_with("--message=")
        || arg.starts_with("--reuse-message=")
        || arg.starts_with("--reedit-message=")
        || arg.starts_with("--fixup=")
        || arg.starts_with("--reset-author=")
        || arg.starts_with("--cleanup=")
        || arg.starts_with("--gpg-sign=")
        || arg.starts_with("--trailer=")
        || arg.starts_with("--date=")
        || arg.starts_with("--author=")
        || arg.starts_with("--pathspec-from-file=")
        || arg.starts_with("--inter-hunk-context=")
        || arg.starts_with("--unified=")
    {
        return false;
    }

    matches!(
        arg,
        "-m" | "-C"
            | "-c"
            | "-F"
            | "--message"
            | "--reuse-message"
            | "--reedit-message"
            | "--cleanup"
            | "-S"
            | "--gpg-sign"
            | "--trailer"
            | "--date"
            | "--author"
            | "--pathspec-from-file"
            | "--inter-hunk-context"
            | "-U"
            | "--unified"
    )
}

/// Set the unstaged changes aside for a rebase in place, once the commit
/// exists so the set-aside holds only what is left unstaged, record it in the
/// journal and save the journal. If either fails, the commit is rolled back as
/// for a rebase that never started.
fn set_aside_after_commit(
    repo: &Repository,
    state: &mut RebaseState,
    target_old_head_id: Oid,
) -> Result<()> {
    let recorded = stash_non_staged_changes(repo).and_then(|changes| {
        state.set_asides.extend(changes);
        save_state(repo, state)
    });
    recorded.map_err(|err| unwind_unstarted_rebase(repo, state, target_old_head_id, err))
}

/// Undo the commit of a fixup or `--on` move whose rebase git refused before
/// starting (e.g. a rejecting `pre-rebase` hook).
///
/// Nothing was rewritten, so the saved state must not survive: `kin continue`
/// would restack the dependents onto the raw `fixup!` commit, or onto the
/// commit that never moved. As absorb does, take the new commit back off and
/// return the changes to where they were before the command, then clear the
/// state. If the unwind cannot complete, the state is kept so `kin abort` can
/// still recover.
fn unwind_unstarted_rebase(
    repo: &Repository,
    state: &mut RebaseState,
    target_old_head_id: Oid,
    err: anyhow::Error,
) -> anyhow::Error {
    let target = target_old_head_id.to_string();
    // The state keeps naming the stash until its changes are back, so a
    // rollback that stops partway still leaves `kin abort` able to find them.
    let unwound = match state.caller_branch.clone() {
        // In place: the set-aside stash (if any) is based on the new commit, so
        // it goes back first; the soft reset then returns the commit's content
        // to the index.
        None => {
            let set_aside_anything = !state.set_asides.is_empty();
            let outcome = set_aside::unwind(repo, &mut state.set_asides);
            match run_git(&["reset", "--soft", &target]) {
                Ok(()) => Ok(outcome),
                Err(reset_err) if set_aside_anything => {
                    let changes = match outcome {
                        Outcome::Restored => "your set-aside changes were restored",
                        Outcome::ConflictsLeft { .. } => {
                            "restoring your set-aside changes left conflicts in the working tree"
                        }
                        Outcome::NotRestored { .. } => {
                            "your set-aside changes could not be restored"
                        }
                    };
                    Err(anyhow!("{reset_err:#}; {changes}"))
                }
                Err(reset_err) => Err(reset_err),
            }
        }
        // Checkout path: the stash taken on the caller branch before the switch
        // holds the staged changes as well as the non-staged ones, so the fixup
        // commit can be dropped outright and the stash applied back on its base.
        Some(caller_branch) => run_git(&["reset", "--hard", &target])
            .and_then(|()| checkout_branch(&caller_branch))
            .map(|()| set_aside::unwind(repo, &mut state.set_asides)),
    };
    // Until the set-aside changes are back, keep the state so `kin abort` can
    // still restore them.
    // Anything saved from here on describes a rolled-back commit, so there is
    // nothing for `kin continue` to resume.
    state.abort_only = true;
    match unwound.and_then(|outcome| {
        if state.set_asides.is_empty() {
            clear_state(repo)
        } else {
            save_state(repo, state)
        }
        .map(|()| outcome)
    }) {
        Ok(Outcome::Restored) => anyhow!(
            "{err:#} The commit was rolled back and your changes were restored as they were."
        ),
        Ok(Outcome::ConflictsLeft { .. }) => anyhow!(
            "{err:#} The commit was rolled back, but restoring your set-aside changes left conflicts in the working tree (see the warning above)."
        ),
        Ok(Outcome::NotRestored { .. }) => anyhow!(
            "{err:#} The commit was rolled back, but your set-aside changes could not be restored (see the warning above). Clear the way, then run 'kin abort' to restore them."
        ),
        Err(unwind_err) => match save_state(repo, state) {
            Ok(()) => anyhow!(
                "{err:#} Rolling back the commit did not complete ({unwind_err:#}); run 'kin abort' to restore the original state."
            ),
            // Without a saved journal that lists what is still set aside,
            // `kin abort` cannot be relied on: say how to recover by hand.
            Err(save_err) => anyhow!(
                "{err:#} Rolling back the commit did not complete ({unwind_err:#}), and saving the recovery state also failed ({save_err:#}), so 'kin abort' may not be able to finish it. Recover by hand:{}",
                manual_recovery_steps(state, target_old_head_id)
            ),
        },
    }
}

/// How to finish rolling back a commit by hand when no journal records what
/// is left to undo, in the order [`unwind_unstarted_rebase`] undoes it: in
/// place, every set-aside not yet restored (newest first, based on the new
/// commit) and then the commit to take back off; after a branch switch, the
/// branch tip to put back and then the set-asides, on the caller branch.
fn manual_recovery_steps(state: &RebaseState, target_old_head_id: Oid) -> String {
    let mut steps = Vec::new();
    if let Some(caller) = &state.caller_branch {
        steps.push(format!(
            "'{}' should point at {target_old_head_id} again: find its tip from before the commit with 'git reflog show refs/heads/{}' and reset it there, then check out '{caller}'",
            state.original_branch, state.original_branch
        ));
    }
    for set_aside in state.set_asides.newest_first() {
        // The rollback restores as `Phase::Unwind` does, which always brings
        // the staged state back with `--index`; repeating it by hand must too.
        let apply = match &set_aside.oid {
            Some(oid) => format!("'git stash apply --index {oid}'"),
            None => "'git stash apply --index <entry>' (find it with 'git stash list')".to_string(),
        };
        steps.push(format!(
            "clean up any partial application, then restore the changes set aside in stash entry '{}' with {apply}; drop that entry only after the changes are restored",
            set_aside.stash
        ));
    }
    if state.caller_branch.is_none()
        && head_commit_id().is_ok_and(|head| head != target_old_head_id)
    {
        steps.push(format!(
            "take the new commit back off with 'git reset --soft {target_old_head_id}' (its changes return to the index)"
        ));
    }
    if steps.is_empty() {
        return " nothing is left to undo; remove the saved state with 'kin abort --clear-state' if it remains.".to_string();
    }
    steps
        .iter()
        .enumerate()
        .map(|(n, step)| format!(" {}. {step}.", n + 1))
        .collect()
}

fn run_git(args: &[&str]) -> Result<()> {
    let status = Command::new("git").args(args).status()?;
    if !status.success() {
        return Err(anyhow!("git {} failed", args.join(" ")));
    }
    Ok(())
}

/// The commit id at HEAD, read through git so it reflects a commit just made by
/// a subprocess rather than whatever the open repository handle has cached.
fn head_commit_id() -> Result<Oid> {
    let output = Command::new("git").args(["rev-parse", "HEAD"]).output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "Failed to resolve HEAD after committing: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(Oid::from_str(
        String::from_utf8_lossy(&output.stdout).trim(),
    )?)
}

/// Switch to `target_branch` with the staged content in hand.
///
/// The unstaged changes are already set aside at this point, so the tree holds
/// exactly what the commit will take. `git checkout` still refuses to carry that
/// across whenever one of those paths differs on the target branch — it declines
/// on any difference, not just a conflicting one — so the staged content rides
/// along in a stash of its own instead: switch on a clean tree, then bring it
/// back with `git stash apply --index`, a three-way merge that only fails when
/// the change genuinely conflicts with the target.
///
/// The carry stash is recorded in the saved state while it is live, and every
/// path out of here clears it again: on success by dropping it, on failure by
/// putting it back where it applies cleanly (the branch it was taken on). A
/// set-aside leaves the saved state only once its changes are back, so a
/// restore that fails leaves it recorded for `kin abort`.
fn carry_staged_changes_onto(
    repo: &Repository,
    state: &mut RebaseState,
    target_branch: &str,
) -> Result<()> {
    let caller_branch = state.caller_branch.clone().ok_or_else(|| {
        anyhow!("Internal error: no caller branch recorded for the branch switch.")
    })?;

    let carry_stash = set_aside::take(repo, set_aside::Kind::Carry, "kin-commit-on-index")
        .with_context(|| {
            "Failed to set the staged changes aside for the branch switch. Use 'kin abort' to restore original state."
        })?;
    let Some(carry_stash) = carry_stash else {
        // Nothing staged to carry (an empty or `-a`-style commit): the tree is
        // already clean, so the switch cannot be refused for our changes.
        return checkout_branch(target_branch).context(
            "Failed to checkout target branch. Use 'kin abort' to restore original state.",
        );
    };

    state.set_asides.push(carry_stash.clone());
    if let Err(err) = save_state(repo, state) {
        // The saved journal already records the unstaged changes, so only the
        // carry goes back. If it cannot, keep it and try again to record it,
        // with nothing to continue, so `kin abort` restores it.
        match set_aside::restore(repo, &carry_stash, Phase::Unwind) {
            Outcome::NotRestored { .. } => {
                state.abort_only = true;
                if let Err(save_err) = save_state(repo, state) {
                    return Err(err.context(format!(
                        "Additionally, recording the staged changes set aside in stash entry '{}' failed ({save_err:#}).",
                        carry_stash.stash
                    )));
                }
            }
            outcome => {
                state.set_asides.take_carry();
                // No saved journal ever recorded the carry.
                if outcome == Outcome::Restored {
                    set_aside::drop_restored(&carry_stash);
                }
            }
        }
        return Err(err);
    }

    if let Err(err) = checkout_branch(target_branch) {
        // Nothing moved, so put the staged content back on the branch it was
        // taken from and leave the rest of the state for `kin abort`. Nothing
        // was committed either, so there is nothing for `kin continue` to do.
        let outcome = set_aside::restore(repo, &carry_stash, Phase::Unwind);
        if !matches!(outcome, Outcome::NotRestored { .. }) {
            state.set_asides.take_carry();
        }
        state.abort_only = true;
        // The entry goes once the journal no longer records it, and also if
        // saving that failed, as `set_aside::restore_all` does: the saved
        // journal then skips a carry whose entry is gone rather than apply
        // it twice.
        let saved = save_state(repo, state);
        if outcome == Outcome::Restored {
            set_aside::drop_restored(&carry_stash);
        }
        saved?;
        return Err(err.context(
            "Failed to checkout target branch. Use 'kin abort' to restore original state.",
        ));
    }

    match set_aside::restore(repo, &carry_stash, Phase::Completion) {
        Outcome::Restored => {
            state.set_asides.take_carry();
            // Dropped after the save, whether or not it succeeded, as above.
            let saved = save_state(repo, state);
            set_aside::drop_restored(&carry_stash);
            saved
        }
        Outcome::ConflictsLeft { .. } => {
            // A real conflict between the staged change and the target branch —
            // the one case a switch genuinely cannot carry. The stash entry still
            // holds the change, so discard the conflicted merge and unwind the
            // whole operation: the user is left where they started, on their own
            // branch with their changes as they were, rather than mid-merge on a
            // branch they didn't ask to be on.
            //
            // Each set-aside stays in the state until it is back, and the state
            // is cleared only once all of them are; otherwise it is kept, with
            // nothing left to continue, so `kin abort` restores the rest. The
            // carry is newest, so it goes back first, onto the branch it was
            // taken on.
            let unwound = discard_conflicted_carry(&caller_branch)
                .map(|()| set_aside::unwind(repo, &mut state.set_asides));
            let err = anyhow!(
                "The staged changes conflict with '{}', so committing them there would leave conflicts to resolve. Restack this branch onto '{}' first, or move the overlapping changes by hand.",
                target_branch,
                target_branch
            );
            let unwind_err = match unwound {
                Ok(Outcome::Restored) => {
                    clear_state(repo)?;
                    return Err(err);
                }
                Ok(_) => anyhow!("see the warning above"),
                Err(unwind_err) => unwind_err,
            };
            state.abort_only = true;
            save_state(repo, state)?;
            Err(err.context(format!(
                "Additionally, restoring the changes on '{caller_branch}' did not complete ({unwind_err:#}). Clear the way, then run 'kin abort' to restore them."
            )))
        }
        Outcome::NotRestored { reason } => Err(anyhow!(reason).context(
            "Failed to move the staged changes onto the target branch. Use 'kin abort' to restore original state.",
        )),
    }
}

/// Drop a conflicted carry from the tree and return to `caller_branch`, where
/// the carry applies cleanly again.
fn discard_conflicted_carry(caller_branch: &str) -> Result<()> {
    let status = Command::new("git").args(["reset", "--hard"]).status()?;
    if !status.success() {
        return Err(anyhow!(
            "Failed to discard the conflicted changes from the working tree."
        ));
    }
    checkout_branch(caller_branch)
}

fn stash_non_staged_changes(repo: &Repository) -> Result<Option<SetAside>> {
    let changes = set_aside::take(repo, set_aside::Kind::UnstagedOnly, "kin-commit-on")?;
    if changes.is_some() {
        // set_aside::take captures git's own "Saved working directory…"
        // confirmation (it would leak the internal stash token), so tell the
        // user their non-staged work was set aside, not lost.
        println!(
            "Set aside non-staged changes; they will be restored when the operation completes."
        );
    }
    Ok(changes)
}

fn resolve_fixup_commit(
    repo: &Repository,
    commits: &[StackCommit],
    fixup_target: &str,
) -> Result<StackCommit> {
    if commits.is_empty() {
        return Err(anyhow!("No commits found in the stack."));
    }

    let target_id = repo
        .revparse_single(fixup_target)
        .with_context(|| format!("Could not resolve '{}' to a commit.", fixup_target))?
        .peel_to_commit()
        .with_context(|| format!("'{}' does not refer to a commit.", fixup_target))?
        .id();

    commits
        .iter()
        .find(|c| c.commit_id == target_id)
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "Commit '{}' is not part of the current stack. Only commits in the current stack can be fixed up.",
                fixup_target
            )
        })
}

fn select_commit_interactive(commits: &[StackCommit]) -> Result<StackCommit> {
    if commits.is_empty() {
        return Err(anyhow!("No commits found in the stack."));
    }

    // The amend picker uses its own scripted seam (a single index) rather than
    // the sequential `prompt_select` counter, so it is resolved here directly.
    let mode = crate::interaction::current();
    if !mode.is_interactive() {
        if let Some(idx) = mode.scripted().and_then(|s| s.single_selection())
            && idx < commits.len()
        {
            return Ok(commits[idx].clone());
        }
        if mode.scripted().is_none() {
            return Err(crate::interaction::input_required(
                "Cannot pick a commit to amend without a terminal.",
            ));
        }
        return Ok(commits[0].clone());
    }

    let display: Vec<String> = commits
        .iter()
        .map(|c| {
            format!(
                "{} {}/{} - \"{}\"",
                c.branch_name, c.position.0, c.position.1, c.message
            )
        })
        .collect();

    let selected_display = crate::commands::prompt_select(
        "Select commit to amend:",
        display,
        crate::commands::Fallback::Require("Cannot pick a commit to amend without a terminal."),
    )?;

    let index = commits
        .iter()
        .position(|c| {
            let rendered = format!(
                "{} {}/{} - \"{}\"",
                c.branch_name, c.position.0, c.position.1, c.message
            );
            rendered == selected_display
        })
        .ok_or_else(|| anyhow!("Failed to resolve selected commit."))?;

    Ok(commits[index].clone())
}

#[cfg(test)]
mod tests {
    #[test]
    fn non_resolution_commit_flags_are_detected_in_bundles_but_not_in_values() {
        let detects = |args: &[&str]| {
            forwards_non_resolution_commit_flag(
                &args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>(),
            )
        };
        assert!(detects(&["-a", "-m", "msg"]));
        assert!(detects(&["--all"]));
        assert!(detects(&["-am", "msg"]));
        assert!(detects(&["-vp"]));
        assert!(detects(&["--amend", "--no-edit"]));
        assert!(!detects(&["-m", "-p"]));
        assert!(!detects(&["-mall"]));
        assert!(!detects(&["--message", "--all"]));
        assert!(!detects(&["-s", "-m", "msg"]));
        assert!(!detects(&["--", "-a"]));
    }

    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_strips_global_yes_from_git_args() {
        let parsed = parse_commit_args(&args(&["--amend", "--yes", "--no-edit"])).unwrap();
        assert_eq!(parsed.git_commit_args, args(&["--amend", "--no-edit"]));
    }

    #[test]
    fn parse_strips_global_no_interactive_from_git_args() {
        let parsed = parse_commit_args(&args(&["--no-interactive", "-m", "msg"])).unwrap();
        assert_eq!(parsed.git_commit_args, args(&["-m", "msg"]));
    }

    #[test]
    fn parse_keeps_global_flags_after_double_dash_as_pathspecs() {
        // Everything after `--` is a literal git pathspec and must be forwarded
        // verbatim, including a file that happens to be named `--yes`.
        let parsed =
            parse_commit_args(&args(&["-m", "msg", "--", "--yes", "--no-interactive"])).unwrap();
        assert_eq!(
            parsed.git_commit_args,
            args(&["-m", "msg", "--", "--yes", "--no-interactive"])
        );
    }

    #[test]
    fn recover_flags_defaults_to_cli_values() {
        assert_eq!(
            recover_interaction_flags(&args(&["--amend"]), false, false),
            (false, false)
        );
    }

    #[test]
    fn recover_flags_picks_up_yes_after_subcommand() {
        assert_eq!(
            recover_interaction_flags(&args(&["--amend", "--yes"]), false, false),
            (false, true)
        );
    }

    #[test]
    fn recover_flags_picks_up_no_interactive_after_subcommand() {
        assert_eq!(
            recover_interaction_flags(&args(&["--no-interactive", "--amend"]), false, false),
            (true, false)
        );
    }

    #[test]
    fn recover_flags_ors_with_cli_values() {
        // A flag already bound to `Cli` (before the subcommand) stays set even
        // when absent from the pass-through args.
        assert_eq!(
            recover_interaction_flags(&args(&["--amend"]), false, true),
            (false, true)
        );
    }

    #[test]
    fn recover_flags_ignores_tokens_after_double_dash() {
        // `kin commit -- --yes` targets a pathspec named `--yes`; it must not be
        // treated as the interaction flag.
        assert_eq!(
            recover_interaction_flags(&args(&["-m", "msg", "--", "--yes"]), false, false),
            (false, false)
        );
    }

    #[test]
    fn parse_new_branch_with_explicit_name() {
        let parsed = parse_commit_args(&args(&["-b", "topic", "-m", "msg"])).unwrap();
        assert_eq!(parsed.new_branch, Some(Some("topic".to_string())));
        assert!(!parsed.insert);
        assert_eq!(parsed.git_commit_args, args(&["-m", "msg"]));
    }

    #[test]
    fn parse_new_branch_without_name_derives_from_message() {
        // A following flag is not consumed as the branch name.
        let parsed = parse_commit_args(&args(&["-b", "-m", "msg"])).unwrap();
        assert_eq!(parsed.new_branch, Some(None));
        assert_eq!(parsed.git_commit_args, args(&["-m", "msg"]));
    }

    #[test]
    fn parse_new_branch_equals_form_and_insert() {
        let parsed =
            parse_commit_args(&args(&["--new-branch=mid", "--insert", "-m", "x"])).unwrap();
        assert_eq!(parsed.new_branch, Some(Some("mid".to_string())));
        assert!(parsed.insert);
        assert_eq!(parsed.git_commit_args, args(&["-m", "x"]));
    }

    #[test]
    fn parse_insert_without_new_branch_is_rejected() {
        let err = parse_commit_args(&args(&["--insert", "-m", "x"])).unwrap_err();
        assert!(err.to_string().contains("--insert requires --new-branch"));
    }

    #[test]
    fn parse_new_branch_conflicts_with_on() {
        let err = parse_commit_args(&args(&["-b", "topic", "--on", "main"])).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn message_subject_reads_first_message() {
        assert_eq!(
            message_subject_from_args(&args(&["-m", "First line", "-m", "body"])).as_deref(),
            Some("First line")
        );
        assert_eq!(
            message_subject_from_args(&args(&["--message=Hello there"])).as_deref(),
            Some("Hello there")
        );
        // Glued short form `-mSubject`.
        assert_eq!(
            message_subject_from_args(&args(&["-mGlued subject"])).as_deref(),
            Some("Glued subject")
        );
    }

    #[test]
    fn message_subject_reads_clustered_short_options() {
        // `-am msg` — the `-m` is bundled after `-a`, value in the next arg.
        assert_eq!(
            message_subject_from_args(&args(&["-am", "Bundled subject"])).as_deref(),
            Some("Bundled subject")
        );
        // `-amGlued` — bundled with a glued value.
        assert_eq!(
            message_subject_from_args(&args(&["-amGlued"])).as_deref(),
            Some("Glued")
        );
        // A value-taking flag before `m` swallows the rest, so it is not a message.
        assert_eq!(message_subject_from_args(&args(&["-Cmaybe"])), None);
    }

    #[test]
    fn message_subject_takes_only_first_line() {
        assert_eq!(
            message_subject_from_args(&args(&["-m", "subject\n\nbody"])).as_deref(),
            Some("subject")
        );
    }

    #[test]
    fn message_subject_absent_without_inline_message() {
        assert_eq!(message_subject_from_args(&args(&["--amend"])), None);
        // A `-m` after `--` is a pathspec, not a message.
        assert_eq!(
            message_subject_from_args(&args(&["--", "-m", "file"])),
            None
        );
    }
}
