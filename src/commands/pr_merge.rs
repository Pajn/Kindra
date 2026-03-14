use crate::commands::pr::{
    StackPr, collect_open_stack_prs, discover_stack_branches_with_upstream, normalize_base_for_gh,
    parse_github_owner_repo_from_pr_url, select_stack_pr,
};
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

pub(crate) fn pr_merge() -> Result<()> {
    gh::check_gh().context("GitHub CLI check failed")?;

    let repo = crate::open_repo()?;
    let (upstream_name, branches_with_upstream) = discover_stack_branches_with_upstream(&repo)?;

    if branches_with_upstream.is_empty() {
        println!("No branches with a remote upstream in stack.");
        println!("Run `gits push` first to set upstreams.");
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
        match merge_pr_and_retarget_children(
            &repo,
            &upstream_name,
            &branches_with_upstream,
            &all_stack_prs,
            selected.pr.number,
            status.head_ref_oid.as_deref(),
            &selected.branch_name,
        )? {
            MergeOutcome::Merged => println!("✓ Merged PR #{}", selected.pr.number),
            MergeOutcome::Pending(state) => {
                println!(
                    "Merge requested for PR #{}; current GitHub state is {}. Child PR bases were left unchanged.",
                    selected.pr.number, state
                );
            }
        }
        return Ok(());
    }

    println!(
        "{}",
        render_pr_merge_summary(&selected.branch_name, &selected.pr, &assessment)
    );

    if assessment.repo_allows_merge {
        let confirmed =
            crate::commands::prompt_confirm("Merge anyway despite outstanding reviews/checks?")?;
        if confirmed {
            match merge_pr_and_retarget_children(
                &repo,
                &upstream_name,
                &branches_with_upstream,
                &all_stack_prs,
                selected.pr.number,
                status.head_ref_oid.as_deref(),
                &selected.branch_name,
            )? {
                MergeOutcome::Merged => println!("✓ Merged PR #{}", selected.pr.number),
                MergeOutcome::Pending(state) => {
                    println!(
                        "Merge requested for PR #{}; current GitHub state is {}. Child PR bases were left unchanged.",
                        selected.pr.number, state
                    );
                }
            }
            return Ok(());
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

fn merge_pr_and_retarget_children(
    repo: &Repository,
    upstream_name: &str,
    branches_with_upstream: &[(StackBranch, String)],
    all_stack_prs: &[StackPr],
    pr_number: u64,
    head_ref_oid: Option<&str>,
    merged_branch_name: &str,
) -> Result<MergeOutcome> {
    gh::merge_pr(pr_number, head_ref_oid)?;
    let pr_state = gh::get_pr_state(pr_number)?;
    if !pr_state.eq_ignore_ascii_case("MERGED") {
        return Ok(MergeOutcome::Pending(pr_state));
    }

    if let Err(err) = retarget_child_pr_bases_before_merge(
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

fn retarget_child_pr_bases_before_merge(
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
