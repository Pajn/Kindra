use crate::commands::find_upstream;
use crate::rebase_utils::passively_reconcile_rebase_state;
use crate::stack::{
    get_stack_branches_from_merge_base, resolve_merge_base, sort_branches_topologically,
};
use anyhow::{Context, Result, anyhow};
use clap::Args;
use git2::Repository;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// CLI arguments for the run command
#[derive(Args, Debug, Clone, Serialize, Deserialize)]
pub struct RunArgs {
    /// The command to run on each branch
    #[arg(short, long)]
    pub command: String,

    /// Continue on failure instead of stopping at the first error
    #[arg(long)]
    pub continue_on_failure: bool,

    /// Stash uncommitted changes for the duration of the run and restore them
    /// when it finishes (defaults to the rebase.autostash config)
    #[arg(long, overrides_with = "no_autostash")]
    #[serde(default, skip)]
    pub autostash: bool,

    /// Refuse to run with a dirty working tree even if autostash is configured
    #[arg(long = "no-autostash", overrides_with = "autostash")]
    #[serde(default, skip)]
    pub no_autostash: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunStatus {
    InProgress,
    Failed,
    Aborted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunState {
    pub target_branches: Vec<String>,
    pub current_index: usize,
    pub args: RunArgs,
    pub original_branch: Option<String>,
    pub original_head_id: String,
    pub status: RunStatus,
    #[serde(default)]
    pub failed_branches: Vec<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// Autostash ref set aside for the duration of the run; restored (applied +
    /// dropped) on any terminal exit — success, failure, or `kin abort`.
    #[serde(default)]
    pub stash_ref: Option<String>,
}

pub fn run(args: &RunArgs) -> Result<()> {
    let repo = crate::open_repo()?;
    let _lock = crate::state_io::RepoLock::acquire(&repo)?;
    if passively_reconcile_rebase_state(&repo)? || run_state_exists(&repo) {
        return Err(anyhow!(
            "A Kindra operation is already in progress. Use 'kin continue' or 'kin abort'."
        ));
    }

    let upstream_name = find_upstream(&repo)?.ok_or_else(|| {
        anyhow!("Could not find a base branch (init.defaultBranch, main, master, or trunk)")
    })?;

    let head = repo.head()?;
    let head_id = head.peel_to_commit()?.id();
    let current_branch_name = if !repo.head_detached()? {
        head.shorthand().map(|s| s.to_string())
    } else {
        None
    };

    let upstream_obj = repo.revparse_single(&upstream_name)?;
    let upstream_id = upstream_obj.id();
    let merge_base = resolve_merge_base(&repo, upstream_id, head_id)?;

    let mut stack_branches = get_stack_branches_from_merge_base(
        &repo,
        merge_base,
        head_id,
        upstream_id,
        &upstream_name,
    )?;

    if stack_branches.is_empty() {
        println!("No branches found in the current stack.");
        return Ok(());
    }

    // Sort from base to tips (topological order)
    sort_branches_topologically(&repo, &mut stack_branches)?;

    // Enforce the uniform clean-or-autostash contract before checking out any
    // branch, so uncommitted changes never travel across the stack.
    let autostash = crate::commands::resolve_rebase_autostash(
        &repo,
        crate::commands::autostash_override(args.autostash, args.no_autostash),
    )?;
    let stash_ref = crate::rebase_utils::take_autostash(&repo, autostash)?;

    let mut run_state = RunState {
        target_branches: stack_branches.into_iter().map(|b| b.name).collect(),
        current_index: 0,
        args: args.clone(),
        original_branch: current_branch_name,
        original_head_id: head_id.to_string(),
        status: RunStatus::InProgress,
        failed_branches: Vec::new(),
        last_error: None,
        stash_ref,
    };
    // If persisting fails, nothing downstream knows to restore the autostash, so
    // pop it back now rather than stranding the user's uncommitted changes.
    if let Err(err) = persist_run_state(&repo, &run_state) {
        restore_run_stash(&mut run_state);
        return Err(err);
    }
    execute_run(&repo, &mut run_state)
}

pub(crate) fn abort_run(repo: &Repository) -> Result<()> {
    let mut run_state = load_run_state(repo)?;
    mark_aborted(repo, &mut run_state, None)?;
    checkout_original_checkout(&run_state.original_branch, &run_state.original_head_id)?;
    restore_run_stash(&mut run_state);
    clear_run_state(repo)?;
    println!("Run operation aborted (state cleared).");
    Ok(())
}

pub(crate) fn run_state_path(repo: &Repository) -> PathBuf {
    repo.path().join("kindra_run_state.json")
}

pub(crate) fn run_state_exists(repo: &Repository) -> bool {
    run_state_path(repo).exists()
}

pub(crate) fn load_run_state(repo: &Repository) -> Result<RunState> {
    let path = run_state_path(repo);
    if !path.exists() {
        return Err(anyhow!("No run operation in progress."));
    }
    let json = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&json)?)
}

fn persist_run_state(repo: &Repository, run_state: &RunState) -> Result<()> {
    let json = serde_json::to_string_pretty(run_state)?;
    crate::state_io::write_atomic(&run_state_path(repo), &json)?;
    Ok(())
}

/// Restore the autostash set aside at the start of the run, if any. Called on
/// every terminal exit — success, failure, or `kin abort` — once HEAD is back on
/// the original checkout, so uncommitted work lands where the user left it.
/// (A run is not resumable: leftover state after a failed checkout-restore is
/// recovered by `kin abort`, not `kin continue`.)
fn restore_run_stash(run_state: &mut RunState) {
    let Some(stash_ref) = run_state.stash_ref.take() else {
        return;
    };
    if let Err(err) = crate::rebase_utils::apply_stash(&stash_ref) {
        // `run` is a reporter, not a resumable operation, so callers clear the
        // run-state file after this returns — keeping the ref in the (dropped)
        // state would lose it. Surface an actionable message instead, so the
        // surviving stash entry isn't orphaned silently. A failed apply does not
        // drop the stash, so the user's changes remain recoverable.
        eprintln!(
            "Warning: could not reapply your autostashed changes: {err}\n         \
             They are preserved in `git stash list` (\"{stash_ref}\"); reapply with `git stash pop`."
        );
        return;
    }
    if let Err(err) = crate::rebase_utils::drop_stash(&stash_ref) {
        eprintln!("Warning: {err}");
    }
}

fn clear_run_state(repo: &Repository) -> Result<()> {
    let path = run_state_path(repo);
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn persist_failure(
    repo: &Repository,
    run_state: &mut RunState,
    message: impl Into<String>,
) -> Result<()> {
    run_state.status = RunStatus::Failed;
    run_state.last_error = Some(message.into());
    persist_run_state(repo, run_state)
}

fn mark_aborted(
    repo: &Repository,
    run_state: &mut RunState,
    message: Option<String>,
) -> Result<()> {
    run_state.status = RunStatus::Aborted;
    run_state.last_error = message;
    persist_run_state(repo, run_state)
}

fn execute_run(repo: &Repository, run_state: &mut RunState) -> Result<()> {
    let mut success_count = 0usize;
    let mut failure_count = 0usize;

    while run_state.current_index < run_state.target_branches.len() {
        let branch = run_state.target_branches[run_state.current_index].clone();
        println!("\n=== Running on {} ===", branch);

        if let Err(err) = run_git_checkout(&branch) {
            eprintln!("Failed to checkout branch {}: {}", branch, err);
            record_branch_failure(run_state, &branch);
            failure_count += 1;

            if !run_state.args.continue_on_failure {
                let state_error = format!("Failed to checkout branch '{}': {}", branch, err);
                return fail_and_restore(repo, run_state, &state_error);
            }

            run_state.current_index += 1;
            persist_run_state(repo, run_state)?;
            continue;
        }

        let output = Command::new("sh")
            .arg("-c")
            .arg(&run_state.args.command)
            .output();

        match output {
            Ok(output) => {
                if !output.stdout.is_empty() {
                    print!("{}", String::from_utf8_lossy(&output.stdout));
                }
                if !output.stderr.is_empty() {
                    eprint!("{}", String::from_utf8_lossy(&output.stderr));
                }

                if output.status.success() {
                    success_count += 1;
                    clear_branch_failure(run_state, &branch);
                    run_state.current_index += 1;
                    persist_run_state(repo, run_state)?;
                } else {
                    failure_count += 1;
                    record_branch_failure(run_state, &branch);

                    if !run_state.args.continue_on_failure {
                        let state_error = format!(
                            "Command failed on branch '{}' with exit code {:?}.",
                            branch,
                            output.status.code()
                        );
                        eprintln!("\n{}", state_error);
                        return fail_and_restore(repo, run_state, &state_error);
                    }

                    run_state.current_index += 1;
                    persist_run_state(repo, run_state)?;
                }
            }
            Err(err) => {
                eprintln!("Failed to execute command: {}", err);
                failure_count += 1;
                record_branch_failure(run_state, &branch);

                if !run_state.args.continue_on_failure {
                    let state_error =
                        format!("Failed to execute command on branch '{}': {}", branch, err);
                    return fail_and_restore(repo, run_state, &state_error);
                }

                run_state.current_index += 1;
                persist_run_state(repo, run_state)?;
            }
        }
    }

    if let Err(restore_err) =
        checkout_original_checkout(&run_state.original_branch, &run_state.original_head_id)
    {
        // Record why the restore failed so `kin status` can show it and `kin
        // abort` can recover the stranded autostash, instead of leaving RunState
        // stuck InProgress with no reason. Mirrors fail_and_restore's error path.
        let msg = format!("Failed to restore original checkout after run: {restore_err}");
        persist_failure(repo, run_state, msg.clone())?;
        return Err(anyhow!(msg));
    }

    println!("\n=== Summary ===");
    println!("Succeeded: {}", success_count);
    println!("Failed: {}", failure_count);
    if !run_state.failed_branches.is_empty() {
        println!("Failed branches: {}", run_state.failed_branches.join(", "));
    }

    // `run` is a reporter, not a resumable operation. Whether or not the command
    // failed, the original checkout (above) and autostash are restored and the
    // state is cleared, so a non-zero command never leaves a blocking operation
    // behind. Failure is surfaced only through the error / exit code.
    restore_run_stash(run_state);
    clear_run_state(repo)?;

    if run_state.failed_branches.is_empty() {
        return Ok(());
    }

    Err(anyhow!(
        "Command failed on {} branch(es): {}",
        run_state.failed_branches.len(),
        run_state.failed_branches.join(", ")
    ))
}

fn fail_and_restore(repo: &Repository, run_state: &mut RunState, state_error: &str) -> Result<()> {
    // Same reporter contract as the end-of-loop path: restore the original
    // checkout + autostash and clear state, so stopping at the first failure
    // does not leave a blocking operation. Only a genuine failure to restore the
    // checkout keeps state, so `kin abort` can recover the stranded autostash.
    match checkout_original_checkout(&run_state.original_branch, &run_state.original_head_id) {
        Ok(()) => {
            restore_run_stash(run_state);
            clear_run_state(repo)?;
            Err(anyhow!(state_error.to_string()))
        }
        Err(restore_err) => {
            let combined = format!(
                "{} Failed to restore original checkout: {}",
                state_error, restore_err
            );
            persist_failure(repo, run_state, combined.clone())?;
            Err(anyhow!(combined))
        }
    }
}

fn record_branch_failure(run_state: &mut RunState, branch: &str) {
    if !run_state.failed_branches.iter().any(|b| b == branch) {
        run_state.failed_branches.push(branch.to_string());
    }
}

fn clear_branch_failure(run_state: &mut RunState, branch: &str) {
    run_state.failed_branches.retain(|b| b != branch);
}

fn checkout_original_checkout(
    original_branch: &Option<String>,
    original_head_id: &str,
) -> Result<()> {
    if let Some(branch) = original_branch {
        run_git_checkout(branch)
            .with_context(|| format!("Failed to checkout original branch '{}'.", branch))
    } else {
        run_git_checkout(original_head_id).with_context(|| {
            format!(
                "Failed to checkout original detached HEAD '{}'.",
                original_head_id
            )
        })
    }
}

fn run_git_checkout(target: &str) -> Result<()> {
    let status = Command::new("git").arg("checkout").arg(target).status()?;
    if !status.success() {
        return Err(anyhow!(
            "git checkout '{}' exited with non-zero status",
            target
        ));
    }
    Ok(())
}
