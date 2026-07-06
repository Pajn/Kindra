use crate::commands::{find_upstream, resolve_rebase_autostash};
use crate::rebase_utils::{
    RebaseState, apply_stash, check_worktrees, checkout_branch, clear_state, drop_stash,
    git_rebase_in_progress, passively_reconcile_rebase_state, record_branch_tips_in_range,
    restore_stashed_changes, run_rebase_loop, save_state, stash_push_changes,
};
use crate::stack::{
    StackBranch, StackCommit, collect_descendants, enumerate_stack_commits,
    get_stack_branches_from_merge_base,
};
use anyhow::{Context, Result, anyhow};
use git2::{BranchType, Oid, Repository};
use std::collections::HashMap;
use std::process::Command;

pub fn commit(args: &[String]) -> Result<()> {
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
    .ok_or_else(|| anyhow!("You must be on a branch to use 'commit'"))?;

    let upstream_name = find_upstream(&repo)?.ok_or_else(|| {
        anyhow!("Could not find a base branch (init.defaultBranch, main, master, or trunk)")
    })?;
    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let upstream_id = upstream_obj.id();
    let head_id = head.peel_to_commit()?.id();
    let mut parsed = parse_commit_args(args)?;
    let autostash = resolve_rebase_autostash(&repo, parsed.autostash)?;
    let on_flag = parsed.on_target.is_some();

    let current_stack = build_stack_context(&repo, head_id, upstream_id, &upstream_name)
        .with_context(|| {
            format!(
                "Failed to discover stack context for current branch '{}'.",
                current_branch_name
            )
        })?;

    let interactive_selection = if parsed.interactive {
        let commits =
            enumerate_stack_commits(&repo, &current_stack.stack_branches, &upstream_name)?;
        Some(select_commit_interactive(&commits)?)
    } else if let Some(fixup_target) = &parsed.fixup_target {
        let commits =
            enumerate_stack_commits(&repo, &current_stack.stack_branches, &upstream_name)?;
        Some(resolve_fixup_commit(&repo, &commits, fixup_target)?)
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
    let requires_staged_changes = !parsed
        .git_commit_args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-a" | "--all" | "-p" | "--patch"))
        && !has_forwarded_pathspec(&parsed.git_commit_args);
    if interactive_selection.is_some() && requires_staged_changes && !has_staged_changes(&repo)? {
        return Err(anyhow!("nothing to commit, working tree clean"));
    }

    let target_branch = match &interactive_selection {
        // An inline fixup stays on the current branch: it folds into an ancestor
        // and restacks descendants, never switching to the target's branch.
        Some(_) if inline_fixup => current_branch_name.clone(),
        Some(sel) => sel.branch_name.clone(),
        None => match parsed.on_target {
            None => current_branch_name.clone(),
            Some(Some(ref branch_name)) => branch_name.clone(),
            Some(None) => select_target_branch(
                &repo,
                &current_branch_name,
                head_id,
                &current_stack.stack_branches,
            )?,
        },
    };

    repo.find_branch(&target_branch, BranchType::Local)
        .with_context(|| format!("Target branch '{}' not found.", target_branch))?;
    let target_old_head_id = repo.revparse_single(&target_branch)?.id();
    let target_in_current_context = target_branch == upstream_name
        || current_stack
            .stack_branches
            .iter()
            .any(|b| b.name == target_branch);

    let target_stack = build_stack_context(&repo, target_old_head_id, upstream_id, &upstream_name)?;
    let target_sub_stack = collect_target_sub_stack(
        &repo,
        &target_branch,
        target_old_head_id,
        &upstream_name,
        &target_stack.stack_branches,
    )?;
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
    crate::stack::sort_branches_topologically(&repo, &mut sub_stack)?;

    let remaining_branches: Vec<String> = sub_stack
        .iter()
        .filter(|sb| sb.name != target_branch)
        .map(|sb| sb.name.clone())
        .collect();

    let will_rebase = should_rebase && target_has_dependents && !remaining_branches.is_empty();
    let needs_autosquash = is_fixup;
    let autosquash_state_required = needs_autosquash && !switching_branches && !will_rebase;

    // The check_worktrees call must run before the code path that performs the commit and
    // mutates target_branch so failures don't leave state unpersisted.
    if will_rebase || needs_autosquash {
        check_worktrees(&remaining_branches, parsed.force)?;
    }

    // The autosquash below rewrites branch tips with `--update-refs` (git >= 2.38).
    // Verify support up front, before `git commit` creates a `fixup!` commit or
    // any state is stashed/saved, so an unsupported git fails cleanly with nothing
    // left to undo.
    if needs_autosquash {
        crate::rebase_utils::ensure_git_supports_update_refs()?;
    }

    let pre_commit_state_required = switching_branches || will_rebase;
    if pre_commit_state_required || needs_autosquash {
        let (parent_id_map, parent_name_map) = if will_rebase {
            crate::stack::build_parent_maps(
                &repo,
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
                &repo,
                &fixup_commit_id,
                target_old_head_id,
                &mut original_tip_map,
            )?;
        }

        let mut state = RebaseState {
            operation: crate::rebase_utils::Operation::Commit,
            original_branch: target_branch.clone(),
            target_branch: target_branch.clone(),
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
            stash_ref: None,
            stash_apply_index: false,
            preserve_content_on_abort: false,
            suppress_editor: false,
            unstage_on_restore: switching_branches,
            autostash,
            cleanup_merged_branches: Vec::new(),
            cleanup_checkout_fallback: None,
        };

        // Deliberate exception to the uniform clean-or-autostash contract: when
        // committing onto another branch (`--on`), keep the *staged* changes (what
        // we're committing there) while setting the *unstaged* ones aside via
        // `git stash --keep-index --include-untracked`. Take it only now — after
        // the fallible planning above — so a failure there can't strand the user's
        // changes, and record it in the saved state right away.
        if switching_branches {
            state.stash_ref = stash_non_staged_changes()?;
        }

        if pre_commit_state_required && let Err(err) = save_state(&repo, &state) {
            // Persisting failed, so no later `kin continue`/`abort` knows about
            // the stash; pop it back rather than stranding the user's changes.
            restore_stashed_changes(state.stash_ref.take());
            return Err(err);
        }

        if switching_branches && let Err(err) = checkout_branch(&target_branch) {
            return Err(err.context(
                "Failed to checkout target branch. Use 'kin abort' to restore original state.",
            ));
        }

        // Run the actual git commit
        let status = Command::new("git")
            .arg("commit")
            .args(&parsed.git_commit_args)
            .status()?;
        if !status.success() {
            if pre_commit_state_required {
                return Err(anyhow!(
                    "git commit failed. Resolve and run 'kin continue', or run 'kin abort'."
                ));
            }
            return Err(anyhow!("git commit failed"));
        }

        if needs_autosquash {
            if autosquash_state_required {
                // Take the pre-rebase stash only *after* the fixup commit: the
                // staged content we're folding in is now committed, so a
                // `--keep-index` stash captures genuinely unstaged leftovers rather
                // than re-capturing (and later re-applying) the fixup content. If
                // stashing fails, undo the fixup commit we just created so the
                // failure can't strand a `fixup!` commit without a recoverable
                // state.
                state.stash_ref = match stash_non_staged_changes() {
                    Ok(stash_ref) => stash_ref,
                    Err(err) => {
                        // Roll back the fixup commit we just created. If the reset
                        // itself fails, surface that explicitly — a stray `fixup!`
                        // commit is now stranded at HEAD and the user must remove it.
                        match Command::new("git")
                            .args(["reset", "--soft", "HEAD^"])
                            .status()
                        {
                            Ok(status) if status.success() => return Err(err),
                            _ => {
                                return Err(err.context(
                                    "Additionally, failed to roll back the fixup commit; a stray 'fixup!' commit remains at HEAD. Remove it with 'git reset --soft HEAD^'.",
                                ));
                            }
                        }
                    }
                };
                if let Err(err) = save_state(&repo, &state) {
                    // Persisting failed; pop the stash back rather than leaving
                    // the user's unstaged changes stranded.
                    restore_stashed_changes(state.stash_ref.take());
                    return Err(err);
                }
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
                .arg("--autosquash");
            if autostash {
                cmd.arg("--autostash");
            }
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

            let status = cmd.status()?;

            if !status.success() {
                if git_rebase_in_progress(&repo) {
                    // The autosquash rebase paused on a conflict. Record which
                    // branch is mid-rebase so `kin continue` matches the saved
                    // state — required whether or not the target has dependents
                    // (a missing in_progress_branch makes `kin continue` refuse).
                    state.in_progress_branch = Some(target_branch.clone());
                    save_state(&repo, &state)?;
                } else if autosquash_state_required {
                    // autosquash_state_required implies no dependents/switch, so
                    // this only runs on the single-branch path. The rebase failed
                    // without a resumable state, so put the user's autostash back
                    // before surfacing the error.
                    restore_autostash(&repo, &mut state)?;
                }
                return Err(anyhow!(
                    "git rebase --autosquash failed. Resolve conflicts and run 'kin continue', or run 'kin abort'."
                ));
            }

            if autosquash_state_required {
                restore_autostash(&repo, &mut state)?;
                clear_state(&repo)?;
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

struct StackContext {
    merge_base: Oid,
    stack_branches: Vec<StackBranch>,
}

#[derive(Default)]
struct ParsedCommitArgs {
    on_target: Option<Option<String>>,
    interactive: bool,
    fixup_target: Option<String>,
    force: bool,
    autostash: Option<bool>,
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

        if arg == "--force" {
            parsed.force = true;
            idx += 1;
            continue;
        }

        if arg == "--autostash" {
            parsed.autostash = Some(true);
            idx += 1;
            continue;
        }

        if arg == "--no-autostash" {
            parsed.autostash = Some(false);
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

/// Reapply the autostash recorded in `state` (if any), drop it, and only then
/// clear the `stash_ref` / `in_progress_branch` fields and persist.
///
/// The ordering matters: `apply_stash` runs *before* the saved state stops
/// referencing the stash. If it fails, the on-disk state still points at the
/// stash, so `kin abort` can recover the user's changes instead of orphaning
/// them. `save_state` already persists `stash_ref` before the autosquash rebase,
/// so no state is lost on the failure path.
fn restore_autostash(repo: &Repository, state: &mut RebaseState) -> Result<()> {
    let Some(stash_ref) = state.stash_ref.clone() else {
        return Ok(());
    };
    apply_stash(&stash_ref)?;
    if let Err(err) = drop_stash(&stash_ref) {
        eprintln!("Warning: {}", err);
    }
    state.stash_ref = None;
    state.in_progress_branch = None;
    save_state(repo, state)
}

fn stash_non_staged_changes() -> Result<Option<String>> {
    let stash_ref = stash_push_changes(true, "kin-commit-on")?;
    if stash_ref.is_some() {
        // stash_push_changes captures git's own "Saved working directory…"
        // confirmation (it would leak the internal stash token), so tell the
        // user their non-staged work was set aside, not lost.
        println!(
            "Set aside non-staged changes; they will be restored when the operation completes."
        );
    }
    Ok(stash_ref)
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
}
