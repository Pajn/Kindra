use crate::commands::pr_merge::pr_merge;
use crate::gh::{self, CreatePrParams};
use crate::stack::{
    StackBranch, compute_base_map, get_stack_branches, sort_branches_topologically,
};
use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use clap::{Args, Subcommand};
use git2::{BranchType, Repository};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use tempfile::NamedTempFile;

#[derive(Subcommand, Clone)]
pub enum PrSubcommand {
    /// Open an existing PR from the current stack in your default browser
    Open,
    /// Edit an existing PR from the current stack
    Edit,
    /// Merge an open PR from the current stack
    Merge,
    /// Show status summary for all open PRs in the current stack
    Status,
    /// Fetch and render review comments for an open PR in the current stack
    Review(PrReviewArgs),
}

#[derive(Args, Clone, Debug)]
pub struct PrReviewArgs {
    /// Write the rendered markdown to a file
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,
    /// Copy the rendered markdown to the terminal clipboard via OSC 52
    #[arg(long)]
    copy: bool,
    /// Exclude outdated review threads
    #[arg(long)]
    no_outdated: bool,
    /// Include resolved review threads
    #[arg(long)]
    resolved: bool,
    /// Only include comments from a specific reviewer/login
    #[arg(long)]
    reviewer: Option<String>,
    /// Include bot comments/replies
    #[arg(long, conflicts_with = "no_bots")]
    bots: bool,
    /// Exclude bot comments/replies
    #[arg(long = "no-bots", conflicts_with = "bots")]
    no_bots: bool,
}

impl PrReviewArgs {
    fn include_bots(&self) -> bool {
        !self.no_bots
    }
}

pub fn pr(subcommand: &Option<PrSubcommand>) -> Result<()> {
    match subcommand {
        Some(PrSubcommand::Open) => pr_open(),
        Some(PrSubcommand::Edit) => pr_edit(),
        Some(PrSubcommand::Merge) => pr_merge(),
        Some(PrSubcommand::Status) => pr_status(),
        Some(PrSubcommand::Review(args)) => pr_review(args),
        None => pr_create_or_update(),
    }
}

const STACK_SECTION_START: &str = "<!-- gits-stack:start -->";
const STACK_SECTION_END: &str = "<!-- gits-stack:end -->";

#[derive(Clone)]
pub(crate) struct StackPr {
    pub(crate) branch_name: String,
    pub(crate) pr: gh::EditablePr,
}

#[derive(Debug, Clone, PartialEq)]
struct StoredPr {
    branch_name: String,
    url: String,
    number: u64,
}

#[derive(Debug, Clone)]
struct RenderItem {
    branch_name: String,
    url: String,
    number: u64,
    is_current: bool,
    is_merged: bool,
}

fn pr_create_or_update() -> Result<()> {
    gh::check_gh().context("GitHub CLI check failed")?;

    let repo = crate::open_repo()?;
    let (upstream_name, branches_with_upstream) = discover_stack_branches_with_upstream(&repo)?;

    if branches_with_upstream.is_empty() {
        println!("No branches with a remote upstream to create PRs for.");
        println!("Run `gits push` first to set upstreams.");
        return Ok(());
    }

    // Determine the correct base for each branch.
    // The base is the branch directly beneath it in the stack (or the repo
    // upstream if it sits directly on top of main/master).
    let base_map = compute_base_map(&repo, &branches_with_upstream, &upstream_name)?;

    println!(
        "Found {} branch(es) with upstreams. Processing PRs...\n",
        branches_with_upstream.len()
    );

    let mut open_prs = Vec::new();
    for (sb, _remote_upstream) in &branches_with_upstream {
        let git_base = base_map
            .get(&sb.name)
            .cloned()
            .unwrap_or_else(|| upstream_name.clone());
        let gh_base = normalize_base_for_gh(&git_base);

        if let Some(pr) = process_branch_pr(&repo, &sb.name, &git_base, &gh_base)? {
            open_prs.push(StackPr {
                branch_name: sb.name.clone(),
                pr,
            });
        }
        println!();
    }

    // Now that we have all active PRs, update descriptions to include the full stack
    // (including merged ones parsed from existing descriptions).
    sync_stack_descriptions(&open_prs)?;

    Ok(())
}

fn pr_open() -> Result<()> {
    gh::check_gh().context("GitHub CLI check failed")?;

    let repo = crate::open_repo()?;
    let (_upstream_name, branches_with_upstream) = discover_stack_branches_with_upstream(&repo)?;

    if branches_with_upstream.is_empty() {
        println!("No branches with a remote upstream in stack.");
        println!("Run `gits push` first to set upstreams.");
        return Ok(());
    }

    let mut prs: Vec<(String, String)> = Vec::new();
    for (sb, _remote_upstream) in &branches_with_upstream {
        if let Some(open_pr) = gh::find_open_pr_url(&sb.name)? {
            prs.push((sb.name.clone(), open_pr.url));
        }
    }

    if prs.is_empty() {
        println!("No open PRs found in the current stack.");
        return Ok(());
    }

    if prs.len() == 1 {
        let (branch, url) = &prs[0];
        println!("Opening PR for {}: {}", branch, url);
        gh::open_url(url)?;
        return Ok(());
    }

    let options: Vec<String> = prs
        .iter()
        .map(|(branch, url)| format!("{} → {}", branch, url))
        .collect();

    let selection = crate::commands::prompt_select("Select PR to open:", options)?;
    let selected_url = prs
        .iter()
        .find(|(branch, url)| format!("{} → {}", branch, url) == selection)
        .map(|(_, url)| url)
        .ok_or_else(|| anyhow::anyhow!("Selected PR not found"))?;

    println!("Opening {}", selected_url);
    gh::open_url(selected_url)?;
    Ok(())
}

fn pr_edit() -> Result<()> {
    gh::check_gh().context("GitHub CLI check failed")?;

    let repo = crate::open_repo()?;
    let (_upstream_name, branches_with_upstream) = discover_stack_branches_with_upstream(&repo)?;

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

    let selected = select_stack_pr(&all_stack_prs, "Select PR to edit:")?;
    let branch_name = selected.branch_name.clone();
    let existing = selected.pr.clone();

    println!(
        "Editing PR #{} for {} ({})",
        existing.number, branch_name, existing.url
    );

    let title = prompt_edit_title(&existing.title)?;
    let mut body = prompt_edit_body(&existing.body)?;
    let mut labels = existing.labels.clone();
    let mut reviewers = existing.reviewers.clone();

    loop {
        let mut menu_items = vec!["Save".to_string()];
        if labels.is_empty() {
            menu_items.push("Set labels".to_string());
        } else {
            menu_items.push(format!("Set labels [{}]", labels.join(", ")));
        }
        if reviewers.is_empty() {
            menu_items.push("Set reviewers".to_string());
        } else {
            menu_items.push(format!("Set reviewers [{}]", reviewers.join(", ")));
        }
        menu_items.push("Edit body".to_string());

        let choice = crate::commands::prompt_select("PR edit options:", menu_items)?;
        match choice.as_str() {
            "Save" => break,
            s if s.starts_with("Set labels") => {
                labels = prompt_labels_for_edit(&labels)?;
            }
            s if s.starts_with("Set reviewers") => {
                reviewers = prompt_reviewers_for_edit(&reviewers)?;
            }
            "Edit body" => {
                body = prompt_edit_body(&existing.body)?;
            }
            _ => {}
        }
    }

    let body_for_reconciliation = body.as_deref().unwrap_or(&existing.body);
    let old_list = parse_stack_section(body_for_reconciliation);
    let merged_list = merge_stack_lists(&old_list, &all_stack_prs, &branch_name)?;
    let stack_section = render_stack_section(&merged_list);
    let final_body = update_stack_section(body_for_reconciliation, stack_section);

    let body_to_send = if final_body == existing.body && body.is_none() {
        None
    } else {
        Some(final_body)
    };

    gh::edit_pr(&gh::EditPrParams {
        number: existing.number,
        title,
        body: body_to_send,
        current_labels: existing.labels.clone(),
        labels,
        current_reviewers: existing.reviewers.clone(),
        reviewers,
    })?;
    println!("✓ PR updated: {}", existing.url);
    Ok(())
}

fn pr_review(args: &PrReviewArgs) -> Result<()> {
    gh::check_gh().context("GitHub CLI check failed")?;

    let repo = crate::open_repo()?;
    let (_upstream_name, branches_with_upstream) = discover_stack_branches_with_upstream(&repo)?;

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

    let selected = select_stack_pr(&all_stack_prs, "Select PR to review:")?;
    let (owner, repo_name) =
        parse_github_owner_repo_from_pr_url(&selected.pr.url).ok_or_else(|| {
            anyhow!(
                "Could not parse owner/repo from PR URL: {}",
                selected.pr.url
            )
        })?;
    let threads = gh::get_pr_review_threads(&owner, &repo_name, selected.pr.number)?;
    let markdown = render_review_markdown(threads, args);

    println!("{markdown}");

    if let Some(path) = &args.output {
        fs::write(path, &markdown).with_context(|| {
            format!(
                "Failed to write rendered review markdown to {}",
                path.display()
            )
        })?;
        eprintln!("Saved review markdown to {}", path.display());
    }

    if args.copy {
        copy_via_osc52(&markdown)?;
        eprintln!("Copied review markdown to clipboard");
    }

    Ok(())
}

fn pr_status() -> Result<()> {
    gh::check_gh().context("GitHub CLI check failed")?;

    let repo = crate::open_repo()?;
    let (_upstream_name, branches_with_upstream) = discover_stack_branches_with_upstream(&repo)?;

    if branches_with_upstream.is_empty() {
        println!("No branches with a remote upstream in stack.");
        println!("Run `gits push` first to set upstreams.");
        return Ok(());
    }

    let mut prs: Vec<(String, gh::EditablePr)> = Vec::new();
    for (sb, _remote_upstream) in &branches_with_upstream {
        if let Some(pr) = gh::find_open_pr_for_edit(&sb.name)? {
            prs.push((sb.name.clone(), pr));
        }
    }

    if prs.is_empty() {
        println!("No open PRs found in the current stack.");
        return Ok(());
    }

    for (idx, (branch, pr)) in prs.iter().enumerate() {
        if idx > 0 {
            println!();
        }

        let (owner, repo_name) = parse_github_owner_repo_from_pr_url(&pr.url)
            .ok_or_else(|| anyhow::anyhow!("Could not parse owner/repo from PR URL: {}", pr.url))?;
        let status = gh::get_pr_status(&owner, &repo_name, pr.number)?;

        println!("── {} (#{}): {} ──", branch, pr.number, pr.title);
        println!("URL: {}", pr.url);

        if status.reviewer_statuses.is_empty() {
            println!("Reviewers: none");
        } else {
            println!("Reviewers:");
            for reviewer in &status.reviewer_statuses {
                println!("  - {}: {}", reviewer.reviewer, reviewer.status);
            }
        }

        println!("Unresolved comments: {}", status.unresolved_comments);

        if status.running_checks.is_empty() {
            println!("Running checks: none");
        } else {
            println!("Running checks: {}", status.running_checks.join(", "));
        }

        if status.failed_checks.is_empty() {
            println!("Failed checks: none");
        } else {
            println!("Failed checks: {}", status.failed_checks.join(", "));
        }
    }

    Ok(())
}

pub(crate) fn parse_github_owner_repo_from_pr_url(url: &str) -> Option<(String, String)> {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let mut parts = after_scheme.split('/');

    let _host = parts.next()?;
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    Some((owner, repo))
}

pub(crate) fn collect_open_stack_prs(
    branches_with_upstream: &[(StackBranch, String)],
) -> Result<Vec<StackPr>> {
    let mut stack_prs = Vec::new();
    for (sb, _remote_upstream) in branches_with_upstream {
        if let Some(pr) = gh::find_open_pr_for_edit(&sb.name)? {
            stack_prs.push(StackPr {
                branch_name: sb.name.clone(),
                pr,
            });
        }
    }

    Ok(stack_prs)
}

pub(crate) fn select_stack_pr<'a>(prs: &'a [StackPr], prompt: &str) -> Result<&'a StackPr> {
    if prs.len() == 1 {
        return Ok(&prs[0]);
    }

    let options: Vec<String> = prs.iter().map(format_stack_pr_option).collect();
    let selection = crate::commands::prompt_select(prompt, options)?;
    prs.iter()
        .find(|pr| format_stack_pr_option(pr) == selection)
        .ok_or_else(|| anyhow!("Selected PR not found"))
}

fn format_stack_pr_option(pr: &StackPr) -> String {
    format!("{} → {}", pr.branch_name, pr.pr.url)
}

pub(crate) fn discover_stack_branches_with_upstream(
    repo: &Repository,
) -> Result<(String, Vec<(StackBranch, String)>)> {
    let upstream_name = crate::commands::find_upstream(repo)?.ok_or_else(|| {
        anyhow!("Could not find a base branch (init.defaultBranch, main, master, or trunk)")
    })?;

    // Resolve the stack boundary (git ref for stack detection) and PR base
    let (git_boundary_ref, _logical_pr_base) =
        resolve_stack_boundary_and_base(repo, &upstream_name)?;

    let upstream_obj = repo.revparse_single(&git_boundary_ref)?;
    let upstream_id = upstream_obj.id();
    let head_id = repo.head()?.peel_to_commit()?.id();

    // Collect all stack branches and sort bottom→top so base branches are
    // processed before the branches that depend on them.
    let mut stack_branches = get_stack_branches(repo, head_id, upstream_id, &git_boundary_ref)?;
    sort_branches_topologically(repo, &mut stack_branches)?;

    if stack_branches.is_empty() {
        println!("No branches in stack.");
        return Ok((git_boundary_ref, Vec::new()));
    }

    // Only operate on branches that have a remote upstream configured.
    let branches_with_upstream: Vec<(StackBranch, String)> = stack_branches
        .into_iter()
        .filter_map(|sb| {
            let branch = repo.find_branch(&sb.name, BranchType::Local).ok()?;
            let up = branch.upstream().ok()?;
            let up_name = up.name().ok()??.to_string();
            Some((sb, up_name))
        })
        .collect();

    Ok((git_boundary_ref, branches_with_upstream))
}

fn render_review_markdown(threads: Vec<gh::PrReviewThread>, args: &PrReviewArgs) -> String {
    let filtered_threads: Vec<gh::PrReviewThread> = threads
        .into_iter()
        .filter(|thread| args.resolved || !thread.is_resolved)
        .filter_map(|thread| {
            let root = thread.comments.first()?;
            if args.no_outdated && root.outdated {
                return None;
            }
            if !review_comment_matches_filters(root, args) {
                return None;
            }

            let mut comments = Vec::with_capacity(thread.comments.len());
            comments.push(root.clone());
            comments.extend(
                thread
                    .comments
                    .iter()
                    .skip(1)
                    .filter(|comment| review_comment_matches_filters(comment, args))
                    .cloned(),
            );

            Some(gh::PrReviewThread {
                is_resolved: thread.is_resolved,
                comments,
            })
        })
        .collect();

    if filtered_threads.is_empty() {
        return "_No matching review comments found._".to_string();
    }

    filtered_threads
        .iter()
        .map(render_review_thread)
        .collect::<Vec<_>>()
        .join("\n\n\n")
}

fn render_review_thread(thread: &gh::PrReviewThread) -> String {
    let root = &thread.comments[0];
    let mut sections = vec![format!(
        "{}\n{}",
        render_review_heading(root, thread.is_resolved),
        sanitize_review_body(&root.body)
    )];

    for reply in thread.comments.iter().skip(1) {
        sections.push(format!(
            "**Reply from @{}**\n{}",
            reply.author.login,
            sanitize_review_body(&reply.body)
        ));
    }

    sections.join("\n\n")
}

fn render_review_heading(comment: &gh::PrReviewComment, is_resolved: bool) -> String {
    let mut labels = Vec::new();
    if is_resolved {
        labels.push("RESOLVED".to_string());
    }
    if comment.outdated {
        let original_line = comment
            .original_start_line
            .or(comment.original_line)
            .map_or_else(|| "unknown".to_string(), |line| line.to_string());
        labels.push(format!("OUTDATED, original comment line: {original_line}"));
    }

    let mut heading = format!(
        "### `{}` — @{}",
        render_review_location(comment),
        comment.author.login
    );
    if !labels.is_empty() {
        heading.push_str(" [");
        heading.push_str(&labels.join(" | "));
        heading.push(']');
    }
    heading
}

fn render_review_location(comment: &gh::PrReviewComment) -> String {
    if comment.outdated {
        return comment.path.clone();
    }

    match (comment.start_line.or(comment.line), comment.line) {
        (Some(start), Some(end)) if start != end => format!("{}:{}-{}", comment.path, start, end),
        (_, Some(line)) => format!("{}:{}", comment.path, line),
        (Some(start), None) => format!("{}:{}", comment.path, start),
        (None, None) => comment.path.clone(),
    }
}

fn review_comment_matches_filters(comment: &gh::PrReviewComment, args: &PrReviewArgs) -> bool {
    if !args.include_bots() && comment.author.is_bot {
        return false;
    }

    if let Some(reviewer) = &args.reviewer {
        return comment.author.login.eq_ignore_ascii_case(reviewer);
    }

    true
}

fn copy_via_osc52(text: &str) -> Result<()> {
    let encoded = STANDARD.encode(text.as_bytes());
    let mut stderr = std::io::stderr();
    write!(stderr, "\x1b]52;c;{encoded}\x07").context("Failed to write OSC 52 sequence")?;
    stderr.flush().context("Failed to flush OSC 52 sequence")?;
    Ok(())
}

fn sanitize_review_body(text: &str) -> String {
    trim_trailing_newlines(&strip_html_comments(text)).to_string()
}

fn strip_html_comments(text: &str) -> String {
    let mut remaining = text;
    let mut cleaned = String::new();

    loop {
        if let Some(start) = remaining.find("<!--") {
            cleaned.push_str(&remaining[..start]);
            let after_start = &remaining[start + 4..];
            if let Some(end) = after_start.find("-->") {
                remaining = &after_start[end + 3..];
            } else {
                break;
            }
        } else {
            cleaned.push_str(remaining);
            break;
        }
    }

    cleaned
}

fn trim_trailing_newlines(text: &str) -> &str {
    text.trim_end_matches('\n')
}

/// Resolves the stack boundary reference and the logical PR base for a given upstream.
///
/// Returns `(git_boundary_ref, logical_pr_base)` where:
/// - `git_boundary_ref` is the Git reference to use for stack boundary detection
/// - `logical_pr_base` is the base branch name to use for GitHub PRs (normalized)
///
/// This function implements the same fallback behavior as `resolve_sync_onto`:
/// - First tries to use the remote tracking branch if the local branch has diverged
/// - Falls back to `origin/{upstream}` if no local tracking exists
/// - Falls back to `{only_remote}/{upstream}` if only one remote exists
pub fn resolve_stack_boundary_and_base(
    repo: &Repository,
    upstream_name: &str,
) -> Result<(String, String)> {
    // First try: check if local branch has a remote tracking branch that diverged
    if let Ok(branch) = repo.find_branch(upstream_name, BranchType::Local)
        && let Ok(upstream_branch) = branch.upstream()
        && let Ok(Some(upstream_ref)) = upstream_branch.name()
    {
        let upstream_ref_str = upstream_ref.to_string();
        let local_id = repo.revparse_single(upstream_name)?.id();
        let remote_id = repo.revparse_single(upstream_ref)?.id();
        if local_id != remote_id {
            // Local is behind or diverged from remote - use remote tracking branch
            let gh_base = normalize_base_for_gh(&upstream_ref_str);
            return Ok((upstream_ref_str, gh_base));
        }
    }

    // Fallback behavior from resolve_sync_onto
    let remotes = repo.remotes()?;
    let remote_names: Vec<String> = remotes.iter().flatten().map(|s| s.to_string()).collect();

    // Check if upstream_name has a remote prefix (e.g., "origin/main")
    if let Some((prefix, _)) = upstream_name.split_once('/')
        && remote_names.iter().any(|remote| remote == prefix)
    {
        let gh_base = normalize_base_for_gh(upstream_name);
        return Ok((upstream_name.to_string(), gh_base));
    }

    // Try origin/{upstream}
    let origin_candidate = format!("origin/{upstream_name}");
    if repo.revparse_single(&origin_candidate).is_ok() {
        let gh_base = normalize_base_for_gh(&origin_candidate);
        return Ok((origin_candidate, gh_base));
    }

    // Try {only_remote}/{upstream} if only one remote
    if remote_names.len() == 1 {
        let only_remote_candidate = format!("{}/{}", remote_names[0], upstream_name);
        if repo.revparse_single(&only_remote_candidate).is_ok() {
            let gh_base = normalize_base_for_gh(&only_remote_candidate);
            return Ok((only_remote_candidate, gh_base));
        }
    }

    // Fallback: use upstream_name as-is
    let gh_base = normalize_base_for_gh(upstream_name);
    Ok((upstream_name.to_string(), gh_base))
}

// ────────────────────────────────────────────────────────────────────────────
// Per-branch PR logic
// ────────────────────────────────────────────────────────────────────────────

pub(crate) fn normalize_base_for_gh(base: &str) -> String {
    if let Some((first, rest)) = base.split_once('/')
        && (first == "origin" || first == "upstream")
    {
        return rest.to_string();
    }
    base.to_string()
}

fn process_branch_pr(
    repo: &Repository,
    branch_name: &str,
    git_base: &str,
    gh_base: &str,
) -> Result<Option<gh::EditablePr>> {
    println!("── {} ──", branch_name);

    // Check for an existing open PR
    match gh::find_open_pr(branch_name)? {
        Some(existing) => {
            println!("  Open PR #{} found.", existing.number);
            if existing.base_branch != gh_base {
                println!("  Updating base: {} → {}", existing.base_branch, gh_base);
                gh::update_pr_base(existing.number, gh_base)?;
                println!("  ✓ Base updated.");
            } else {
                println!("  Base is already '{}'. Nothing to update.", gh_base);
            }
            let pr = gh::find_open_pr_for_edit(branch_name)?.ok_or_else(|| {
                anyhow!(
                    "Expected editable PR details for branch '{}' after finding PR #{}.",
                    branch_name,
                    existing.number
                )
            })?;
            Ok(Some(pr))
        }
        None => {
            // New PR: run the interactive wizard
            create_pr_interactive(repo, branch_name, git_base, gh_base)
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Interactive PR creation wizard
// ────────────────────────────────────────────────────────────────────────────

fn create_pr_interactive(
    repo: &Repository,
    branch_name: &str,
    git_base: &str,
    gh_base: &str,
) -> Result<Option<gh::EditablePr>> {
    let commits = get_branch_commits(repo, branch_name, git_base)?;

    if commits.is_empty() {
        println!(
            "No commits on this branch compared to '{}'. Skipping.",
            git_base
        );
        return Ok(None);
    }

    // ── Step 1: Title ────────────────────────────────────────────────────────
    let title = prompt_title(&commits)?;
    if title.is_empty() {
        println!("  PR title is empty. Skipping {}.", branch_name);
        return Ok(None);
    }

    // ── Step 2: Body ─────────────────────────────────────────────────────────
    let body = prompt_body(branch_name, &commits)?;

    // ── Step 3: Submit options ───────────────────────────────────────────────
    let submission = prompt_submit_options()?;
    let labels = submission.labels.clone();
    let reviewers = submission.reviewers.clone();

    println!("  Creating PR...");
    let url = gh::create_pr(&CreatePrParams {
        title: title.clone(),
        body: body.clone(),
        base: gh_base.to_string(),
        head: branch_name.to_string(),
        draft: submission.draft,
        labels: submission.labels,
        reviewers: submission.reviewers,
    })?;

    println!("  ✓ PR created: {}", url);
    Ok(Some(gh::EditablePr {
        number: parse_pr_number_from_url(&url)?,
        title,
        body,
        url,
        labels,
        reviewers,
    }))
}

fn sync_stack_descriptions(prs: &[StackPr]) -> Result<()> {
    for pr in prs {
        let old_list = parse_stack_section(&pr.pr.body);
        let merged_list = merge_stack_lists(&old_list, prs, &pr.branch_name)?;
        let stack_section = render_stack_section(&merged_list);
        let updated_body = update_stack_section(&pr.pr.body, stack_section);

        if updated_body == pr.pr.body {
            continue;
        }

        println!("Syncing stack section for PR #{}.", pr.pr.number);
        if let Err(err) = gh::edit_pr(&gh::EditPrParams {
            number: pr.pr.number,
            title: pr.pr.title.clone(),
            body: Some(updated_body),
            current_labels: pr.pr.labels.clone(),
            labels: pr.pr.labels.clone(),
            current_reviewers: pr.pr.reviewers.clone(),
            reviewers: pr.pr.reviewers.clone(),
        }) {
            eprintln!(
                "Failed to sync stack section for PR #{}: {}",
                pr.pr.number, err
            );
        }
    }

    Ok(())
}

fn parse_stack_section(body: &str) -> Vec<StoredPr> {
    let mut prs = Vec::new();
    let start_idx = body.find(STACK_SECTION_START);
    let end_idx = body.find(STACK_SECTION_END);

    if let (Some(start), Some(end)) = (start_idx, end_idx)
        && start < end
    {
        let section_start = start + STACK_SECTION_START.len();
        let section = &body[section_start..end];
        for line in section.lines() {
            let line = line.trim();
            if (line.starts_with("- [") || line.starts_with("- → ") || line.starts_with("- ~"))
                && let Some(pr) = parse_stack_line(line)
            {
                prs.push(pr);
            }
        }
    }
    prs
}

fn parse_stack_line(line: &str) -> Option<StoredPr> {
    // Case 1: - [branch](url) #number (possibly with ~ for strikethrough)
    // Case 2: - → branch #number
    if let Some(rest) = line.strip_prefix("- → ") {
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.len() >= 2 {
            let branch_name = parts[0].to_string();
            let number_str = parts[1].trim_start_matches('#');
            if let Ok(number) = number_str.parse::<u64>() {
                return Some(StoredPr {
                    branch_name,
                    url: String::new(), // We'll fix this during merge if needed
                    number,
                });
            }
        }
    } else if line.starts_with("- [") || line.starts_with("- ~[") {
        // Line like: - [branch](url) #number
        // or - ~[branch](url) #number~ (merged)
        let line = line
            .trim_start_matches("- ")
            .trim_start_matches('~')
            .trim_end_matches(" (merged)")
            .trim_end_matches('~');
        if let Some(branch_end) = line.find("](") {
            let branch_name = line[1..branch_end].to_string();
            let rest = &line[branch_end + 2..];
            if let Some(url_end) = rest.find(')') {
                let url = rest[..url_end].to_string();
                let after_url = rest[url_end + 1..].trim();
                if let Some(hash_idx) = after_url.find('#') {
                    let number_part = &after_url[hash_idx + 1..];
                    let number_str: String = number_part
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    if let Ok(number) = number_str.parse::<u64>() {
                        return Some(StoredPr {
                            branch_name,
                            url,
                            number,
                        });
                    }
                }
            }
        }
    }
    None
}

fn merge_stack_lists(
    old_list: &[StoredPr],
    active_prs: &[StackPr],
    current_branch: &str,
) -> Result<Vec<RenderItem>> {
    let active_by_number: HashMap<u64, &StackPr> =
        active_prs.iter().map(|pr| (pr.pr.number, pr)).collect();
    let mut merged_buckets = vec![Vec::new(); active_prs.len() + 1];
    let mut emitted_numbers = HashSet::new();
    let mut seen_old_active = HashSet::new();
    let mut old_active_rank = 0usize;

    for old in old_list {
        if active_by_number.contains_key(&old.number) {
            if seen_old_active.insert(old.number) {
                old_active_rank = (old_active_rank + 1).min(active_prs.len());
            }
            continue;
        }

        // Check state for PRs not in the active local stack.
        match gh::get_pr_state(old.number) {
            Ok(state) if state == "MERGED" && emitted_numbers.insert(old.number) => {
                merged_buckets[old_active_rank].push(RenderItem {
                    branch_name: old.branch_name.clone(),
                    url: old.url.clone(),
                    number: old.number,
                    is_current: false,
                    is_merged: true,
                });
            }
            Ok(_) => {}
            Err(err) => {
                eprintln!(
                    "Skipping inaccessible historical PR #{} while reconciling stack: {}",
                    old.number, err
                );
            }
        }
        // else: skip (it was closed but not merged, or it's now inaccessible).
    }

    let mut items = Vec::new();
    for (idx, active) in active_prs.iter().enumerate() {
        items.append(&mut merged_buckets[idx]);
        if emitted_numbers.insert(active.pr.number) {
            items.push(RenderItem {
                branch_name: active.branch_name.clone(),
                url: active.pr.url.clone(),
                number: active.pr.number,
                is_current: active.branch_name == current_branch,
                is_merged: false,
            });
            emitted_numbers.insert(active.pr.number);
        }
    }

    items.append(&mut merged_buckets[active_prs.len()]);

    Ok(items)
}

fn render_stack_section(items: &[RenderItem]) -> Option<String> {
    if items.len() <= 1 {
        return None;
    }

    let mut section = String::from(STACK_SECTION_START);
    section.push_str("\n## Stack\n");

    for item in items {
        if item.is_current {
            section.push_str(&format!("- → {} #{}\n", item.branch_name, item.number));
        } else if item.is_merged {
            section.push_str(&format!(
                "- ~[{}]({}) #{}~ (merged)\n",
                item.branch_name, item.url, item.number
            ));
        } else {
            section.push_str(&format!(
                "- [{}]({}) #{}\n",
                item.branch_name, item.url, item.number
            ));
        }
    }

    section.push_str(STACK_SECTION_END);
    Some(section)
}

fn update_stack_section(body: &str, stack_section: Option<String>) -> String {
    let (body_without_section, removed_existing_section) = remove_existing_stack_section(body);

    match stack_section {
        Some(section) => {
            let trimmed = body_without_section.trim_end();
            if trimmed.is_empty() {
                section
            } else {
                format!("{trimmed}\n\n{section}")
            }
        }
        None if removed_existing_section => body_without_section.trim_end().to_string(),
        None => body.to_string(),
    }
}

fn remove_existing_stack_section(body: &str) -> (String, bool) {
    let mut current_body = body.to_string();
    let mut any_removed = false;

    loop {
        if let Some(start_idx) = current_body.find(STACK_SECTION_START) {
            let search_after_start = start_idx + STACK_SECTION_START.len();
            if let Some(end_offset) = current_body[search_after_start..].find(STACK_SECTION_END) {
                let end_idx = search_after_start + end_offset;
                let slice_end = end_idx + STACK_SECTION_END.len();

                let before = current_body[..start_idx].trim_end();
                let after = current_body[slice_end..].trim_start();

                current_body = match (before.is_empty(), after.is_empty()) {
                    (true, true) => String::new(),
                    (false, true) => before.to_string(),
                    (true, false) => after.to_string(),
                    (false, false) => format!("{before}\n\n{after}"),
                };
                any_removed = true;
                continue;
            }
        }
        break;
    }

    (current_body, any_removed)
}

fn parse_pr_number_from_url(url: &str) -> Result<u64> {
    let path = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split_once('/')
        .map(|(_, path)| path)
        .unwrap_or("");

    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() < 2 || segments[segments.len() - 2] != "pull" {
        return Err(anyhow!(
            "Could not parse PR number from URL '{}': expected '/pull/<number>' in URL",
            url
        ));
    }

    segments[segments.len() - 1].parse().map_err(|_| {
        anyhow!(
            "Could not parse PR number from URL '{}': expected '/pull/<number>' in URL",
            url
        )
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers: commits on branch
// ────────────────────────────────────────────────────────────────────────────

pub(crate) struct CommitSummary {
    pub subject: String,
}

pub(crate) fn get_branch_commits(
    repo: &Repository,
    branch_name: &str,
    base_name: &str,
) -> Result<Vec<CommitSummary>> {
    let branch_id = repo.revparse_single(branch_name)?.peel_to_commit()?.id();
    let upstream_id = repo.revparse_single(base_name)?.peel_to_commit()?.id();

    let merge_base = repo.merge_base(upstream_id, branch_id)?;

    let mut revwalk = repo.revwalk()?;
    revwalk.push(branch_id)?;
    revwalk.hide(merge_base)?;
    revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;

    let mut commits = Vec::new();
    for oid in revwalk {
        let commit = repo.find_commit(oid?)?;
        commits.push(CommitSummary {
            subject: commit.summary().unwrap_or("").to_string(),
        });
    }

    Ok(commits)
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers: interactive prompts
// ────────────────────────────────────────────────────────────────────────────

fn prompt_title(commits: &[CommitSummary]) -> Result<String> {
    let prefill = if commits.len() == 1 {
        commits[0].subject.clone()
    } else {
        // Show commit list to help the user choose a title
        println!("  Commits on this branch:");
        for c in commits {
            println!("    • {}", c.subject);
        }
        println!();
        String::new()
    };

    if !std::io::stdin().is_terminal() {
        println!(
            "  [non-interactive] Using title: {}",
            if prefill.is_empty() {
                "(empty)"
            } else {
                &prefill
            }
        );
        return Ok(prefill);
    }

    let title = inquire::Text::new("  PR title:")
        .with_initial_value(&prefill)
        .prompt()
        .context("Title prompt failed")?;

    Ok(title)
}

fn prompt_body(branch_name: &str, commits: &[CommitSummary]) -> Result<String> {
    // Build commit list HTML comment preamble
    let mut preamble = format!("<!--\nCommits on {}:\n", branch_name);
    for c in commits {
        preamble.push_str(&format!("- {}\n", c.subject));
    }
    preamble.push_str("-->\n");

    // Try to read PR template
    let template = read_pr_template().unwrap_or_default();

    let editor_prefill = format!("{}\n{}", preamble, template);

    if !std::io::stdin().is_terminal() {
        return Ok(editor_prefill);
    }

    println!("  PR body: [e] open editor  [b] leave blank  [enter] use PR template");

    loop {
        crossterm::terminal::enable_raw_mode()?;
        let key = read_single_key();
        crossterm::terminal::disable_raw_mode()?;

        match key.as_deref() {
            Some("e") => {
                println!("e");
                return open_editor_for_body(&editor_prefill);
            }
            Some("b") => {
                println!("b");
                return Ok(String::new());
            }
            Some("\r") | Some("\n") | Some("") => {
                println!();
                // Use template (open editor prefilled with preamble + template)
                return open_editor_for_body(&editor_prefill);
            }
            _ => {
                // ignore and re-prompt
            }
        }
    }
}

fn open_editor_for_body(prefill: &str) -> Result<String> {
    let mut temp = NamedTempFile::new()?;
    temp.write_all(prefill.as_bytes())?;
    let path = temp.path().to_path_buf();

    crate::editor::launch_editor(&path)?;

    let body = fs::read_to_string(&path)?;

    // Strip the HTML comment preamble from the final body; it's only for
    // the author's reference and should not appear in the PR description.
    let cleaned = strip_html_comment(&body);
    Ok(cleaned.trim().to_string())
}

fn open_editor_for_plain_body(prefill: &str) -> Result<String> {
    let mut temp = NamedTempFile::new()?;
    temp.write_all(prefill.as_bytes())?;
    let path = temp.path().to_path_buf();

    crate::editor::launch_editor(&path)?;
    let body = fs::read_to_string(&path)?;
    Ok(body)
}

fn prompt_edit_title(current_title: &str) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        println!("  [non-interactive] Keeping title: {}", current_title);
        return Ok(current_title.to_string());
    }

    let edited = inquire::Text::new("  PR title:")
        .with_initial_value(current_title)
        .prompt()
        .context("Title prompt failed")?;

    if edited.trim().is_empty() {
        Ok(current_title.to_string())
    } else {
        Ok(edited)
    }
}

fn prompt_edit_body(current_body: &str) -> Result<Option<String>> {
    if !std::io::stdin().is_terminal() {
        println!("  [non-interactive] Keeping body unchanged");
        return Ok(None);
    }

    println!("  PR body: [e] open editor  [enter] keep unchanged");
    loop {
        crossterm::terminal::enable_raw_mode()?;
        let key = read_single_key();
        crossterm::terminal::disable_raw_mode()?;
        match key.as_deref() {
            Some("e") => {
                println!("e");
                let edited = open_editor_for_plain_body(current_body)?;
                return Ok(Some(edited));
            }
            Some("\r") | Some("\n") | Some("") => {
                println!();
                return Ok(None);
            }
            _ => {}
        }
    }
}

fn prompt_labels_for_edit(current: &[String]) -> Result<Vec<String>> {
    let available = gh::list_labels().unwrap_or_default();
    if available.is_empty() {
        println!("  No labels found in this repository.");
        return Ok(current.to_vec());
    }

    if !std::io::stdin().is_terminal() {
        println!("  [non-interactive] Keeping labels unchanged");
        return Ok(current.to_vec());
    }

    let default_indexes: Vec<usize> = available
        .iter()
        .enumerate()
        .filter(|(_, l)| current.contains(*l))
        .map(|(idx, _)| idx)
        .collect();

    let selected = inquire::MultiSelect::new(
        "  Select labels (Space to toggle, Enter to confirm):",
        available,
    )
    .with_default(&default_indexes)
    .prompt()
    .context("Label selection failed")?;
    Ok(selected)
}

fn prompt_reviewers_for_edit(current: &[String]) -> Result<Vec<String>> {
    let available = gh::list_collaborators().unwrap_or_default();
    if available.is_empty() {
        println!("  No collaborators found for this repository.");
        return Ok(current.to_vec());
    }

    if !std::io::stdin().is_terminal() {
        println!("  [non-interactive] Keeping reviewers unchanged");
        return Ok(current.to_vec());
    }

    let default_indexes: Vec<usize> = available
        .iter()
        .enumerate()
        .filter(|(_, r)| current.contains(*r))
        .map(|(idx, _)| idx)
        .collect();

    let selected = inquire::MultiSelect::new(
        "  Select reviewers (Space to toggle, Enter to confirm):",
        available,
    )
    .with_default(&default_indexes)
    .prompt()
    .context("Reviewer selection failed")?;
    Ok(selected)
}

fn strip_html_comment(s: &str) -> &str {
    // Remove everything inside <!-- ... --> at the very start of the string.
    let trimmed = s.trim_start();
    if let Some(rest) = trimmed.strip_prefix("<!--")
        && let Some(after) = rest.find("-->")
    {
        return &trimmed[4 + after + 3..]; // 4 = len("<!--"), 3 = len("-->")
    }
    s
}

fn read_pr_template() -> Option<String> {
    // Look relative to cwd – same place git would use
    let candidates = [
        ".github/pull_request_template.md",
        ".github/PULL_REQUEST_TEMPLATE.md",
        "pull_request_template.md",
        "PULL_REQUEST_TEMPLATE.md",
    ];
    for path in candidates {
        if let Ok(content) = fs::read_to_string(path) {
            return Some(content);
        }
    }
    None
}

/// Read one keypress from the terminal (raw mode must be enabled by caller).
fn read_single_key() -> Option<String> {
    use crossterm::event::{Event, KeyCode, KeyEvent, read};

    match read().ok()? {
        Event::Key(KeyEvent { code, .. }) => match code {
            KeyCode::Char(c) => Some(c.to_string()),
            KeyCode::Enter => Some("\r".to_string()),
            KeyCode::Esc => Some("esc".to_string()),
            _ => None,
        },
        _ => None,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Submit options menu
// ────────────────────────────────────────────────────────────────────────────

struct Submission {
    draft: bool,
    labels: Vec<String>,
    reviewers: Vec<String>,
}

fn prompt_submit_options() -> Result<Submission> {
    let mut labels: Vec<String> = Vec::new();
    let mut reviewers: Vec<String> = Vec::new();

    let draft = loop {
        let mut menu_items = vec!["Submit".to_string(), "Submit as draft".to_string()];

        // Show current selections in the menu labels
        if labels.is_empty() {
            menu_items.push("Set labels".to_string());
        } else {
            menu_items.push(format!("Set labels [{}]", labels.join(", ")));
        }

        if reviewers.is_empty() {
            menu_items.push("Assign reviewers".to_string());
        } else {
            menu_items.push(format!("Assign reviewers [{}]", reviewers.join(", ")));
        }

        let choice = crate::commands::prompt_select("  Ready to submit?", menu_items)?;

        match choice.as_str() {
            "Submit" => break false,
            "Submit as draft" => break true,
            s if s.starts_with("Set labels") => {
                labels = prompt_labels()?;
            }
            s if s.starts_with("Assign reviewers") => {
                reviewers = prompt_reviewers()?;
            }
            _ => {}
        }
    };

    Ok(Submission {
        draft,
        labels,
        reviewers,
    })
}

fn prompt_labels() -> Result<Vec<String>> {
    let available = gh::list_labels().unwrap_or_else(|_| Vec::new());

    if available.is_empty() {
        println!("  No labels found in this repository.");
        return Ok(Vec::new());
    }

    let selected = crate::commands::prompt_multi_select(
        "  Select labels (Space to toggle, Enter to confirm):",
        available,
    )?;
    Ok(selected)
}

fn prompt_reviewers() -> Result<Vec<String>> {
    let available = gh::list_collaborators().unwrap_or_else(|_| Vec::new());

    if available.is_empty() {
        println!("  No collaborators found for this repository.");
        return Ok(Vec::new());
    }

    let selected = crate::commands::prompt_multi_select(
        "  Select reviewers (Space to toggle, Enter to confirm):",
        available,
    )?;
    Ok(selected)
}

// ────────────────────────────────────────────────────────────────────────────
// is_terminal helper (std::io::IsTerminal is in scope via mod.rs import)
// ────────────────────────────────────────────────────────────────────────────

use std::io::IsTerminal;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_stack_line() {
        // Case: current branch
        let line = "- → feature-a #123";
        let pr = parse_stack_line(line).unwrap();
        assert_eq!(pr.branch_name, "feature-a");
        assert_eq!(pr.number, 123);
        assert_eq!(pr.url, "");

        // Case: other branch
        let line = "- [feature-b](https://github.com/u/r/pull/124) #124";
        let pr = parse_stack_line(line).unwrap();
        assert_eq!(pr.branch_name, "feature-b");
        assert_eq!(pr.number, 124);
        assert_eq!(pr.url, "https://github.com/u/r/pull/124");

        // Case: merged branch (strikethrough)
        let line = "- ~[feature-c](https://github.com/u/r/pull/122) #122~ (merged)";
        let pr = parse_stack_line(line).unwrap();
        assert_eq!(pr.branch_name, "feature-c");
        assert_eq!(pr.number, 122);
        assert_eq!(pr.url, "https://github.com/u/r/pull/122");
    }

    #[test]
    fn test_parse_stack_section() {
        let start = STACK_SECTION_START;
        let end = STACK_SECTION_END;
        let body = format!(
            "Check out my stack:\n\n{}\n## Stack\n- ~[old](url1) #111~ (merged)\n- [current](url2) #222\n- → next #333\n{}\nFooter",
            start, end
        );
        let prs = parse_stack_section(&body);
        assert_eq!(prs.len(), 3);
        assert_eq!(prs[0].branch_name, "old");
        assert_eq!(prs[1].branch_name, "current");
        assert_eq!(prs[2].branch_name, "next");
        assert_eq!(prs[0].number, 111);
        assert_eq!(prs[1].number, 222);
        assert_eq!(prs[2].number, 333);
    }

    #[test]
    fn test_normalize_base_for_gh() {
        assert_eq!(normalize_base_for_gh("main"), "main");
        assert_eq!(normalize_base_for_gh("origin/main"), "main");
        assert_eq!(normalize_base_for_gh("upstream/main"), "main");
        assert_eq!(normalize_base_for_gh("feature/base"), "feature/base");
        assert_eq!(normalize_base_for_gh("origin/feature/base"), "feature/base");
        assert_eq!(
            normalize_base_for_gh("upstream/feature/base"),
            "feature/base"
        );
    }

    #[test]
    fn test_remove_existing_stack_section() {
        let start = STACK_SECTION_START;
        let end = STACK_SECTION_END;

        // Normal case
        let body = format!("Hello\n\n{}\nSome stack info\n{}\nWorld", start, end);
        let (cleaned, removed) = remove_existing_stack_section(&body);
        assert!(removed);
        assert_eq!(cleaned, "Hello\n\nWorld");

        // Duplicate case
        let body = format!(
            "Hello\n\n{}\nSection 1\n{}\nMid\n{}\nSection 2\n{}\nWorld",
            start, end, start, end
        );
        let (cleaned, removed) = remove_existing_stack_section(&body);
        assert!(removed);
        assert_eq!(cleaned, "Hello\n\nMid\n\nWorld");

        // Malformed (missing end) - Should not remove anything if no matching END follows START
        let body = format!("Hello\n\n{}\nMissing end\nWorld", start);
        let (cleaned, removed) = remove_existing_stack_section(&body);
        assert!(!removed);
        assert_eq!(cleaned, body);

        // Malformed (end before start) - Should ignore ENDs before STARTs
        let body = format!("{}Hello\n{}\nWorld", end, start);
        let (cleaned, _removed) = remove_existing_stack_section(&body);
        assert_eq!(cleaned, body);
    }

    #[test]
    fn parse_pr_number_from_standard_github_url() {
        assert_eq!(
            parse_pr_number_from_url("https://github.com/test/repo/pull/123").unwrap(),
            123
        );
    }

    #[test]
    fn parse_pr_number_rejects_non_pull_urls() {
        let err = parse_pr_number_from_url("https://github.com/test/repo/issues/123").unwrap_err();
        assert!(err.to_string().contains("expected '/pull/<number>' in URL"));
    }

    #[test]
    fn normalize_base_keeps_branch_paths() {
        assert_eq!(normalize_base_for_gh("feature/base"), "feature/base");
    }

    #[test]
    fn normalize_base_strips_single_remote_prefix() {
        assert_eq!(normalize_base_for_gh("origin/feature/base"), "feature/base");
    }
}
