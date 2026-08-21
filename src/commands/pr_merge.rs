use crate::commands::pr::{
    PrMergeArgs, StackPr, collect_open_stack_prs, discover_stack_branches_with_upstream,
    normalize_base_for_gh, parse_github_owner_repo_from_pr_url, select_stack_pr,
};
use crate::commands::sync::{SyncArgs, sync};
use crate::gh;
use crate::stack::{StackBranch, compute_base_map};
use anyhow::{Context, Result, anyhow};
use git2::Repository;

enum MergeOutcome {
    Merged,
    Pending(String),
}

#[derive(Debug)]
pub(crate) struct PrMergeAssessment {
    pub(crate) outstanding_reviews: Vec<String>,
    pub(crate) unresolved_comments: usize,
    pub(crate) running_checks: Vec<String>,
    pub(crate) failed_checks: Vec<String>,
    pub(crate) repo_allows_merge: bool,
    pub(crate) repo_block_reason: Option<String>,
}

pub(crate) fn assess_pr_mergeability(status: &gh::PrStatusSummary) -> PrMergeAssessment {
    let mut outstanding_reviews = status
        .reviewer_statuses
        .iter()
        .filter(|reviewer| {
            !reviewer.status.eq_ignore_ascii_case("approved")
                && !reviewer.status.eq_ignore_ascii_case("commented")
                && !reviewer.status.eq_ignore_ascii_case("comments")
        })
        .map(|reviewer| format!("{}: {}", reviewer.reviewer, reviewer.status))
        .collect::<Vec<_>>();

    if let Some(review_decision) = &status.review_decision
        && !review_decision.eq_ignore_ascii_case("APPROVED")
    {
        let normalized = review_decision.to_ascii_lowercase().replace('_', " ");
        let summary = format!("overall review decision: {normalized}");
        if !outstanding_reviews.contains(&summary) {
            outstanding_reviews.push(summary);
        }
    }

    let repo_allows_merge = !status.is_draft
        && status.mergeable.eq_ignore_ascii_case("MERGEABLE")
        && matches!(status.merge_state_status.as_str(), "CLEAN" | "UNSTABLE");

    let repo_block_reason = if status.is_draft {
        Some("PR is still marked as draft".to_string())
    } else if !status.mergeable.eq_ignore_ascii_case("MERGEABLE") {
        Some(format!("GitHub mergeability is {}", status.mergeable))
    } else if !matches!(status.merge_state_status.as_str(), "CLEAN" | "UNSTABLE") {
        Some(format!(
            "GitHub merge state is {}",
            status.merge_state_status
        ))
    } else {
        None
    };

    PrMergeAssessment {
        outstanding_reviews,
        unresolved_comments: status.unresolved_comments,
        running_checks: status.running_checks.clone(),
        failed_checks: status.failed_checks.clone(),
        repo_allows_merge,
        repo_block_reason,
    }
}

pub(crate) fn render_pr_merge_summary(
    branch_name: &str,
    pr: &gh::EditablePr,
    assessment: &PrMergeAssessment,
) -> String {
    let mut lines = vec![format!(
        "PR #{} for {} is not ready to merge:",
        pr.number, branch_name
    )];

    if assessment.unresolved_comments > 0 {
        lines.push(format!(
            "  - Unresolved review comments: {}",
            assessment.unresolved_comments
        ));
    }

    if !assessment.outstanding_reviews.is_empty() {
        lines.push("  - Outstanding reviews:".to_string());
        lines.extend(
            assessment
                .outstanding_reviews
                .iter()
                .map(|review| format!("    - {review}")),
        );
    }

    if !assessment.running_checks.is_empty() {
        lines.push(format!(
            "  - Running checks: {}",
            assessment.running_checks.join(", ")
        ));
    }

    if !assessment.failed_checks.is_empty() {
        lines.push(format!(
            "  - Failed checks: {}",
            assessment.failed_checks.join(", ")
        ));
    }

    if let Some(reason) = &assessment.repo_block_reason {
        lines.push(format!("  - Merge blocked by GitHub: {reason}"));
    } else {
        lines.push("  - GitHub would still allow merging this PR.".to_string());
    }

    lines.join("\n")
}

pub(crate) fn pr_merge(args: &PrMergeArgs) -> Result<()> {
    gh::check_gh().context("GitHub CLI check failed")?;

    let repo = crate::open_repo()?;
    let (upstream_name, branches_with_upstream) = discover_stack_branches_with_upstream(&repo)?;

    if branches_with_upstream.is_empty() {
        println!("No branches with a remote upstream in stack.");
        println!("Run `kin push` first to set upstreams.");
        return Ok(());
    }

    let all_stack_prs = collect_open_stack_prs(&branches_with_upstream)?;
    if all_stack_prs.is_empty() {
        println!("No open PRs found in the current stack.");
        return Ok(());
    }

    let selected = select_stack_pr(&all_stack_prs, "Select PR to merge:")?;
    let (owner, repo_name) =
        parse_github_owner_repo_from_pr_url(&selected.pr.url).ok_or_else(|| {
            anyhow!(
                "Could not parse owner/repo from PR URL: {}",
                selected.pr.url
            )
        })?;
    let status = gh::get_pr_status(&owner, &repo_name, selected.pr.number)?;
    let assessment = assess_pr_mergeability(&status);

    if assessment.unresolved_comments == 0
        && assessment.outstanding_reviews.is_empty()
        && assessment.running_checks.is_empty()
        && assessment.failed_checks.is_empty()
        && assessment.repo_allows_merge
    {
        println!(
            "Merging PR #{} for {} ({})",
            selected.pr.number, selected.branch_name, selected.pr.url
        );
        return merge_and_cascade(
            &repo,
            args,
            &upstream_name,
            &branches_with_upstream,
            &all_stack_prs,
            selected,
            status.head_ref_oid.as_deref(),
        );
    }

    println!(
        "{}",
        render_pr_merge_summary(&selected.branch_name, &selected.pr, &assessment)
    );

    if assessment.repo_allows_merge {
        let confirmed = crate::commands::prompt_confirm(
            "Merge anyway despite outstanding reviews/checks?",
            crate::commands::Fallback::Default(false),
        )?;
        if confirmed {
            return merge_and_cascade(
                &repo,
                args,
                &upstream_name,
                &branches_with_upstream,
                &all_stack_prs,
                selected,
                status.head_ref_oid.as_deref(),
            );
        }

        return Err(anyhow!(
            "Merge cancelled: outstanding reviews or checks remain for PR #{}",
            selected.pr.number
        ));
    }

    let reason = assessment
        .repo_block_reason
        .unwrap_or_else(|| "repository rules or GitHub merge state block merging".to_string());
    Err(anyhow!(
        "Merge prevented for PR #{}: {}",
        selected.pr.number,
        reason
    ))
}

/// Merge the selected PR on GitHub — retargeting dependent child PR bases as
/// part of that step, which always happens on a successful merge — then, unless
/// `--no-cascade`, run the local cascade: restack the children onto the resolved
/// trunk and delete the merged branch locally and on the remote.
fn merge_and_cascade(
    repo: &Repository,
    args: &PrMergeArgs,
    upstream_name: &str,
    branches_with_upstream: &[(StackBranch, String)],
    all_stack_prs: &[StackPr],
    selected: &StackPr,
    head_ref_oid: Option<&str>,
) -> Result<()> {
    let pr_number = selected.pr.number;
    let merged_branch_name = selected.branch_name.as_str();

    match merge_pr_and_retarget_children(
        repo,
        args,
        upstream_name,
        branches_with_upstream,
        all_stack_prs,
        selected,
        head_ref_oid,
    )? {
        MergeOutcome::Pending(state) => {
            println!(
                "Merge requested for PR #{pr_number}; current GitHub state is {state}. Child PR bases and local branches were left unchanged."
            );
            return Ok(());
        }
        MergeOutcome::Merged => println!("✓ Merged PR #{pr_number}"),
    }

    if args.no_cascade {
        println!(
            "Skipping local cascade (--no-cascade). Run `kin sync` to restack children onto {upstream_name}."
        );
        return Ok(());
    }

    // Restack the remaining stack onto the freshly-updated trunk. `sync` fetches
    // trunk, detects the (squash-)merged branch, rebases children with
    // `--update-refs`, and deletes the merged branch locally with a recoverable
    // SHA (undoable via `kin undo`). It acquires its own repo lock and oplog
    // entry, so `pr_merge` must not hold either here.
    println!("Restacking children onto {upstream_name}...");
    let sync_result = sync(&SyncArgs {
        no_delete: args.no_delete,
        ..SyncArgs::default()
    })
    .with_context(|| format!("merged PR #{pr_number}, but the local restack (`kin sync`) failed"));

    // Clean up the remote branch even if the restack failed. The PR is already
    // merged on GitHub, so if the remote branch isn't deleted now it's orphaned:
    // the PR is no longer open, so re-running `kin pr merge` can't reach this
    // path again. Run it *after* sync (which may rewrite/delete the local
    // branch), then surface any sync error so the user can `kin continue`.
    if !args.no_delete {
        delete_merged_remote_branch(repo, branches_with_upstream, merged_branch_name);
    }

    sync_result?;
    Ok(())
}

/// Delete the merged branch on its remote. Best-effort: a failure here is
/// warned about but does not fail the command, since the merge and local
/// restack have already succeeded.
fn delete_merged_remote_branch(
    repo: &Repository,
    branches_with_upstream: &[(StackBranch, String)],
    merged_branch_name: &str,
) {
    let Some((_, upstream)) = branches_with_upstream
        .iter()
        .find(|(sb, _)| sb.name == merged_branch_name)
    else {
        return;
    };

    let Some((remote, remote_branch)) = parse_remote_and_branch(upstream) else {
        eprintln!(
            "Warning: could not parse remote from upstream '{upstream}'; skipping remote branch delete."
        );
        return;
    };

    // The upstream is only a reliable source for "this branch's remote branch" when
    // it actually names that branch. A branch created off a base branch's remote ref
    // tracks that base (git's default `branch.autoSetupMerge=true`), and deleting
    // `remote_branch` would then delete the base branch on the remote. Skip rather
    // than fail: the merge already succeeded, and the base is not ours to remove.
    // Same root cause as the push guard in `commands::push`.
    //
    // A failure to resolve the protected set must also skip: a safety check that
    // cannot run has to block the destructive action, not wave it through.
    let protected = match crate::commands::protected_push_targets(repo) {
        Ok(protected) => protected,
        Err(err) => {
            eprintln!(
                "Warning: not deleting remote branch '{remote}/{remote_branch}': could not \
                 determine the repository's base branches ({err})."
            );
            return;
        }
    };
    if let Some(base) =
        crate::commands::foreign_base_target(merged_branch_name, remote_branch, &protected)
    {
        eprintln!(
            "Warning: not deleting remote branch '{remote}/{remote_branch}': branch \
             '{merged_branch_name}' tracks the base branch '{base}', so this would delete it on \
             the remote. Repoint it with 'git branch --unset-upstream {merged_branch_name}'."
        );
        return;
    }

    match gh::delete_remote_branch(remote, remote_branch) {
        Ok(()) => println!("✓ Deleted remote branch {upstream}"),
        Err(err) => eprintln!("Warning: failed to delete remote branch {upstream}: {err}"),
    }
}

fn merge_pr_and_retarget_children(
    repo: &Repository,
    args: &PrMergeArgs,
    upstream_name: &str,
    branches_with_upstream: &[(StackBranch, String)],
    all_stack_prs: &[StackPr],
    selected: &StackPr,
    head_ref_oid: Option<&str>,
) -> Result<MergeOutcome> {
    let pr_number = selected.pr.number;
    let merged_branch_name = selected.branch_name.as_str();
    gh::merge_pr(pr_number, head_ref_oid, args.method.map(|m| m.gh_flag()))?;
    // The merge request was accepted; if reading the resulting state fails, make
    // clear the merge already happened and how to finish, rather than surfacing a
    // bare error that hides it and leaves the stack half-cascaded.
    let pr_state = gh::get_pr_state(pr_number).with_context(|| {
        format!(
            "PR #{pr_number} was merged on GitHub, but reading its state to continue the cascade \
             failed. If it merged, run `kin sync` to restack the remaining stack onto {upstream_name}."
        )
    })?;
    if !pr_state.eq_ignore_ascii_case("MERGED") {
        return Ok(MergeOutcome::Pending(pr_state));
    }

    if let Err(err) = retarget_child_pr_bases(
        repo,
        upstream_name,
        branches_with_upstream,
        all_stack_prs,
        merged_branch_name,
    ) {
        eprintln!(
            "Warning: merged PR #{pr_number}, but failed to retarget dependent PR bases: {err}"
        );
    }

    Ok(MergeOutcome::Merged)
}

fn retarget_child_pr_bases(
    repo: &Repository,
    upstream_name: &str,
    branches_with_upstream: &[(StackBranch, String)],
    all_stack_prs: &[StackPr],
    merged_branch_name: &str,
) -> Result<()> {
    let base_map = compute_base_map(repo, branches_with_upstream, upstream_name)?;
    let new_base = base_map
        .get(merged_branch_name)
        .map(|base| normalize_base_for_gh(base))
        .unwrap_or_else(|| normalize_base_for_gh(upstream_name));

    for child_pr in all_stack_prs.iter().filter(|pr| {
        base_map
            .get(&pr.branch_name)
            .is_some_and(|base| base == merged_branch_name)
    }) {
        println!(
            "Retargeting dependent PR #{} for {} to base '{}'",
            child_pr.pr.number, child_pr.branch_name, new_base
        );
        gh::update_pr_base(child_pr.pr.number, &new_base)?;
        println!("✓ Retargeted PR #{}", child_pr.pr.number);
    }

    Ok(())
}

/// Split an upstream shorthand (`origin/feature`, `origin/team/feature`) into
/// its remote and remote-branch name. The remote is always the first path
/// component; the branch may itself contain slashes.
fn parse_remote_and_branch(upstream: &str) -> Option<(&str, &str)> {
    match upstream.split_once('/') {
        Some((remote, branch)) if !remote.is_empty() && !branch.is_empty() => {
            Some((remote, branch))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::pr::MergeMethod;

    #[test]
    fn merge_method_maps_to_gh_flag() {
        assert_eq!(MergeMethod::Squash.gh_flag(), "--squash");
        assert_eq!(MergeMethod::Rebase.gh_flag(), "--rebase");
        assert_eq!(MergeMethod::Merge.gh_flag(), "--merge");
    }

    #[test]
    fn parse_remote_and_branch_splits_on_first_slash() {
        assert_eq!(
            parse_remote_and_branch("origin/feature"),
            Some(("origin", "feature"))
        );
        // Branch names may contain slashes; the remote is only the first segment.
        assert_eq!(
            parse_remote_and_branch("origin/team/feature"),
            Some(("origin", "team/feature"))
        );
        assert_eq!(
            parse_remote_and_branch("upstream/main"),
            Some(("upstream", "main"))
        );
    }

    #[test]
    fn parse_remote_and_branch_rejects_malformed() {
        assert_eq!(parse_remote_and_branch("feature"), None);
        assert_eq!(parse_remote_and_branch(""), None);
        assert_eq!(parse_remote_and_branch("origin/"), None);
        assert_eq!(parse_remote_and_branch("/feature"), None);
    }
}
